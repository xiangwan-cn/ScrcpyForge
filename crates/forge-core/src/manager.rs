use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use tokio::sync::{broadcast, RwLock};

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
            if let Some(session) = self.sessions.write().await.remove(serial) {
                let _ = session.shutdown().await;
            }
            None
        } else {
            value
        }
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
            return Ok(existing);
        }
        let session = Arc::new(ScrcpySession::connect(serial.clone(), options).await?);
        self.sessions.write().await.insert(serial, session.clone());
        Ok(session)
    }
    pub async fn stop_session(&self, serial: &str) -> bool {
        let session = self.sessions.write().await.remove(serial);
        if let Some(session) = session {
            let _ = session.shutdown().await;
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
    }

    pub async fn scan(&self, connect_mdns: bool) -> Result<Vec<DeviceInfo>> {
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
            !device.wireless
                || (device.product.is_none() && device.model.is_none())
                || seen_wireless.insert((device.product.clone(), device.model.clone()))
        });
        *self.devices.write().await = devices.clone();
        let _ = self.events.send(ForgeEvent::DeviceSnapshot {
            devices: devices.clone(),
        });
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
