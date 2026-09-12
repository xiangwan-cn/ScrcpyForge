use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Result;
use tokio::sync::{broadcast, Mutex, RwLock, Semaphore};

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
    mdns_last_probe: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    stopping_all: Arc<AtomicBool>,
    // Incremented whenever stop-all starts. A start that was already in the
    // ADB/scrcpy connection phase must not insert its result after that stop
    // operation has drained the session map.
    start_epoch: Arc<std::sync::atomic::AtomicU64>,
    stop_all_gate: Arc<Mutex<()>>,
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
            mdns_last_probe: Default::default(),
            stopping_all: Arc::new(AtomicBool::new(false)),
            start_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stop_all_gate: Arc::new(Mutex::new(())),
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
        let (alive, stale) = {
            let sessions = self.sessions.read().await;
            sessions.iter().fold(
                (Vec::new(), Vec::new()),
                |(mut alive, mut stale), (serial, session)| {
                    if session.is_alive() {
                        alive.push((serial.clone(), session.clone()));
                    } else {
                        stale.push((serial.clone(), session.clone()));
                    }
                    (alive, stale)
                },
            )
        };
        if !stale.is_empty() {
            let mut sessions = self.sessions.write().await;
            let mut removed = Vec::new();
            for (serial, session) in stale {
                if sessions
                    .get(&serial)
                    .is_some_and(|current| Arc::ptr_eq(current, &session))
                {
                    removed.push(session);
                    sessions.remove(&serial);
                }
            }
            drop(sessions);
            for session in removed {
                let _ = session.shutdown().await;
            }
        }
        alive
    }
    pub async fn start_session(
        &self,
        serial: String,
        options: SessionOptions,
    ) -> Result<Arc<ScrcpySession>> {
        if self.stopping_all.load(Ordering::Acquire) {
            anyhow::bail!("session manager is stopping all sessions")
        }
        let start_epoch = self.start_epoch.load(Ordering::Acquire);
        let lock = self
            .start_locks
            .lock()
            .unwrap()
            .entry(serial.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if self.stopping_all.load(Ordering::Acquire)
            || self.start_epoch.load(Ordering::Acquire) != start_epoch
        {
            drop(_guard);
            self.prune_start_lock(&serial, &lock);
            anyhow::bail!("session manager is stopping all sessions")
        }
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
        let mut sessions = self.sessions.write().await;
        if self.stopping_all.load(Ordering::Acquire)
            || self.start_epoch.load(Ordering::Acquire) != start_epoch
        {
            drop(sessions);
            let _ = session.shutdown().await;
            drop(_guard);
            self.prune_start_lock(&serial, &lock);
            anyhow::bail!("session manager is stopping all sessions")
        }
        sessions.insert(serial, session.clone());
        drop(sessions);
        drop(_guard);
        self.prune_start_lock(&session.serial, &lock);
        Ok(session)
    }
    pub async fn stop_session(&self, serial: &str) -> bool {
        // Serialize stop with a concurrent start for the same serial. Without
        // this guard, a start finishing just after the map removal could put a
        // fresh session back into the manager after the stop request returns.
        let lock = self
            .start_locks
            .lock()
            .unwrap()
            .entry(serial.to_owned())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let session = self.sessions.write().await.remove(serial);
        if let Some(session) = session {
            let _ = session.shutdown().await;
            drop(_guard);
            self.prune_start_lock(serial, &lock);
            true
        } else {
            drop(_guard);
            self.prune_start_lock(serial, &lock);
            false
        }
    }
    pub async fn stop_all_sessions(&self) {
        let _stop_all_guard = self.stop_all_gate.lock().await;
        self.start_epoch.fetch_add(1, Ordering::AcqRel);
        self.stopping_all.store(true, Ordering::Release);
        let _reset = StopAllFlag(self.stopping_all.clone());
        let sessions = self
            .sessions
            .write()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>();
        let concurrency = std::env::var("SCRCPYFORGE_SESSION_STOP_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(4)
            .clamp(1, 16);
        let permits = Arc::new(Semaphore::new(concurrency));
        let mut tasks = tokio::task::JoinSet::new();
        for session in sessions {
            let permit = permits.clone().acquire_owned().await;
            let Ok(permit) = permit else { break };
            tasks.spawn(async move {
                let _permit = permit;
                let _ = session.shutdown().await;
            });
        }
        while tasks.join_next().await.is_some() {}
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
        let probe_mdns = connect_mdns
            && self
                .mdns_last_probe
                .lock()
                .unwrap()
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(10));
        if probe_mdns {
            *self.mdns_last_probe.lock().unwrap() = Some(std::time::Instant::now());
            let endpoints = self
                .adb
                .mdns_endpoints()
                .await
                .unwrap_or_default()
                .into_iter()
                .take(32)
                .collect::<Vec<_>>();
            let permits = Arc::new(Semaphore::new(4));
            let mut tasks = tokio::task::JoinSet::new();
            for endpoint in endpoints {
                let permit = permits.clone().acquire_owned().await;
                let Ok(permit) = permit else { break };
                let adb = self.adb.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _ = adb.connect(&endpoint).await;
                });
            }
            while tasks.join_next().await.is_some() {}
            devices = self.adb.devices().await?;
        }
        // adb 自身的 mDNS 自动连接可能把同一台设备的 IPv4 和多个 IPv6
        // 地址都登记为 serial。UI 和 per-device 脚本只保留一个稳定入口。
        devices.sort_by(|left, right| {
            (!left.wireless, left.serial.starts_with('['), &left.serial).cmp(&(
                !right.wireless,
                right.serial.starts_with('['),
                &right.serial,
            ))
        });
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

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        for connect_endpoint in self
            .adb
            .mdns_endpoints()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|endpoint| endpoint_host(endpoint) == host)
            .take(32)
        {
            if std::time::Instant::now() >= deadline {
                break;
            }
            if self.adb.connect(&connect_endpoint).await.is_ok() {
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

struct StopAllFlag(Arc<AtomicBool>);

impl Drop for StopAllFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
