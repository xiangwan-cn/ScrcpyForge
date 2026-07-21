use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    sync::{broadcast, mpsc, oneshot, watch, Mutex, RwLock},
};

use crate::{
    adb::Adb,
    protocol::{
        control,
        stream::{Codec, PacketReader, StreamPacket},
    },
    video::{FfmpegDecoder, VideoFrame},
};

const REMOTE_SERVER: &str = "/data/local/tmp/scrcpy-server-v4.0.jar";

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub server_jar: PathBuf,
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
    pub codec: Codec,
    pub encoder: Option<String>,
    pub stay_awake: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            server_jar: default_server_jar(),
            max_size: 1280,
            bit_rate: 8_000_000,
            max_fps: 60,
            codec: Codec::H264,
            encoder: None,
            stay_awake: true,
        }
    }
}

fn default_server_jar() -> PathBuf {
    if let Some(path) = std::env::var_os("SCRCPYFORGE_SERVER_JAR") {
        return path.into();
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("resources/scrcpy-server-v4.0.jar"));
            candidates.push(dir.join("../share/scrcpyforge/scrcpy-server-v4.0.jar"));
            if let Some(root) = dir.parent().and_then(|path| path.parent()) {
                candidates.push(root.join("third_party/scrcpy-server-v4.0.jar"));
                candidates.push(root.join("scrcpyforge-rs/third_party/scrcpy-server-v4.0.jar"));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("scrcpyforge-rs/third_party/scrcpy-server-v4.0.jar"));
        candidates.push(cwd.join("third_party/scrcpy-server-v4.0.jar"));
    }
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("scrcpy-server-v4.0.jar"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewMode {
    Off,
    Realtime,
    FiveSeconds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceProfile {
    Auto,
    Eco,
    Balanced,
    Realtime,
}
impl Default for PerformanceProfile {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetrics {
    pub decoded_frames: u64,
    pub preview_frames: u64,
    pub script_frames: u64,
    pub dropped_script_frames: u64,
    pub decoded_fps: f64,
    pub preview_fps: f64,
    pub script_fps: f64,
    pub latest_frame_age_ms: f64,
    pub average_script_ms: f64,
    pub script_p50_ms: f64,
    pub script_p95_ms: f64,
    pub profile: PerformanceProfile,
    pub preview_profile: PerformanceProfile,
}

pub struct FrameHub {
    latest: watch::Sender<Option<Arc<VideoFrame>>>,
    preview: broadcast::Sender<Arc<VideoFrame>>,
    mode: RwLock<PreviewMode>,
    last_preview: Mutex<Instant>,
    last_script: Mutex<Instant>,
    script: Mutex<Option<watch::Sender<Option<Arc<VideoFrame>>>>>,
    script_profile: RwLock<PerformanceProfile>,
    preview_profile: RwLock<PerformanceProfile>,
    started: Instant,
    decoded: AtomicU64,
    previewed: AtomicU64,
    scripted: AtomicU64,
    script_published: AtomicU64,
    window: std::sync::Mutex<MetricWindow>,
}
#[derive(Default)]
struct MetricWindow {
    decoded: VecDeque<Instant>,
    previewed: VecDeque<Instant>,
    scripted: VecDeque<Instant>,
    script_ms: VecDeque<u64>,
}

impl FrameHub {
    pub fn new() -> Self {
        let (latest, _) = watch::channel(None);
        let (preview, _) = broadcast::channel(2);
        Self {
            latest,
            preview,
            mode: RwLock::new(PreviewMode::Realtime),
            last_preview: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_script: Mutex::new(Instant::now() - Duration::from_secs(10)),
            script: Mutex::new(None),
            script_profile: RwLock::new(PerformanceProfile::Auto),
            preview_profile: RwLock::new(PerformanceProfile::Eco),
            started: Instant::now(),
            decoded: AtomicU64::new(0),
            previewed: AtomicU64::new(0),
            scripted: AtomicU64::new(0),
            script_published: AtomicU64::new(0),
            window: Default::default(),
        }
    }
    pub fn latest(&self) -> watch::Receiver<Option<Arc<VideoFrame>>> {
        self.latest.subscribe()
    }
    pub fn preview(&self) -> broadcast::Receiver<Arc<VideoFrame>> {
        self.preview.subscribe()
    }
    pub async fn set_preview_mode(&self, mode: PreviewMode) {
        *self.mode.write().await = mode;
    }
    pub async fn set_script_profile(&self, profile: PerformanceProfile) {
        *self.script_profile.write().await = profile;
    }
    pub async fn set_preview_profile(&self, profile: PerformanceProfile) {
        *self.preview_profile.write().await = profile;
    }
    pub async fn profile(&self) -> PerformanceProfile {
        *self.script_profile.read().await
    }
    pub async fn metrics(&self) -> SessionMetrics {
        let profile = *self.script_profile.read().await;
        let preview_profile = *self.preview_profile.read().await;
        let decoded = self.decoded.load(Ordering::Relaxed);
        let preview = self.previewed.load(Ordering::Relaxed);
        let script = self.scripted.load(Ordering::Relaxed);
        let published = self.script_published.load(Ordering::Relaxed);
        let cutoff = Instant::now() - Duration::from_secs(5);
        let mut window = self.window.lock().unwrap();
        prune(&mut window.decoded, cutoff);
        prune(&mut window.previewed, cutoff);
        prune(&mut window.scripted, cutoff);
        let seconds = self.started.elapsed().as_secs_f64().min(5.0).max(0.001);
        let decoded_fps = window.decoded.len() as f64 / seconds;
        let preview_fps = window.previewed.len() as f64 / seconds;
        let script_fps = window.scripted.len() as f64 / seconds;
        let mut samples = window.script_ms.iter().copied().collect::<Vec<_>>();
        drop(window);
        let average_script_ms = if samples.is_empty() {
            0.0
        } else {
            samples.iter().map(|v| *v as f64).sum::<f64>() / samples.len() as f64 / 1_000_000.0
        };
        samples.sort_unstable();
        let percentile = |p: f64| {
            samples
                .get(((samples.len().saturating_sub(1)) as f64 * p) as usize)
                .copied()
                .unwrap_or(0) as f64
                / 1_000_000.0
        };
        SessionMetrics {
            decoded_frames: decoded,
            preview_frames: preview,
            script_frames: script,
            dropped_script_frames: published.saturating_sub(script + 1),
            decoded_fps,
            preview_fps,
            script_fps,
            latest_frame_age_ms: self
                .latest
                .borrow()
                .as_ref()
                .map(|frame| frame.age_ms())
                .unwrap_or(0.0),
            average_script_ms,
            script_p50_ms: percentile(0.50),
            script_p95_ms: percentile(0.95),
            profile,
            preview_profile,
        }
    }
    pub fn record_script(&self, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.scripted.fetch_add(1, Ordering::Relaxed);
        let mut window = self.window.lock().unwrap();
        push_time(&mut window.scripted);
        if window.script_ms.len() >= 256 {
            window.script_ms.pop_front();
        }
        window.script_ms.push_back(ns);
    }
    pub async fn attach_script(&self, sender: watch::Sender<Option<Arc<VideoFrame>>>) {
        self.scripted.store(0, Ordering::Relaxed);
        self.script_published.store(0, Ordering::Relaxed);
        {
            let mut window = self.window.lock().unwrap();
            window.scripted.clear();
            window.script_ms.clear();
        }
        *self.last_script.lock().await = Instant::now() - Duration::from_secs(1);
        *self.script.lock().await = Some(sender);
    }
    pub async fn detach_script(&self) {
        self.script.lock().await.take();
    }
    async fn publish(&self, frame: Arc<VideoFrame>) {
        self.decoded.fetch_add(1, Ordering::Relaxed);
        push_time(&mut self.window.lock().unwrap().decoded);
        self.latest.send_replace(Some(frame.clone()));
        // A script always receives the newest decoded frame. Replacing the slot
        // is non-blocking, so slow image processing can never build a latency queue.
        if let Some(tx) = self.script.lock().await.clone() {
            self.script_published.fetch_add(1, Ordering::Relaxed);
            let profile = *self.script_profile.read().await;
            let interval = match profile {
                PerformanceProfile::Realtime => Duration::ZERO,
                PerformanceProfile::Balanced => Duration::from_millis(33),
                PerformanceProfile::Eco => Duration::from_millis(150),
                PerformanceProfile::Auto => {
                    let samples = self
                        .window
                        .lock()
                        .unwrap()
                        .script_ms
                        .iter()
                        .rev()
                        .take(32)
                        .copied()
                        .collect::<Vec<_>>();
                    let average = if samples.is_empty() {
                        0
                    } else {
                        samples.iter().sum::<u64>() / samples.len() as u64
                    };
                    if average < 20_000_000 {
                        Duration::from_millis(16)
                    } else if average < 35_000_000 {
                        Duration::from_millis(33)
                    } else {
                        Duration::from_millis(50)
                    }
                }
            };
            let mut last = self.last_script.lock().await;
            if last.elapsed() >= interval {
                *last = Instant::now();
                tx.send_replace(Some(frame.clone()));
            }
        }
        let mode = *self.mode.read().await;
        let profile = *self.preview_profile.read().await;
        let interval = match mode {
            PreviewMode::Off => None,
            PreviewMode::FiveSeconds => Some(Duration::from_secs(5)),
            PreviewMode::Realtime => Some(match profile {
                PerformanceProfile::Realtime => Duration::from_millis(33),
                PerformanceProfile::Balanced => Duration::from_millis(100),
                PerformanceProfile::Eco => Duration::from_millis(500),
                PerformanceProfile::Auto => Duration::from_millis(200),
            }),
        };
        let emit = if let Some(interval) = interval {
            let mut last = self.last_preview.lock().await;
            if last.elapsed() >= interval {
                *last = Instant::now();
                true
            } else {
                false
            }
        } else {
            false
        };
        if emit {
            self.previewed.fetch_add(1, Ordering::Relaxed);
            push_time(&mut self.window.lock().unwrap().previewed);
            let _ = self.preview.send(frame);
        }
    }
}

impl Default for FrameHub {
    fn default() -> Self {
        Self::new()
    }
}
fn push_time(queue: &mut VecDeque<Instant>) {
    if queue.len() >= 1200 {
        queue.pop_front();
    }
    queue.push_back(Instant::now());
}
fn prune(queue: &mut VecDeque<Instant>, cutoff: Instant) {
    while queue.front().is_some_and(|value| *value < cutoff) {
        queue.pop_front();
    }
}

pub struct ScrcpySession {
    pub serial: String,
    pub device_name: String,
    pub codec: Codec,
    pub size: Arc<RwLock<(u32, u32)>>,
    pub frames: Arc<FrameHub>,
    input: Mutex<Option<mpsc::Sender<InputBatch>>>,
    server: Mutex<Option<Child>>,
    forward_ports: [u16; 2],
    alive: Arc<std::sync::atomic::AtomicBool>,
}
struct InputBatch {
    messages: Vec<(Vec<u8>, Duration)>,
    done: oneshot::Sender<Result<()>>,
}

impl ScrcpySession {
    pub async fn connect(serial: String, options: SessionOptions) -> Result<Self> {
        if !options.server_jar.exists() {
            bail!(
                "scrcpy-server v4.0 not found at {}",
                options.server_jar.display()
            )
        }
        let adb = Adb::default();
        let removed = adb.cleanup_scrcpy_forwards(&serial).await.unwrap_or(0);
        if removed > 0 {
            tracing::info!(%serial,removed,"removed stale scrcpy forwards");
        }
        let jar = options.server_jar.to_string_lossy();
        adb.output(&["-s", &serial, "push", &jar, REMOTE_SERVER])
            .await?;
        let scid: u32 = rand::rng().random_range(1..=0x7fff_ffff);
        let scid_hex = format!("{scid:08x}");
        let video_port = free_port()?;
        let control_port = free_port()?;
        let socket = format!("localabstract:scrcpy_{scid_hex}");
        for port in [video_port, control_port] {
            adb.output(&["-s", &serial, "forward", &format!("tcp:{port}"), &socket])
                .await?;
        }
        let codec_name = match options.codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
        };
        let mut args = vec![
            "-s".to_string(),
            serial.clone(),
            "shell".into(),
            format!("CLASSPATH={REMOTE_SERVER}"),
            "app_process".into(),
            "/".into(),
            "com.genymobile.scrcpy.Server".into(),
            "4.0".into(),
            format!("scid={scid_hex}"),
            "video=true".into(),
            "audio=false".into(),
            "control=true".into(),
            "tunnel_forward=true".into(),
            "send_frame_meta=true".into(),
            "send_dummy_byte=true".into(),
            "cleanup=false".into(),
            format!("max_size={}", options.max_size),
            format!("video_codec={codec_name}"),
            format!("video_bit_rate={}", options.bit_rate),
            format!("max_fps={}", options.max_fps),
            format!("stay_awake={}", options.stay_awake),
        ];
        if let Some(encoder) = options.encoder {
            args.push(format!("video_encoder={encoder}"));
        }
        let server =
            Command::new(std::env::var("SCRCPYFORGE_ADB").unwrap_or_else(|_| "adb".into()))
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
        let mut video = connect_video(video_port)
            .await
            .context("video dummy byte")?;
        let control = connect_retry(control_port)
            .await
            .context("control socket")?;
        let mut name = [0u8; 64];
        video.read_exact(&mut name).await.context("device name")?;
        let device_name = String::from_utf8_lossy(&name).trim_end_matches('\0').into();
        let mut codec_id = [0u8; 4];
        video
            .read_exact(&mut codec_id)
            .await
            .context("video codec id")?;
        let actual_codec = Codec::from_id(u32::from_be_bytes(codec_id))?;
        let mut reader = PacketReader::default();
        let (width, height) = match reader
            .read(&mut video, actual_codec)
            .await
            .context("initial session header")?
        {
            StreamPacket::Session { width, height, .. } => (width, height),
            _ => bail!("expected initial session packet"),
        };
        let frames = Arc::new(FrameHub::new());
        let size = Arc::new(RwLock::new((width, height)));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (packet_tx, mut packet_rx) = mpsc::channel::<(Vec<u8>, i64)>(8);
        let decode_frames = frames.clone();
        let decode_size = size.clone();
        let decode_alive = alive.clone();
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name(format!("forge-decode-{serial}"))
            .spawn(move || {
                let mut decoder = match FfmpegDecoder::new(actual_codec, width, height) {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::error!(%error,"decoder init failed");
                        decode_alive.store(false, Ordering::Relaxed);
                        return;
                    }
                };
                while let Some((data, pts)) = packet_rx.blocking_recv() {
                    match decoder.decode(&data, pts) {
                        Ok(decoded) => {
                            for frame in decoded {
                                runtime.block_on(async {
                                    *decode_size.write().await = (frame.width, frame.height);
                                    decode_frames.publish(Arc::new(frame)).await;
                                });
                            }
                        }
                        Err(error) => tracing::debug!(%error,"packet decode failed"),
                    }
                }
                decode_alive.store(false, Ordering::Relaxed);
            })?;
        let reader_size = size.clone();
        let reader_alive = alive.clone();
        tokio::spawn(async move {
            loop {
                match reader.read(&mut video, actual_codec).await {
                    Ok(StreamPacket::Session { width, height, .. }) => {
                        *reader_size.write().await = (width, height)
                    }
                    Ok(StreamPacket::Media { pts_us, data, .. }) => {
                        if packet_tx.send((data, pts_us.unwrap_or(0))).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error,"video stream ended");
                        break;
                    }
                }
            }
            reader_alive.store(false, Ordering::Relaxed);
        });
        let (input_tx, mut input_rx) = mpsc::channel::<InputBatch>(16);
        tokio::spawn(async move {
            let mut control = control;
            while let Some(batch) = input_rx.recv().await {
                let result = async {
                    for (message, delay) in batch.messages {
                        control.write_all(&message).await?;
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                    }
                    Ok(())
                }
                .await;
                let _ = batch.done.send(result);
                if control.flush().await.is_err() {
                    break;
                }
            }
            let _ = control.shutdown().await;
        });
        Ok(Self {
            serial,
            device_name,
            codec: actual_codec,
            size,
            frames,
            input: Mutex::new(Some(input_tx)),
            server: Mutex::new(Some(server)),
            forward_ports: [video_port, control_port],
            alive,
        })
    }

    async fn send_input(&self, messages: Vec<(Vec<u8>, Duration)>) -> Result<()> {
        if !self.is_alive() {
            bail!("session is stopped")
        }
        let timeout = messages
            .iter()
            .fold(Duration::from_millis(500), |total, (_, delay)| {
                total + *delay
            });
        let sender = self
            .input
            .lock()
            .await
            .clone()
            .context("input writer stopped")?;
        let (tx, rx) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_millis(300),
            sender.send(InputBatch { messages, done: tx }),
        )
        .await
        .context("input queue timed out")?
        .context("input writer stopped")?;
        tokio::time::timeout(timeout, rx)
            .await
            .context("input write timed out")??
    }
    pub async fn tap(&self, x: i32, y: i32) -> Result<()> {
        let (w, h) = *self.size.read().await;
        let mut bytes = control::touch(control::TouchAction::Down, x, y, w as u16, h as u16);
        bytes.extend(control::touch(
            control::TouchAction::Up,
            x,
            y,
            w as u16,
            h as u16,
        ));
        self.send_input(vec![(bytes, Duration::ZERO)]).await
    }
    pub async fn tap_random(&self, x: i32, y: i32, radius: u32) -> Result<()> {
        let (w, h) = *self.size.read().await;
        let (tx, ty) = random_point(x, y, radius);
        self.tap(
            tx.clamp(0, w.saturating_sub(1) as i32),
            ty.clamp(0, h.saturating_sub(1) as i32),
        )
        .await
    }
    pub async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, duration_ms: u64) -> Result<()> {
        let (w, h) = *self.size.read().await;
        let steps = (duration_ms / 16).max(2);
        let delay = Duration::from_millis(duration_ms / steps);
        let messages = (0..steps)
            .map(|i| {
                let t = i as f64 / (steps - 1) as f64;
                let x = (x1 as f64 + (x2 - x1) as f64 * t) as i32;
                let y = (y1 as f64 + (y2 - y1) as f64 * t) as i32;
                let action = if i == 0 {
                    control::TouchAction::Down
                } else if i == steps - 1 {
                    control::TouchAction::Up
                } else {
                    control::TouchAction::Move
                };
                (
                    control::touch(action, x, y, w as u16, h as u16),
                    if i + 1 < steps { delay } else { Duration::ZERO },
                )
            })
            .collect();
        self.send_input(messages).await
    }
    pub async fn long_press(&self, x: i32, y: i32, duration_ms: u64) -> Result<()> {
        let (w, h) = *self.size.read().await;
        self.send_input(vec![
            (
                control::touch(control::TouchAction::Down, x, y, w as u16, h as u16),
                Duration::from_millis(duration_ms),
            ),
            (
                control::touch(control::TouchAction::Up, x, y, w as u16, h as u16),
                Duration::ZERO,
            ),
        ])
        .await
    }
    pub async fn multi_tap(&self, points: &[(i32, i32)]) -> Result<()> {
        let (w, h) = *self.size.read().await;
        let mut bytes = Vec::new();
        for (i, (x, y)) in points.iter().enumerate() {
            bytes.extend(control::touch_pointer(
                control::TouchAction::Down,
                *x,
                *y,
                w as u16,
                h as u16,
                i as u64,
            ))
        }
        let mut up = Vec::new();
        for (i, (x, y)) in points.iter().enumerate() {
            up.extend(control::touch_pointer(
                control::TouchAction::Up,
                *x,
                *y,
                w as u16,
                h as u16,
                i as u64,
            ))
        }
        self.send_input(vec![
            (bytes, Duration::from_millis(50)),
            (up, Duration::ZERO),
        ])
        .await
    }
    pub async fn key(&self, code: u32, long_press: bool) -> Result<()> {
        self.send_input(vec![
            (
                control::keycode(code, 0, 0),
                if long_press {
                    Duration::from_millis(500)
                } else {
                    Duration::ZERO
                },
            ),
            (control::keycode(code, 1, 0), Duration::ZERO),
        ])
        .await
    }
    pub async fn back(&self) -> Result<()> {
        self.send_input(vec![(
            control::back_or_screen_on(0).to_vec(),
            Duration::ZERO,
        )])
        .await
    }
    pub async fn screen_power(&self, on: bool) -> Result<()> {
        self.send_input(vec![(
            control::screen_power(if on { 2 } else { 0 }).to_vec(),
            Duration::ZERO,
        )])
        .await
    }
    pub async fn text(&self, value: &str) -> Result<()> {
        self.send_input(vec![(control::text(value), Duration::ZERO)])
            .await
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.alive.store(false, Ordering::Relaxed);
        self.input.lock().await.take();
        self.frames.detach_script().await;
        if let Some(mut server) = self.server.lock().await.take() {
            let _ = server.kill().await;
            let _ = server.wait().await;
        }
        let adb = Adb::default();
        for port in self.forward_ports {
            let _ = adb
                .output(&[
                    "-s",
                    &self.serial,
                    "forward",
                    "--remove",
                    &format!("tcp:{port}"),
                ])
                .await;
        }
        Ok(())
    }
}

