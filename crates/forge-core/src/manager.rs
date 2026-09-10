use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::{
    adb::Adb,
    session::{ScrcpySession, SessionOptions},
    DeviceInfo, DeviceState, ForgeEvent, InputAction, PairingService,
};

#[derive(Clone)]
pub struct DeviceManager {
    adb: Adb,
    devices: Arc<RwLock<Vec<DeviceInfo>>>,
    events: broadcast::Sender<ForgeEvent>,
    sessions: Arc<RwLock<HashMap<String, Arc<ScrcpySession>>>>,
    start_locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    // A scan can involve ADB plus an mDNS browse. Serialize all callers so a
    // button click cannot start a second expensive discovery while the
    // background poll is still running.
    scan_gate: Arc<Mutex<()>>,
}

impl DeviceManager {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            adb: Adb::default(),
            devices: Arc::new(RwLock::new(Vec::new())),
            events,
            sessions: Default::default(),
            start_locks: Default::default(),
            scan_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ForgeEvent> {
        self.events.subscribe()
    }
    pub fn event_sender(&self) -> broadcast::Sender<ForgeEvent> {
        self.events.clone()
    }
    pub async fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.read().await.clone()
    }
    pub fn adb(&self) -> &Adb {
        &self.adb
    }
    pub async fn session(&self, serial: &str) -> Option<Arc<ScrcpySession>> {
        let value = self.sessions.read().await.get(serial).cloned();
        if value.as_ref().is_some_and(|s| !s.is_alive()) {
            let stale = value.as_ref().expect("checked above").clone();
            let removed = {
                let mut sessions = self.sessions.write().await;
                if sessions
                    .get(serial)
                    .is_some_and(|current| Arc::ptr_eq(current, &stale))
                {
                    sessions.remove(serial)
                } else {
                    None
                }
            };
            if let Some(session) = removed {
                let _ = session.shutdown().await;
            }
            None
        } else {
            value
        }
    }
    pub async fn sessions(&self) -> Vec<(String, Arc<ScrcpySession>)> {
        self.sessions
            .read()
            .await
            .iter()
            .filter(|(_, session)| session.is_alive())
            .map(|(serial, session)| (serial.clone(), session.clone()))
            .collect()
    }
    pub async fn start_session(
        &self,
        serial: String,
        options: SessionOptions,
    ) -> Result<Arc<ScrcpySession>> {
        let lock = self
            .start_locks
            .lock()
            .unwrap()
            .entry(serial.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if let Some(existing) = self.session(&serial).await {
            drop(_guard);
            self.prune_start_lock(&serial, &lock);
            return Ok(existing);
        }
        let result = ScrcpySession::connect(serial.clone(), options).await;
        let session = Arc::new(match result {
            Ok(session) => session,
            Err(error) => {
                drop(_guard);
                self.prune_start_lock(&serial, &lock);
                return Err(error);
            }
        });
        self.sessions.write().await.insert(serial, session.clone());
        drop(_guard);
        self.prune_start_lock(&session.serial, &lock);
        Ok(session)
    }
    pub async fn stop_session(&self, serial: &str) -> bool {
        let session = self.sessions.write().await.remove(serial);
        if let Some(session) = session {
            let _ = session.shutdown().await;
            let lock = self.start_locks.lock().unwrap().get(serial).cloned();
            if let Some(lock) = lock {
                self.prune_start_lock(serial, &lock);
            }
            true
        } else {
            false
        }
    }
    pub async fn stop_all_sessions(&self) {
        let sessions = self
            .sessions
            .write()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>();
        for session in sessions {
            let _ = session.shutdown().await;
        }
        let locks = self
            .start_locks
            .lock()
            .unwrap()
            .iter()
            .map(|(serial, lock)| (serial.clone(), lock.clone()))
            .collect::<Vec<_>>();
        for (serial, lock) in locks {
            self.prune_start_lock(&serial, &lock);
        }
    }

    pub async fn scan(&self, connect_mdns: bool) -> Result<Vec<DeviceInfo>> {
        let _scan_guard = self.scan_gate.lock().await;
        let mut devices = self.adb.devices().await?;
        if connect_mdns
            && !devices
                .iter()
                .any(|device| device.wireless && matches!(device.state, crate::DeviceState::Device))
        {
            for endpoint in self.adb.mdns_endpoints().await.unwrap_or_default() {
                let _ = self.adb.connect(&endpoint).await;
            }
            devices = self.adb.devices().await?;
        }
        // adb 自身的 mDNS 自动连接可能把同一台设备的 IPv4 和多个 IPv6
        // 地址都登记为 serial。UI 和 per-device 脚本只保留一个稳定入口。
        devices.sort_by_key(|device| (!device.wireless, device.serial.starts_with('[')));
        let mut seen_wireless = std::collections::HashSet::new();
        devices.retain(|device| {
            if !device.wireless || !matches!(device.state, crate::DeviceState::Device) {
                return true;
            }
            // transport_id is the only stable identity ADB exposes for two
            // address aliases of one transport. Never collapse devices merely
            // because they share a product/model (a lab often has many
            // identical phones); without a transport id retain the serial.
            let key = device
                .transport_id
                .as_ref()
                .map(|id| format!("transport:{id}"))
                .unwrap_or_else(|| format!("serial:{}", device.serial));
            seen_wireless.insert(key)
        });
        let changed = {
            let mut current = self.devices.write().await;
            let changed = *current != devices;
            *current = devices.clone();
            changed
        };
        if changed {
            let _ = self.events.send(ForgeEvent::DeviceSnapshot {
                devices: devices.clone(),
            });
        }
        Ok(devices)
    }

    pub async fn connect(&self, endpoint: &str) -> Result<Vec<DeviceInfo>> {
        self.adb.connect(endpoint).await?;
        self.scan(false).await
    }

    pub async fn pairing_services(&self) -> Result<Vec<PairingService>> {
        self.adb.pairing_services().await
    }

    pub async fn pair(&self, endpoint: &str, code: &str) -> Result<Vec<DeviceInfo>> {
        self.adb.pair(endpoint, code).await?;
        let host = endpoint_host(endpoint);

        // Modern adb normally connects immediately after a successful pairing.
        // Give that asynchronous registration a short window before resolving
        // the separate TLS connection service ourselves.
        for _ in 0..4 {
            let devices = self.adb.devices().await?;
            if devices.iter().any(|device| {
                device.wireless
                    && matches!(device.state, DeviceState::Device)
                    && endpoint_host(&device.serial) == host
            }) {
                return self.scan(false).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        for connect_endpoint in self.adb.mdns_endpoints().await.unwrap_or_default() {
            if endpoint_host(&connect_endpoint) == host
                && self.adb.connect(&connect_endpoint).await.is_ok()
            {
                break;
            }
        }
        self.scan(false).await
    }

    pub async fn input(&self, serial: &str, action: &InputAction) -> Result<()> {
        self.adb.input(serial, action).await
    }

    fn prune_start_lock(&self, serial: &str, lock: &Arc<tokio::sync::Mutex<()>>) {
        let mut locks = self.start_locks.lock().unwrap();
        if locks
            .get(serial)
            .is_some_and(|current| Arc::ptr_eq(current, lock) && Arc::strong_count(current) == 2)
        {
            // One reference belongs to the map and one to the caller. Waiting
            // starters keep extra references, so their lock is never removed
            // while it can still serialize a new session.
            locks.remove(serial);
        }
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

fn endpoint_host(endpoint: &str) -> String {
    if let Ok(address) = endpoint.parse::<std::net::SocketAddr>() {
        return address.ip().to_string();
    }
    if let Some(rest) = endpoint.strip_prefix('[') {
        return rest
            .split_once(']')
            .map(|(host, _)| host.to_owned())
            .unwrap_or_else(|| endpoint.to_owned());
    }
    endpoint
        .rsplit_once(':')
        .map(|(host, _)| host.to_owned())
        .unwrap_or_else(|| endpoint.to_owned())
}

#[cfg(test)]
mod tests {
    use super::endpoint_host;

    #[test]
    fn extracts_endpoint_hosts() {
        assert_eq!(endpoint_host("192.168.1.2:37001"), "192.168.1.2");
        assert_eq!(endpoint_host("[fe80::1]:37001"), "fe80::1");
        assert_eq!(endpoint_host("phone.local:37001"), "phone.local");
    }
}
