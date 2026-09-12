use std::path::Path;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::{DeviceInfo, DeviceState, InputAction, PairingService};

const CONNECT_SERVICE: &str = "_adb-tls-connect._tcp";
const PAIRING_SERVICE: &str = "_adb-tls-pairing._tcp";
const MAX_ADB_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_ADB_ERROR_BYTES: usize = 8 * 1024;
const MAX_DEVICES: usize = 1024;
const MAX_MDNS_SERVICES: usize = 128;

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
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let timeout = std::env::var("SCRCPYFORGE_ADB_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(15));
        let child = command
            .spawn()
            .with_context(|| format!("failed to execute {}", self.program))?;
        let output = tokio::time::timeout(timeout, wait_child_output_limited(child))
            .await
            .with_context(|| {
                format!(
                    "{} timed out after {} ms",
                    self.program,
                    timeout.as_millis()
                )
            })??;
        let (status, stdout, stderr) = output;
        if !status.success() {
            bail!("adb failed: {}", bounded_diagnostic(&stderr));
        }
        Ok(stdout)
    }

    pub async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let bytes = self.output(&["devices", "-l"]).await?;
        Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .skip(1)
            .take(MAX_DEVICES)
            .filter_map(parse_device)
            .collect())
    }

    pub async fn connect(&self, endpoint: &str) -> Result<()> {
        validate_endpoint(endpoint)?;
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

    pub async fn pair(&self, endpoint: &str, code: &str) -> Result<()> {
        validate_endpoint(endpoint)?;
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("pairing code must contain exactly six digits");
        }

        let mut command = Command::new(&self.program);
        command
            .args(["pair", endpoint])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to execute {}", self.program))?;
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open adb pair input")?;
        stdin
            .write_all(format!("{code}\n").as_bytes())
            .await
            .context("failed to send pairing code to adb")?;
        drop(stdin);

        let output =
            tokio::time::timeout(Duration::from_secs(15), wait_child_output_limited(child))
                .await
                .context("adb pair timed out")??;
        let (status, stdout_bytes, stderr_bytes) = output;
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        if !status.success()
            || stdout.to_ascii_lowercase().contains("failed")
            || stderr.to_ascii_lowercase().contains("failed")
        {
            let message = if stderr.trim().is_empty() {
                bounded_diagnostic(&stdout_bytes)
            } else {
                bounded_diagnostic(&stderr_bytes)
            };
            bail!("adb pair failed: {}", message.replace(code, "******"));
        }
        Ok(())
    }

    pub async fn mdns_endpoints(&self) -> Result<Vec<String>> {
        if let Ok(bytes) = self.output(&["mdns", "services"]).await {
            let endpoints: Vec<_> = parse_mdns_services(&bytes, CONNECT_SERVICE)
                .into_iter()
                .map(|service| service.endpoint)
                .collect();
            if !endpoints.is_empty() {
                return Ok(endpoints);
            }
        }
        // 部分发行版的 adb 未编译 mDNS host service。直接浏览 DNS-SD，
        // 仍可发现已配对且开启“无线调试”的设备，无需用户追踪动态端口。
        Ok(
            tokio::task::spawn_blocking(|| discover_adb_mdns(CONNECT_SERVICE))
                .await??
                .into_iter()
                .map(|service| service.endpoint)
                .collect(),
        )
    }

    pub async fn pairing_services(&self) -> Result<Vec<PairingService>> {
        if let Ok(bytes) = self.output(&["mdns", "services"]).await {
            let services = parse_mdns_services(&bytes, PAIRING_SERVICE);
            if !services.is_empty() {
                return Ok(services);
            }
        }
        tokio::task::spawn_blocking(|| discover_adb_mdns(PAIRING_SERVICE)).await?
    }

    pub async fn screenshot(&self, serial: &str) -> Result<Vec<u8>> {
        let output = self
            .output(&["-s", serial, "exec-out", "screencap", "-p"])
            .await?;
        if output.len() > 16 * 1024 * 1024 {
            bail!("ADB screenshot exceeds 16 MiB")
        }
        Ok(output)
    }

    pub async fn screenshot_to(&self, serial: &str, path: &Path) -> Result<()> {
        tokio::fs::write(path, self.screenshot(serial).await?).await?;
        Ok(())
    }

    pub async fn input(&self, serial: &str, action: &InputAction) -> Result<()> {
        action.validate().map_err(anyhow::Error::msg)?;
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

async fn wait_child_output_limited(
    mut child: Child,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let stdout = child.stdout.take().context("failed to open adb stdout")?;
    let stderr = child.stderr.take().context("failed to open adb stderr")?;
    let budget = Arc::new(AtomicUsize::new(0));
    let mut stdout_reader = Box::pin(read_output_limited(stdout, budget.clone()));
    let mut stderr_reader = Box::pin(read_output_limited(stderr, budget));
    let mut wait = Box::pin(child.wait());
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut status = None;
    let mut stdout_result = None;
    let mut stderr_result = None;
    while status.is_none() || !stdout_done || !stderr_done {
        tokio::select! {
            result = &mut stdout_reader, if !stdout_done => {
                stdout_done = true;
                stdout_result = Some(result);
                if stdout_result.as_ref().is_some_and(|result| result.is_err()) {
                    break;
                }
            }
            result = &mut stderr_reader, if !stderr_done => {
                stderr_done = true;
                stderr_result = Some(result);
                if stderr_result.as_ref().is_some_and(|result| result.is_err()) {
                    break;
                }
            }
            result = &mut wait, if status.is_none() => {
                status = Some(result.context("failed to wait for adb")?);
            }
        }
    }
    if stdout_result.as_ref().is_some_and(|result| result.is_err())
        || stderr_result.as_ref().is_some_and(|result| result.is_err())
    {
        drop(stdout_reader);
        drop(stderr_reader);
        drop(wait);
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(match stdout_result {
            Some(Err(error)) => error,
            _ => match stderr_result {
                Some(Err(error)) => error,
                _ => anyhow::anyhow!("failed to read adb output"),
            },
        });
    }
    let stdout = stdout_result.context("adb stdout reader did not finish")??;
    let stderr = stderr_result.context("adb stderr reader did not finish")??;
    Ok((
        status.context("adb exited without a status")?,
        stdout,
        stderr,
    ))
}

async fn read_output_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    budget: Arc<AtomicUsize>,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let mut used = budget.load(Ordering::Acquire);
        loop {
            let next = used
                .checked_add(count)
                .context("adb output size overflow")?;
            if next > MAX_ADB_OUTPUT_BYTES {
                bail!(
                    "adb output exceeds {} MiB",
                    MAX_ADB_OUTPUT_BYTES / (1024 * 1024)
                );
            }
            match budget.compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(current) => used = current,
            }
        }
        output.extend_from_slice(&buffer[..count]);
    }
    Ok(output)
}