fn random_point(x: i32, y: i32, radius: u32) -> (i32, i32) {
    if radius == 0 {
        return (x, y);
    }
    let mut rng = rand::rng();
    loop {
        let r = radius.min(i32::MAX as u32) as i32;
        let dx = rng.random_range(-r..=r);
        let dy = rng.random_range(-r..=r);
        if dx as i64 * dx as i64 + dy as i64 * dy as i64 <= r as i64 * r as i64 {
            return (x.saturating_add(dx), y.saturating_add(dy));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::random_point;
    #[test]
    fn random_tap_stays_inside_radius() {
        for _ in 0..1000 {
            let (x, y) = random_point(100, 200, 12);
            let dx = (x - 100) as i64;
            let dy = (y - 200) as i64;
            assert!(dx * dx + dy * dy <= 144)
        }
    }
    #[test]
    fn zero_radius_is_exact() {
        assert_eq!(random_point(10, 20, 0), (10, 20));
    }
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
async fn connect_retry(port: u16) -> Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => {
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(_e) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
}
async fn connect_video(port: u16) -> Result<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(mut s) => {
                s.set_nodelay(true)?;
                let mut dummy = [0u8; 1];
                match tokio::time::timeout(Duration::from_millis(750), s.read_exact(&mut dummy))
                    .await
                {
                    Ok(Ok(_)) => return Ok(s),
                    _ if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                    _ => bail!("server closed before dummy byte"),
                }
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => return Err(e.into()),
        }
    }
}

impl Drop for ScrcpySession {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Ok(mut guard) = self.server.try_lock() {
            if let Some(server) = guard.as_mut() {
                let _ = server.start_kill();
            }
        }
    }
}
