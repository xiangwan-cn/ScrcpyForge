use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::{DeviceInfo, DeviceState, InputAction};

#[derive(Debug, Clone)]
pub struct Adb {
    program: String,
}

impl Default for Adb {
    fn default() -> Self {
        Self {
            program: std::env::var("SCRCPYFORGE_ADB").unwrap_or_else(|_| "adb".into()),
        }
    }
}

impl Adb {
    pub async fn output(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.program);
        command.args(args).stdin(Stdio::null()).kill_on_drop(true);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to execute {}", self.program))?;
        if !output.status.success() {
            bail!(
                "adb failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    pub async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let bytes = self.output(&["devices", "-l"]).await?;
        Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .skip(1)
            .filter_map(parse_device)
            .collect())
    }

    pub async fn connect(&self, endpoint: &str) -> Result<()> {
        // adb server keeps retrying a silent/stale endpoint even after the
        // short-lived `adb connect` client is killed. Probe TCP first so an
        // old wireless-debugging mDNS record cannot accumulate SYN attempts.
        let stream = tokio::time::timeout(
            Duration::from_millis(1200),
            tokio::net::TcpStream::connect(endpoint),
        )
        .await
        .with_context(|| format!("wireless ADB endpoint did not respond: {endpoint}"))?
        .with_context(|| format!("wireless ADB endpoint is unavailable: {endpoint}"))?;
        drop(stream);
        tokio::time::timeout(Duration::from_secs(3), self.output(&["connect", endpoint]))
            .await
            .with_context(|| format!("adb connect timed out: {endpoint}"))??;
        Ok(())
    }

    pub async fn mdns_endpoints(&self) -> Result<Vec<String>> {
        if let Ok(bytes) = self.output(&["mdns", "services"]).await {
            let endpoints: Vec<_> = String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| line.contains("_adb-tls-connect._tcp"))
                .filter_map(|line| line.split_whitespace().last().map(str::to_owned))
                .collect();
            if !endpoints.is_empty() {
                return Ok(endpoints);
            }
        }
        // 部分发行版的 adb 未编译 mDNS host service。直接浏览 DNS-SD，
        // 仍可发现已配对且开启“无线调试”的设备，无需用户追踪动态端口。
        tokio::task::spawn_blocking(discover_adb_mdns).await?
    }

    pub async fn screenshot(&self, serial: &str) -> Result<Vec<u8>> {
        self.output(&["-s", serial, "exec-out", "screencap", "-p"])
            .await
    }

    pub async fn screenshot_to(&self, serial: &str, path: &Path) -> Result<()> {
        tokio::fs::write(path, self.screenshot(serial).await?).await?;
        Ok(())
    }

    /// Remove forwards left by an interrupted ScrcpyForge daemon. The filter is
    /// deliberately limited to this device and scrcpy localabstract sockets.
    pub async fn cleanup_scrcpy_forwards(&self, serial: &str) -> Result<usize> {
        let output = self.output(&["forward", "--list"]).await?;
        let ports = String::from_utf8_lossy(&output)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let owner = fields.next()?;
                let local = fields.next()?;
                let remote = fields.next()?;
                (owner == serial
                    && local.starts_with("tcp:")
                    && remote.starts_with("localabstract:scrcpy_"))
                .then(|| local.to_owned())
            })
            .collect::<Vec<_>>();
        for port in &ports {
            let _ = self
                .output(&["-s", serial, "forward", "--remove", port])
                .await;
        }
        Ok(ports.len())
    }

    pub async fn input(&self, serial: &str, action: &InputAction) -> Result<()> {
        let mut owned = vec![
            "-s".to_string(),
            serial.to_string(),
            "shell".into(),
            "input".into(),
        ];
        match action {
            InputAction::Tap { x, y } => owned.extend(["tap".into(), x.to_string(), y.to_string()]),
            InputAction::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
            } => owned.extend([
                "swipe".into(),
                x1.to_string(),
                y1.to_string(),
                x2.to_string(),
                y2.to_string(),
                duration_ms.to_string(),
            ]),
            InputAction::Text { value } => owned.extend(["text".into(), value.replace(' ', "%s")]),
            InputAction::Key { code } => owned.extend(["keyevent".into(), code.to_string()]),
        }
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        self.output(&refs).await.map(|_| ())
    }
}

fn discover_adb_mdns() -> Result<Vec<String>> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse("_adb-tls-connect._tcp.local.")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
    let mut endpoints = Vec::new();
    while std::time::Instant::now() < deadline {
        match receiver.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(address) = info
                    .get_addresses()
                    .iter()
                    .find(|a| a.is_ipv4())
                    .or_else(|| info.get_addresses().iter().next())
                {
                    let endpoint = if address.is_ipv6() {
                        format!("[{address}]:{}", info.get_port())
                    } else {
                        format!("{address}:{}", info.get_port())
                    };
                    if !endpoints.contains(&endpoint) {
                        endpoints.push(endpoint)
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    let _ = daemon.stop_browse("_adb-tls-connect._tcp.local.");
    let _ = daemon.shutdown();
    Ok(endpoints)
}

fn parse_device(line: &str) -> Option<DeviceInfo> {
    let mut fields = line.split_whitespace();
    let serial = fields.next()?.to_owned();
    let state = match fields.next()? {
        "device" => DeviceState::Device,
        "offline" => DeviceState::Offline,
        "unauthorized" => DeviceState::Unauthorized,
        _ => DeviceState::Unknown,
    };
    let mut product = None;
    let mut model = None;
    let mut transport_id = None;
    for field in fields {
        if let Some(value) = field.strip_prefix("product:") {
            product = Some(value.into());
        }
        if let Some(value) = field.strip_prefix("model:") {
            model = Some(value.into());
        }
        if let Some(value) = field.strip_prefix("transport_id:") {
            transport_id = Some(value.into());
        }
    }
    let wireless = serial.contains(':');
    Some(DeviceInfo {
        serial,
        state,
        product,
        model,
        transport_id,
        wireless,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_long_device_line() {
        let item = parse_device("192.168.1.2:5555 device product:foo model:Pixel_8 transport_id:4")
            .unwrap();
        assert_eq!(item.model.as_deref(), Some("Pixel_8"));
        assert!(item.wireless);
    }
}