fn bounded_diagnostic(bytes: &[u8]) -> String {
    let truncated = bytes.len() > MAX_ADB_ERROR_BYTES;
    let mut message = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_ADB_ERROR_BYTES)])
        .trim()
        .to_owned();
    if truncated {
        message.push('…');
    }
    message
}

/// Validate a host:port endpoint before any network or ADB subprocess work.
/// The daemon uses this as its request-level validation so malformed input is
/// reported as a client error instead of an opaque subprocess failure.
pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty()
        || endpoint.len() > 255
        || endpoint
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        || endpoint.contains('/')
    {
        bail!("invalid network endpoint")
    }
    if let Ok(address) = endpoint.parse::<std::net::SocketAddr>() {
        if address.port() != 0 {
            return Ok(());
        }
    }
    let (host, port) = if let Some(rest) = endpoint.strip_prefix('[') {
        let (host, port) = rest
            .split_once(']')
            .and_then(|(host, suffix)| suffix.strip_prefix(':').map(|port| (host, port)))
            .context("invalid bracketed endpoint")?;
        (host, port)
    } else {
        endpoint
            .rsplit_once(':')
            .context("endpoint must include a port")?
    };
    let ipv6_host = host
        .split_once('%')
        .map(|(address, _)| address)
        .unwrap_or(host)
        .parse::<std::net::Ipv6Addr>()
        .is_ok();
    if host.is_empty()
        || !port.parse::<u16>().ok().is_some_and(|value| value != 0)
        || (host.contains(':') && !ipv6_host)
        || host
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || b".-_:%".contains(&byte)))
    {
        bail!("invalid network endpoint")
    }
    Ok(())
}

fn discover_adb_mdns(service_type: &str) -> Result<Vec<PairingService>> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let daemon = ServiceDaemon::new()?;
    let service_fullname = format!("{service_type}.local.");
    let receiver = daemon.browse(&service_fullname)?;
    let timeout_ms = std::env::var("SCRCPYFORGE_MDNS_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2_000)
        .clamp(500, 4_000);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut services = Vec::new();
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
                    if !services
                        .iter()
                        .any(|item: &PairingService| item.endpoint == endpoint)
                    {
                        let name = info
                            .get_fullname()
                            .strip_suffix(&format!(".{service_fullname}"))
                            .unwrap_or(info.get_fullname())
                            .to_owned();
                        services.push(PairingService { name, endpoint })
                    }
                    if services.len() >= MAX_MDNS_SERVICES {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = daemon.stop_browse(&service_fullname);
    let _ = daemon.shutdown();
    Ok(services)
}

fn parse_mdns_services(bytes: &[u8], service_type: &str) -> Vec<PairingService> {
    let mut services = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(index) = fields
            .iter()
            .position(|field| field.trim_end_matches('.') == service_type)
        else {
            continue;
        };
        let (Some(name), Some(endpoint)) = (
            index.checked_sub(1).and_then(|i| fields.get(i)),
            fields.get(index + 1),
        ) else {
            continue;
        };
        if !services
            .iter()
            .any(|item: &PairingService| item.endpoint == *endpoint)
        {
            services.push(PairingService {
                name: (*name).to_owned(),
                endpoint: (*endpoint).to_owned(),
            });
            if services.len() >= MAX_MDNS_SERVICES {
                break;
            }
        }
    }
    services
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

    #[test]
    fn parses_pairing_mdns_services_only() {
        let output = b"List of discovered mdns services\n\
adb-pair _adb-tls-pairing._tcp. 192.168.1.2:37123\n\
adb-connect _adb-tls-connect._tcp 192.168.1.2:39555\n";
        assert_eq!(
            parse_mdns_services(output, PAIRING_SERVICE),
            vec![PairingService {
                name: "adb-pair".into(),
                endpoint: "192.168.1.2:37123".into(),
            }]
        );
    }

    #[test]
    fn ignores_duplicate_mdns_endpoints() {
        let output = b"one _adb-tls-connect._tcp [fe80::1]:4000\n\
two _adb-tls-connect._tcp [fe80::1]:4000\n";
        assert_eq!(parse_mdns_services(output, CONNECT_SERVICE).len(), 1);
    }

    #[test]
    fn validates_network_endpoints_without_accepting_ambiguous_hosts() {
        for endpoint in [
            "192.168.1.2:5555",
            "[fe80::1]:5555",
            "fe80::1:5555",
            "phone.local:5555",
        ] {
            assert!(validate_endpoint(endpoint).is_ok(), "{endpoint}");
        }
        for endpoint in [
            "",
            "phone.local",
            "phone:bad",
            "a:b:5555",
            "host:0",
            "host/5555",
        ] {
            assert!(validate_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_pairing_code_before_starting_adb() {
        let adb = Adb {
            program: "this-adb-must-not-run".into(),
        };
        let error = adb
            .pair("192.168.1.2:37123", "12345")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("six digits"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sends_pairing_code_only_over_stdin() {
        use std::os::unix::fs::PermissionsExt;

        let script = std::env::temp_dir().join(format!("forge-adb-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             [ \"$#\" -eq 2 ] || exit 10\n\
             [ \"$1\" = pair ] || exit 11\n\
             [ \"$2\" = 192.168.1.2:37123 ] || exit 12\n\
             read -r code\n\
             [ \"$code\" = 123456 ] || exit 13\n\
             echo Successfully paired\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let adb = Adb {
            program: script.to_string_lossy().into_owned(),
        };
        let result = adb.pair("192.168.1.2:37123", "123456").await;
        let _ = std::fs::remove_file(script);
        result.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn redacts_pairing_code_from_adb_errors() {
        use std::os::unix::fs::PermissionsExt;

        let script = std::env::temp_dir().join(format!("forge-adb-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             read -r code\n\
             echo \"Failed: $code\" >&2\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let adb = Adb {
            program: script.to_string_lossy().into_owned(),
        };
        let error = adb
            .pair("192.168.1.2:37123", "123456")
            .await
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_file(script);
        assert!(!error.contains("123456"));
        assert!(error.contains("******"));
    }
}
