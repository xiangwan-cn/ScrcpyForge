use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
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
            // Keeping the screen awake for an entire idle session costs
            // device battery. Callers that need kiosk behavior can opt in.
            stay_awake: false,
        }
    }
}

impl SessionOptions {
    pub fn validate(&self) -> Result<()> {
        let jar = std::fs::symlink_metadata(&self.server_jar).with_context(|| {
            format!(
                "unable to stat scrcpy-server jar at {}",
                self.server_jar.display()
            )
        })?;
        if !jar.is_file()
            || jar.file_type().is_symlink()
            || jar.len() == 0
            || jar.len() > 64 * 1024 * 1024
        {
            bail!("scrcpy-server jar is not a regular file")
        }
        if !(240..=4096).contains(&self.max_size) {
            bail!("max_size must be between 240 and 4096")
        }
        if !(1..=120).contains(&self.max_fps) {
            bail!("max_fps must be between 1 and 120")
        }
        if !(100_000..=50_000_000).contains(&self.bit_rate) {
            bail!("bit_rate must be between 100000 and 50000000")
        }
        Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewMode {
    Off,
    Realtime,
    FiveSeconds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceProfile {
    #[default]
    Auto,
    Eco,
    Balanced,
    Realtime,
}

impl PerformanceProfile {
    /// Capture defaults used when a caller explicitly selects a profile. Auto
    /// leaves the caller's existing values unchanged.
    pub fn capture_defaults(self) -> Option<(u32, u32, u32)> {
        match self {
            Self::Eco => Some((720, 15, 2_000_000)),
            Self::Balanced => Some((960, 30, 4_000_000)),
            Self::Realtime => Some((1280, 60, 8_000_000)),
            Self::Auto => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityState {
    Active,
    IdleGrace,
    Suspended,
}

const IDLE_GRACE: Duration = Duration::from_secs(10);
const DEEP_IDLE: Duration = Duration::from_secs(60);
const MAX_SWIPE_DURATION_MS: u64 = 60_000;
const MAX_SWIPE_STEPS: u64 = 240;
const MAX_MULTI_TOUCH: usize = 10;
const MAX_VIDEO_DIMENSION: u32 = 8192;
const MAX_TAP_RADIUS: u32 = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct SessionMetrics {
    pub decoded_frames: u64,
    pub preview_frames: u64,
    pub preview_dropped_frames: u64,
    pub script_frames: u64,
    pub dropped_script_frames: u64,
    pub decoded_fps: f64,
    pub preview_fps: f64,
    pub script_fps: f64,
    pub latest_frame_age_ms: f64,
    pub average_script_ms: f64,
    pub script_p50_ms: f64,
    pub script_p95_ms: f64,
    pub script_published: u64,
    pub script_rescans: u64,
    pub script_source_seq: u64,
    pub script_generation: u64,
    pub last_publish_us: u64,
    pub input_batches: u64,
    pub input_failures: u64,
    pub last_input_us: u64,
    pub video_packet_errors: u64,
    pub video_decode_errors: u64,
    pub video_dimension_changes: u64,
    pub last_video_error: Option<String>,
    pub latest_frame_seq: u64,
    pub preview_leases: u64,
    pub script_active: bool,
    pub activity_state: SessionActivityState,
    pub idle_for_ms: u64,
    pub profile: PerformanceProfile,
    pub preview_profile: PerformanceProfile,
}

pub struct FrameHub {
    latest: watch::Sender<Option<Arc<VideoFrame>>>,
    preview: broadcast::Sender<Arc<VideoFrame>>,
    // These are deliberately short synchronous locks. `publish()` runs on
    // the decoder thread and must never await a Tokio mutex or strategy task.
    mode: std::sync::Mutex<PreviewMode>,
    last_preview: std::sync::Mutex<Instant>,
    script: std::sync::Mutex<ScriptSlot>,
    script_profile: std::sync::Mutex<PerformanceProfile>,
    preview_profile: std::sync::Mutex<PerformanceProfile>,
    started: Instant,
    decoded: AtomicU64,
    previewed: AtomicU64,
    preview_dropped: AtomicU64,
    scripted: AtomicU64,
    script_published: AtomicU64,
    script_rescans: AtomicU64,
    script_source_seq: AtomicU64,
    video_packet_errors: AtomicU64,
    video_decode_errors: AtomicU64,
    video_dimension_changes: AtomicU64,
    last_video_error: std::sync::Mutex<Option<String>>,
    preview_leases: AtomicU64,
    frame_requests: AtomicU64,
    script_demand: std::sync::atomic::AtomicBool,
    demand_epoch: AtomicU64,
    demand_notify: Arc<tokio::sync::Notify>,
    deep_idle_requested: std::sync::atomic::AtomicBool,
    idle_shutdown_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    activity: std::sync::Mutex<ActivityState>,
    last_publish_us: AtomicU64,
    input_batches: AtomicU64,
    input_failures: AtomicU64,
    last_input_us: AtomicU64,
    window: std::sync::Mutex<MetricWindow>,
}

pub struct PreviewLease {
    hub: Arc<FrameHub>,
}

impl Drop for PreviewLease {
    fn drop(&mut self) {
        self.hub.release_preview();
    }
}

/// A short-lived lease for an explicit frame request (for example `/latest` or
/// template capture). Unlike a preview lease, this request reactivates the
/// decoder even when continuous preview mode is disabled.
pub struct FrameRequestLease {
    hub: Arc<FrameHub>,
}

impl Drop for FrameRequestLease {
    fn drop(&mut self) {
        self.hub.release_frame_request();
    }
}

struct ScriptSlot {
    generation: u64,
    sender: Option<watch::Sender<Option<Arc<VideoFrame>>>>,
    last_sent: Instant,
    active: bool,
}

struct ActivityState {
    state: SessionActivityState,
    idle_since: Option<Instant>,
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
            // A session can remain connected without an active preview
            // consumer. Five-second preview keeps the default lightweight
            // while one-shot frame requests and scripts still wake decoding
            // immediately when they need a frame.
            mode: std::sync::Mutex::new(PreviewMode::FiveSeconds),
            last_preview: std::sync::Mutex::new(Instant::now() - Duration::from_secs(10)),
            script: std::sync::Mutex::new(ScriptSlot {
                generation: 0,
                sender: None,
                last_sent: Instant::now() - Duration::from_secs(10),
                active: false,
            }),
            script_profile: std::sync::Mutex::new(PerformanceProfile::Auto),
            preview_profile: std::sync::Mutex::new(PerformanceProfile::Eco),
            started: Instant::now(),
            decoded: AtomicU64::new(0),
            previewed: AtomicU64::new(0),
            preview_dropped: AtomicU64::new(0),
            scripted: AtomicU64::new(0),
            script_published: AtomicU64::new(0),
            script_rescans: AtomicU64::new(0),
            script_source_seq: AtomicU64::new(0),
            video_packet_errors: AtomicU64::new(0),
            video_decode_errors: AtomicU64::new(0),
            video_dimension_changes: AtomicU64::new(0),
            last_video_error: std::sync::Mutex::new(None),
            preview_leases: AtomicU64::new(0),
            frame_requests: AtomicU64::new(0),
            script_demand: std::sync::atomic::AtomicBool::new(false),
            demand_epoch: AtomicU64::new(0),
            demand_notify: Arc::new(tokio::sync::Notify::new()),
            deep_idle_requested: std::sync::atomic::AtomicBool::new(false),
            idle_shutdown_hook: std::sync::Mutex::new(None),
            activity: std::sync::Mutex::new(ActivityState {
                state: SessionActivityState::Suspended,
                idle_since: Some(Instant::now()),
            }),
            last_publish_us: AtomicU64::new(0),
            input_batches: AtomicU64::new(0),
            input_failures: AtomicU64::new(0),
            last_input_us: AtomicU64::new(0),
            window: Default::default(),
        }
    }
    pub fn latest(&self) -> watch::Receiver<Option<Arc<VideoFrame>>> {
        self.latest.subscribe()
    }
    pub fn preview(&self) -> broadcast::Receiver<Arc<VideoFrame>> {
        self.preview.subscribe()
    }
    /// Register a preview consumer. The lease is released automatically when
    /// the WebSocket or other consumer is dropped.
    pub fn acquire_preview(self: &Arc<Self>) -> PreviewLease {
        self.preview_leases.fetch_add(1, Ordering::AcqRel);
        self.refresh_activity();
        PreviewLease { hub: self.clone() }
    }
    pub fn preview_leases(&self) -> u64 {
        self.preview_leases.load(Ordering::Acquire)
    }
    /// Register a one-shot frame consumer. The lease stays active until the
    /// request has copied or encoded the frame, then lets the normal idle
    /// grace period resume.
    pub fn acquire_frame_request(self: &Arc<Self>) -> FrameRequestLease {
        self.frame_requests.fetch_add(1, Ordering::AcqRel);
        self.refresh_activity();
        FrameRequestLease { hub: self.clone() }
    }
    pub fn frame_requests(&self) -> u64 {
        self.frame_requests.load(Ordering::Acquire)
    }
    pub fn demand_notify(&self) -> Arc<tokio::sync::Notify> {
        self.demand_notify.clone()
    }
    fn script_active(&self) -> bool {
        self.script_demand.load(Ordering::Acquire)
    }
    fn frame_request_active(&self) -> bool {
        self.frame_requests() > 0
    }
    fn preview_active(&self) -> bool {
        self.preview_leases() > 0 && *self.mode.lock().unwrap() != PreviewMode::Off
    }
    fn mark_active(&self) {
        self.deep_idle_requested.store(false, Ordering::Release);
        let mut activity = self.activity.lock().unwrap();
        if activity.state == SessionActivityState::Suspended {
            self.demand_epoch.fetch_add(1, Ordering::AcqRel);
            // Packets were discarded while suspended. Do not serve a stale
            // image before the resumed decoder reaches a fresh keyframe.
            self.latest.send_replace(None);
            self.demand_notify.notify_one();
        }
        activity.state = SessionActivityState::Active;
        activity.idle_since = None;
    }
    fn release_preview(&self) {
        let mut current = self.preview_leases.load(Ordering::Acquire);
        while current != 0 {
            match self.preview_leases.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
        self.refresh_activity();
    }
    fn release_frame_request(&self) {
        let mut current = self.frame_requests.load(Ordering::Acquire);
        while current != 0 {
            match self.frame_requests.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
        self.refresh_activity();
    }
    fn refresh_activity(&self) {
        let now = Instant::now();
        let mut activity = self.activity.lock().unwrap();
        // Re-read demand after taking the activity lock. Computing it before
        // locking allows a concurrent preview lease or script generation to
        // be overwritten by a stale Active -> IdleGrace transition.
        let active = self.script_active() || self.preview_active() || self.frame_request_active();
        if active {
            if activity.state != SessionActivityState::Active {
                self.demand_epoch.fetch_add(1, Ordering::AcqRel);
                if activity.state == SessionActivityState::Suspended {
                    self.latest.send_replace(None);
                    self.demand_notify.notify_one();
                }
            }
            activity.state = SessionActivityState::Active;
            activity.idle_since = None;
            return;
        }
        match activity.state {
            SessionActivityState::Active => {
                activity.state = SessionActivityState::IdleGrace;
                activity.idle_since = Some(now);
            }
            SessionActivityState::IdleGrace => {
                let idle_since = activity.idle_since.get_or_insert(now);
                let expired = idle_since.elapsed() >= IDLE_GRACE;
                if expired {
                    activity.state = SessionActivityState::Suspended;
                    self.demand_epoch.fetch_add(1, Ordering::AcqRel);
                    self.demand_notify.notify_one();
                    // Keep the latest frame as a cheap snapshot for the next
                    // short-lived consumer. A subsequent transition from
                    // Suspended clears it before waiting for a keyframe.
                }
            }
            SessionActivityState::Suspended => {}
        }
    }
    pub fn activity(&self) -> (SessionActivityState, u64) {
        self.refresh_activity();
        let activity = self.activity.lock().unwrap();
        let idle_for_ms = activity
            .idle_since
            .map(|since| since.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        let should_shutdown = activity.state == SessionActivityState::Suspended
            && activity
                .idle_since
                .is_some_and(|since| since.elapsed() >= DEEP_IDLE)
            && !self.deep_idle_requested.swap(true, Ordering::AcqRel);
        let hook = if should_shutdown {
            self.idle_shutdown_hook.lock().unwrap().clone()
        } else {
            None
        };
        let state = activity.state;
        drop(activity);
        if let Some(hook) = hook {
            hook();
        }
        (state, idle_for_ms)
    }
    pub fn set_idle_shutdown_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.idle_shutdown_hook.lock().unwrap() = Some(hook);
    }
    /// Whether the decoder should retain and distribute decoded frames. During
    /// IdleGrace the decoder stays warm for quick reconnects; only Suspended
    /// drops encoded packets before FFmpeg.
    pub fn video_demand(&self) -> bool {
        let (state, _) = self.activity();
        matches!(
            state,
            SessionActivityState::Active | SessionActivityState::IdleGrace
        )
    }
    pub fn demand_epoch(&self) -> u64 {
        self.demand_epoch.load(Ordering::Acquire)
    }
    pub async fn set_preview_mode(&self, mode: PreviewMode) {
        self.set_preview_mode_now(mode);
    }
    pub fn set_preview_mode_now(&self, mode: PreviewMode) {
        *self.mode.lock().unwrap() = mode;
        self.refresh_activity();
    }
    pub fn preview_mode_now(&self) -> PreviewMode {
        *self.mode.lock().unwrap()
    }
    pub async fn set_script_profile(&self, profile: PerformanceProfile) {
        self.set_script_profile_now(profile);
    }
    pub fn set_script_profile_now(&self, profile: PerformanceProfile) {
        *self.script_profile.lock().unwrap() = profile;
    }
    pub async fn set_preview_profile(&self, profile: PerformanceProfile) {
        self.set_preview_profile_now(profile);
    }
    pub fn set_preview_profile_now(&self, profile: PerformanceProfile) {
        *self.preview_profile.lock().unwrap() = profile;
    }
    pub async fn profile(&self) -> PerformanceProfile {
        self.profile_now()
    }
    pub fn profile_now(&self) -> PerformanceProfile {
        *self.script_profile.lock().unwrap()
    }
    /// Return the small, stable portion of session state used by `/state`.
    /// Keeping this separate from `metrics_now()` avoids calculating rolling
    /// percentiles merely to validate a long-lived state ETag.
    pub fn stable_state_now(
        &self,
    ) -> (
        SessionActivityState,
        bool,
        u64,
        PerformanceProfile,
        PerformanceProfile,
        PreviewMode,
    ) {
        let (activity_state, _) = self.activity();
        let script = self.script.lock().unwrap();
        let script_active = script.active;
        let script_generation = script.generation;
        drop(script);
        let profile = *self.script_profile.lock().unwrap();
        let preview_profile = *self.preview_profile.lock().unwrap();
        let preview_mode = self.preview_mode_now();
        (
            activity_state,
            script_active,
            script_generation,
            profile,
            preview_profile,
            preview_mode,
        )
    }
    pub async fn metrics(&self) -> SessionMetrics {
        self.metrics_now()
    }
    pub fn metrics_now(&self) -> SessionMetrics {
        let profile = *self.script_profile.lock().unwrap();
        let preview_profile = *self.preview_profile.lock().unwrap();
        // Advance IdleGrace/Suspended before sampling the latest frame so a
        // snapshot cannot report an old image together with `suspended`.
        let (activity_state, idle_for_ms) = self.activity();
        let decoded = self.decoded.load(Ordering::Relaxed);
        let preview = self.previewed.load(Ordering::Relaxed);
        let script_frames = self.scripted.load(Ordering::Relaxed);
        let published = self.script_published.load(Ordering::Relaxed);
        let cutoff = Instant::now() - Duration::from_secs(5);
        let mut window = self.window.lock().unwrap();
        prune(&mut window.decoded, cutoff);
        prune(&mut window.previewed, cutoff);
        prune(&mut window.scripted, cutoff);
        let seconds = self.started.elapsed().as_secs_f64().clamp(0.001, 5.0);
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
        // End the watch borrow before taking the script-slot lock. Publish and
        // attach use slot -> latest ordering, so keeping both guards alive in
        // one struct literal could reintroduce a lock inversion.
        let latest_frame_age_ms = self
            .latest
            .borrow()
            .as_ref()
            .map(|frame| frame.age_ms())
            .unwrap_or(0.0);
        let latest_frame_seq = self
            .latest
            .borrow()
            .as_ref()
            .map(|frame| frame.frame_seq)
            .unwrap_or(0);
        let script = self.script.lock().unwrap();
        let script_generation = script.generation;
        let script_active = script.active;
        drop(script);
        SessionMetrics {
            decoded_frames: decoded,
            preview_frames: preview,
            preview_dropped_frames: self.preview_dropped.load(Ordering::Relaxed),
            script_frames,
            dropped_script_frames: published.saturating_sub(script_frames),
            decoded_fps,
            preview_fps,
            script_fps,
            latest_frame_age_ms,
            average_script_ms,
            script_p50_ms: percentile(0.50),
            script_p95_ms: percentile(0.95),
            script_published: published,
            script_rescans: self.script_rescans.load(Ordering::Relaxed),
            script_source_seq: self.script_source_seq.load(Ordering::Relaxed),
            script_generation,
            last_publish_us: self.last_publish_us.load(Ordering::Relaxed),
            input_batches: self.input_batches.load(Ordering::Relaxed),
            input_failures: self.input_failures.load(Ordering::Relaxed),
            last_input_us: self.last_input_us.load(Ordering::Relaxed),
            video_packet_errors: self.video_packet_errors.load(Ordering::Relaxed),
            video_decode_errors: self.video_decode_errors.load(Ordering::Relaxed),
            video_dimension_changes: self.video_dimension_changes.load(Ordering::Relaxed),
            last_video_error: self.last_video_error.lock().unwrap().clone(),
            latest_frame_seq,
            preview_leases: self.preview_leases(),
            script_active,
            activity_state,
            idle_for_ms,
            profile,
            preview_profile,
        }
    }
    pub fn begin_script(&self) -> u64 {
        let mut slot = self.script.lock().unwrap();
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.sender = None;
        slot.active = true;
        let generation = slot.generation;
        self.script_demand.store(true, Ordering::Release);
        drop(slot);
        self.mark_active();
        generation
    }
    pub fn script_generation(&self) -> u64 {
        self.script.lock().unwrap().generation
    }
    pub fn record_script(&self, elapsed: Duration) {
        let generation = self.script_generation();
        self.record_script_for_generation(elapsed, 0, false, generation);
    }
    pub fn record_script_for_generation(
        &self,
        elapsed: Duration,
        frame_seq: u64,
        rescan: bool,
        generation: u64,
    ) {
        // Keep the generation guard and metric update under one lock. A
        // callback can finish while a new run is being attached; releasing the
        // guard before incrementing counters would let that stale callback
        // pollute the new run's metrics after its reset.
        let slot = self.script.lock().unwrap();
        if slot.generation != generation {
            return;
        }
        let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.scripted.fetch_add(1, Ordering::Relaxed);
        self.script_source_seq.store(frame_seq, Ordering::Relaxed);
        if rescan {
            self.script_rescans.fetch_add(1, Ordering::Relaxed);
        }
        let mut window = self.window.lock().unwrap();
        push_time(&mut window.scripted);
        if window.script_ms.len() >= 256 {
            window.script_ms.pop_front();
        }
        window.script_ms.push_back(ns);
    }
    pub fn record_packet_error(&self, error: impl Into<String>) {
        self.video_packet_errors.fetch_add(1, Ordering::Relaxed);
        *self.last_video_error.lock().unwrap() = Some(error.into());
    }
    pub fn record_decode_error(&self, error: impl Into<String>) {
        self.video_decode_errors.fetch_add(1, Ordering::Relaxed);
        *self.last_video_error.lock().unwrap() = Some(error.into());
    }
    fn record_input(&self, elapsed: Duration, success: bool) {
        self.input_batches.fetch_add(1, Ordering::Relaxed);
        if !success {
            self.input_failures.fetch_add(1, Ordering::Relaxed);
        }
        self.last_input_us.store(
            elapsed.as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    fn record_input_rejected(&self) {
        self.input_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_dimension_change(&self) {
        self.video_dimension_changes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_preview_dropped(&self, count: u64) {
        self.preview_dropped.fetch_add(count, Ordering::Relaxed);
    }
    pub async fn attach_script(&self, sender: watch::Sender<Option<Arc<VideoFrame>>>) {
        let generation = {
            let slot = self.script.lock().unwrap();
            if slot.active && slot.sender.is_none() {
                slot.generation
            } else {
                drop(slot);
                self.begin_script()
            }
        };
        let _ = self.attach_script_for_generation(sender, generation);
    }
    pub fn attach_script_for_generation(
        &self,
        sender: watch::Sender<Option<Arc<VideoFrame>>>,
        generation: u64,
    ) -> bool {
        let mut slot = self.script.lock().unwrap();
        if slot.generation != generation {
            return false;
        }
        self.scripted.store(0, Ordering::Relaxed);
        self.script_published.store(0, Ordering::Relaxed);
        self.script_rescans.store(0, Ordering::Relaxed);
        self.script_source_seq.store(0, Ordering::Relaxed);
        {
            let mut window = self.window.lock().unwrap();
            window.scripted.clear();
            window.script_ms.clear();
        }
        slot.last_sent = Instant::now() - Duration::from_secs(1);
        // Install the sender before reading latest. If a decoder publishes in
        // between, publish() takes the same slot lock first and replaces the
        // value after this initial injection.
        slot.sender = Some(sender.clone());
        slot.active = true;
        self.script_demand.store(true, Ordering::Release);
        if let Some(frame) = self.latest.borrow().clone() {
            self.script_published.fetch_add(1, Ordering::Relaxed);
            sender.send_replace(Some(frame));
        }
        drop(slot);
        self.mark_active();
        true
    }
    pub async fn detach_script(&self) {
        self.detach_script_now();
    }
    pub fn detach_script_now(&self) {
        let mut slot = self.script.lock().unwrap();
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.sender = None;
        slot.active = false;
        self.script_demand.store(false, Ordering::Release);
        drop(slot);
        self.refresh_activity();
    }
    pub fn detach_script_if(&self, generation: u64) -> bool {
        let mut slot = self.script.lock().unwrap();
        if slot.generation != generation {
            return false;
        }
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.sender = None;
        slot.active = false;
        self.script_demand.store(false, Ordering::Release);
        drop(slot);
        self.refresh_activity();
        true
    }
    fn publish(&self, frame: Arc<VideoFrame>) {
        let started = Instant::now();
        self.decoded.fetch_add(1, Ordering::Relaxed);
        push_time(&mut self.window.lock().unwrap().decoded);
        // Keep the script-slot lock before the latest-frame watch lock. Attach
        // uses the same order so a publish racing a new run cannot deadlock or
        // put an older initial frame after a newer one.
        {
            let mut slot = self.script.lock().unwrap();
            let send_script = if slot.sender.is_some() {
                let profile = *self.script_profile.lock().unwrap();
                let interval = match profile {
                    PerformanceProfile::Realtime => Duration::ZERO,
                    PerformanceProfile::Balanced => Duration::from_millis(33),
                    PerformanceProfile::Eco => Duration::from_millis(150),
                    PerformanceProfile::Auto => {
                        let (sum, count) = self
                            .window
                            .lock()
                            .unwrap()
                            .script_ms
                            .iter()
                            .rev()
                            .take(32)
                            .fold((0u64, 0u64), |(sum, count), value| {
                                (sum.saturating_add(*value), count + 1)
                            });
                        let average = sum.checked_div(count).unwrap_or_default();
                        if average < 20_000_000 {
                            Duration::from_millis(16)
                        } else if average < 35_000_000 {
                            Duration::from_millis(33)
                        } else {
                            Duration::from_millis(50)
                        }
                    }
                };
                slot.last_sent.elapsed() >= interval
            } else {
                false
            };
            if send_script {
                slot.last_sent = Instant::now();
                self.script_published.fetch_add(1, Ordering::Relaxed);
            }
            self.latest.send_replace(Some(frame.clone()));
            // A script always receives the newest decoded frame. Replacing the
            // slot is non-blocking, so slow image processing can never build a
            // latency queue. Sending while the slot lock is held also prevents
            // a detached run from receiving a late frame.
            if send_script {
                if let Some(tx) = slot.sender.as_ref() {
                    tx.send_replace(Some(frame.clone()));
                }
            }
        }
        let mode = *self.mode.lock().unwrap();
        let profile = *self.preview_profile.lock().unwrap();
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
        let emit = if self.preview.receiver_count() == 0 {
            false
        } else if let Some(interval) = interval {
            let mut last = self.last_preview.lock().unwrap();
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
            if self.preview.send(frame).is_ok() {
                self.previewed.fetch_add(1, Ordering::Relaxed);
                push_time(&mut self.window.lock().unwrap().previewed);
            } else {
                self.preview_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_publish_us.store(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
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
    input: Arc<Mutex<Option<mpsc::Sender<InputBatch>>>>,
    server: Arc<Mutex<Option<Child>>>,
    forward_ports: [u16; 2],
    alive: Arc<std::sync::atomic::AtomicBool>,
    cleanup_started: Arc<std::sync::atomic::AtomicBool>,
}
struct InputBatch {
    messages: Vec<(Vec<u8>, Duration)>,
    done: oneshot::Sender<Result<()>>,
}

enum DecoderPacket {
    Session {
        width: u32,
        height: u32,
    },
    Media {
        data: Vec<u8>,
        pts_us: i64,
        keyframe: bool,
    },
}

type ResumeKeyframeStore = Arc<std::sync::Mutex<Option<(Arc<Vec<u8>>, i64)>>>;

/// Owns resources acquired during session startup until the session has been
/// fully constructed. Every fallible startup stage can therefore be rolled
/// back without leaking an adb forward or a server process.
struct SessionStartupGuard {
    adb: Adb,
    serial: String,
    ports: Vec<u16>,
    server: Option<Child>,
    committed: bool,
}

impl SessionStartupGuard {
    fn new(adb: Adb, serial: String) -> Self {
        Self {
            adb,
            serial,
            ports: Vec::with_capacity(2),
            server: None,
            committed: false,
        }
    }

    fn track_port(&mut self, port: u16) {
        if !self.ports.contains(&port) {
            self.ports.push(port);
        }
    }

    async fn rollback(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), server.kill()).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
        }
        for port in self.ports.drain(..) {
            let port_arg = format!("tcp:{port}");
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                self.adb
                    .output(&["-s", &self.serial, "forward", "--remove", &port_arg]),
            )
            .await;
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SessionStartupGuard {
    fn drop(&mut self) {
        if self.committed || (self.server.is_none() && self.ports.is_empty()) {
            return;
        }
        let Some(runtime) = tokio::runtime::Handle::try_current().ok() else {
            if let Some(mut server) = self.server.take() {
                let _ = server.start_kill();
            }
            tracing::error!(serial = %self.serial, "session startup rollback could not schedule forward cleanup");
            return;
        };
        let adb = self.adb.clone();
        let serial = self.serial.clone();
        let ports = std::mem::take(&mut self.ports);
        let mut server = self.server.take();
        runtime.spawn(async move {
            if let Some(mut child) = server.take() {
                let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
            for port in ports {
                let port_arg = format!("tcp:{port}");
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    adb.output(&["-s", &serial, "forward", "--remove", &port_arg]),
                )
                .await;
            }
            tracing::debug!(%serial, "cancelled session startup rolled back");
        });
    }
}

impl ScrcpySession {
    pub async fn connect(serial: String, options: SessionOptions) -> Result<Self> {
        let adb = Adb::default();
        let mut last_error = None;
        for attempt in 0..3 {
            let mut guard = SessionStartupGuard::new(adb.clone(), serial.clone());
            match Self::connect_inner(&serial, &options, &adb, &mut guard).await {
                Ok(session) => {
                    guard.commit();
                    return Ok(session);
                }
                Err(error) => {
                    guard.rollback().await;
                    last_error = Some(error);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
                    }
                }
            }
        }
        Err(last_error.expect("session startup attempts always produce an error"))
    }

    async fn connect_inner(
        serial: &str,
        options: &SessionOptions,
        adb: &Adb,
        guard: &mut SessionStartupGuard,
    ) -> Result<Self> {
        options.validate()?;
        if !options.server_jar.exists() {
            bail!(
                "scrcpy-server v4.0 not found at {}",
                options.server_jar.display()
            )
        }
        let jar = options.server_jar.to_string_lossy();
        adb.output(["-s", serial, "push", &jar, REMOTE_SERVER].as_slice())
            .await?;
        let scid: u32 = rand::rng().random_range(1..=0x7fff_ffff);
        let scid_hex = format!("{scid:08x}");
        let [video_port, control_port] = free_port_pair()?;
        let socket = format!("localabstract:scrcpy_{scid_hex}");
        for port in [video_port, control_port] {
            let local = format!("tcp:{port}");
            adb.output(["-s", serial, "forward", &local, &socket].as_slice())
                .await?;
            guard.track_port(port);
        }
        let codec_name = match options.codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
        };
        let mut args = vec![
            "-s".to_string(),
            serial.to_owned(),
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
        if let Some(encoder) = options.encoder.as_deref() {
            args.push(format!("video_encoder={encoder}"));
        }
        let mut server =
            Command::new(std::env::var("SCRCPYFORGE_ADB").unwrap_or_else(|_| "adb".into()))
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
        if let Some(stderr) = server.stderr.take() {
            spawn_server_stderr_collector(serial.to_owned(), stderr);
        }
        guard.server = Some(server);
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
        validate_video_dimensions(width, height)?;
        let frames = Arc::new(FrameHub::new());
        let size = Arc::new(RwLock::new((width, height)));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let cleanup_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A static device screen may not produce media packets, so relying on
        // the reader/decoder hot path to advance IdleGrace would leave an idle
        // session warm forever. This tiny supervisor tick is the only periodic
        // work retained for the logical session and stops with the session.
        let activity_frames = frames.clone();
        let activity_alive = alive.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if !activity_alive.load(Ordering::Acquire) {
                    break;
                }
                activity_frames.activity();
            }
        });
        let (packet_tx, mut packet_rx) = mpsc::channel::<DecoderPacket>(8);
        let decode_frames = frames.clone();
        let decode_size = size.clone();
        let decode_alive = alive.clone();
        std::thread::Builder::new()
            .name(format!("forge-decode-{serial}"))
            .spawn(move || {
                // Do not initialize FFmpeg until a script or preview asks for
                // frames. This avoids allocating codec state for an idle
                // session; packets skipped by the reader while suspended
                // never enter the decoder queue.
                let mut decoder: Option<FfmpegDecoder> = None;
                let mut frame_seq = 0u64;
                let mut observed_demand_epoch = decode_frames.demand_epoch();
                let mut awaiting_keyframe = true;
                let mut decoder_size = (width, height);
                while let Some(packet) = packet_rx.blocking_recv() {
                    let (data, pts, keyframe) = match packet {
                        DecoderPacket::Session { width, height } => {
                            decoder_size = (width, height);
                            decoder = None;
                            awaiting_keyframe = true;
                            continue;
                        }
                        DecoderPacket::Media {
                            data,
                            pts_us,
                            keyframe,
                        } => (data, pts_us, keyframe),
                    };
                    if !decode_frames.video_demand() {
                        decoder = None;
                        awaiting_keyframe = true;
                        observed_demand_epoch = decode_frames.demand_epoch();
                        continue;
                    }
                    let demand_epoch = decode_frames.demand_epoch();
                    if demand_epoch != observed_demand_epoch {
                        // Encoded delta frames were discarded while demand was
                        // suspended. Restart the codec and wait for the next
                        // server keyframe instead of feeding it an invalid
                        // reference chain.
                        decoder = None;
                        awaiting_keyframe = true;
                        observed_demand_epoch = demand_epoch;
                    }
                    if awaiting_keyframe && !keyframe {
                        continue;
                    }
                    if decoder.is_none() {
                        match FfmpegDecoder::new(actual_codec, decoder_size.0, decoder_size.1) {
                            Ok(value) => decoder = Some(value),
                            Err(error) => {
                                decode_frames.record_decode_error(error.to_string());
                                tracing::error!(%error,"decoder init failed");
                                decode_alive.store(false, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                    match decoder
                        .as_mut()
                        .expect("decoder initialized for active demand")
                        .decode(&data, pts)
                    {
                        Ok(decoded) => {
                            awaiting_keyframe = false;
                            for frame in decoded {
                                frame_seq = frame_seq.wrapping_add(1);
                                let frame = frame.with_sequence(frame_seq);
                                if !decode_frames.video_demand() {
                                    continue;
                                }
                                let mut size = decode_size.blocking_write();
                                if *size != (frame.width, frame.height) {
                                    *size = (frame.width, frame.height);
                                    decode_frames.record_dimension_change();
                                }
                                // FrameHub::publish only performs bounded,
                                // synchronous slot swaps and never waits for
                                // Lua, preview, or input work.
                                decode_frames.publish(Arc::new(frame));
                            }
                        }
                        Err(error) => {
                            decoder = None;
                            awaiting_keyframe = true;
                            decode_frames.record_decode_error(error.to_string());
                            tracing::debug!(%error,"packet decode failed");
                        }
                    }
                }
                decode_alive.store(false, Ordering::Relaxed);
            })?;
        let reader_size = size.clone();
        let reader_alive = alive.clone();
        let reader_frames = frames.clone();
        let resume_keyframe: ResumeKeyframeStore = Arc::new(std::sync::Mutex::new(None));
        let reader_demand = Arc::new(AtomicBool::new(reader_frames.video_demand()));
        // A static screen can stop producing packets while the session is
        // suspended. Keep demand transitions independent from the socket read
        // future so a cached keyframe can wake the decoder without cancelling
        // a partially-read packet.
        let resume_frames = reader_frames.clone();
        let resume_alive = reader_alive.clone();
        let resume_notify = reader_frames.demand_notify();
        let resume_sender = packet_tx.clone();
        let resume_demand = reader_demand.clone();
        let resume_anchor_for_watcher = resume_keyframe.clone();
        let resume_anchor_for_reader = resume_keyframe.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = resume_notify.notified() => {},
                    _ = tick.tick() => {},
                }
                if !resume_alive.load(Ordering::Acquire) {
                    break;
                }
                let wants_video = resume_frames.video_demand();
                let was_demanded = resume_demand.swap(wants_video, Ordering::AcqRel);
                if wants_video && !was_demanded {
                    let anchor = resume_anchor_for_watcher.lock().unwrap().take();
                    if let Some((data, pts_us)) = anchor {
                        if resume_sender
                            .send(DecoderPacket::Media {
                                data: data.as_ref().clone(),
                                pts_us,
                                keyframe: true,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            loop {
                if !reader_alive.load(Ordering::Acquire) {
                    break;
                }
                match reader.read(&mut video, actual_codec).await {
                    Ok(StreamPacket::Session { width, height, .. }) => {
                        if let Err(error) = validate_video_dimensions(width, height) {
                            reader_frames.record_packet_error(error.to_string());
                            tracing::warn!(%error, "invalid video dimensions");
                            break;
                        }
                        let mut size = reader_size.write().await;
                        if *size != (width, height) {
                            *size = (width, height);
                            reader_frames.record_dimension_change();
                        }
                        resume_anchor_for_reader.lock().unwrap().take();
                        reader_demand.store(reader_frames.video_demand(), Ordering::Release);
                        if packet_tx
                            .send(DecoderPacket::Session { width, height })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(StreamPacket::Media {
                        pts_us,
                        keyframe,
                        data,
                        ..
                    }) => {
                        if keyframe {
                            if let Some(anchor) = reader.last_keyframe() {
                                *resume_anchor_for_reader.lock().unwrap() = Some(anchor);
                            }
                        }
                        let wants_video = reader_frames.video_demand();
                        let was_demanded = reader_demand.swap(wants_video, Ordering::AcqRel);
                        if wants_video && !was_demanded && !keyframe {
                            // The server is allowed to keep sending deltas
                            // without a frequent IDR. Seed the restarted
                            // decoder from the most recent complete keyframe
                            // captured while the session was suspended.
                            if let Some((data, pts_us)) = reader.take_last_keyframe() {
                                resume_anchor_for_reader.lock().unwrap().take();
                                if packet_tx
                                    .send(DecoderPacket::Media {
                                        data,
                                        pts_us,
                                        keyframe: true,
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        // Reading and discarding encoded packets while there
                        // is no consumer keeps the socket recoverable without
                        // waking FFmpeg or filling the decode queue.
                        if !wants_video {
                            continue;
                        }
                        if packet_tx
                            .send(DecoderPacket::Media {
                                data,
                                pts_us: pts_us.unwrap_or(0),
                                keyframe,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        reader_frames.record_packet_error(error.to_string());
                        tracing::warn!(%error,"video stream ended");
                        break;
                    }
                }
            }
            reader_alive.store(false, Ordering::Relaxed);
        });
        let (input_tx, mut input_rx) = mpsc::channel::<InputBatch>(16);
        let input_alive = alive.clone();
        let input_frames = frames.clone();
        tokio::spawn(async move {
            let mut control = control;
            while let Some(batch) = input_rx.recv().await {
                let started = Instant::now();
                let result = async {
                    for (message, delay) in batch.messages {
                        if !input_alive.load(Ordering::Acquire) {
                            bail!("session is stopped")
                        }
                        control.write_all(&message).await?;
                        let mut remaining = delay;
                        while !remaining.is_zero() {
                            if !input_alive.load(Ordering::Acquire) {
                                bail!("session is stopped")
                            }
                            let step = remaining.min(Duration::from_millis(50));
                            tokio::time::sleep(step).await;
                            remaining = remaining.saturating_sub(step);
                        }
                    }
                    // A batch is not complete until bytes are flushed to the
                    // control socket. Reporting success before flush could
                    // let a failed tap look successful to the script.
                    control.flush().await?;
                    Ok(())
                }
                .await;
                let failed = result.is_err();
                input_frames.record_input(started.elapsed(), !failed);
                let _ = batch.done.send(result);
                if failed {
                    input_alive.store(false, Ordering::Relaxed);
                    break;
                }
            }
            let _ = control.shutdown().await;
        });
        let server = guard.server.take().context("session server missing")?;
        let input = Arc::new(Mutex::new(Some(input_tx)));
        let server = Arc::new(Mutex::new(Some(server)));
        let session = Self {
            serial: serial.to_owned(),
            device_name,
            codec: actual_codec,
            size,
            frames,
            input,
            server,
            forward_ports: [video_port, control_port],
            alive,
            cleanup_started,
        };
        session.install_idle_shutdown_hook();
        Ok(session)
    }

    fn install_idle_shutdown_hook(&self) {
        let alive = self.alive.clone();
        let input = self.input.clone();
        let server = self.server.clone();
        let cleanup_started = self.cleanup_started.clone();
        let serial = self.serial.clone();
        let ports = self.forward_ports;
        let runtime = tokio::runtime::Handle::current();
        self.frames.set_idle_shutdown_hook(Arc::new(move || {
            if !alive.swap(false, Ordering::AcqRel) || cleanup_started.swap(true, Ordering::AcqRel)
            {
                return;
            }
            let input = input.clone();
            let server = server.clone();
            let serial = serial.clone();
            runtime.spawn(async move {
                input.lock().await.take();
                let child = server.lock().await.take();
                cleanup_process_and_forwards(serial, ports, child).await;
            });
        }));
    }

    async fn send_input(&self, messages: Vec<(Vec<u8>, Duration)>) -> Result<()> {
        if !self.is_alive() {
            bail!("session is stopped")
        }
        if messages.is_empty() || messages.len() > MAX_SWIPE_STEPS as usize + 2 {
            bail!("input batch contains too many messages")
        }
        let total_bytes = messages
            .iter()
            .try_fold(0usize, |total, (bytes, _)| total.checked_add(bytes.len()));
        if total_bytes.is_none_or(|bytes| bytes > 256 * 1024) {
            bail!("input batch is too large")
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
        if let Err(error) = sender.try_send(InputBatch { messages, done: tx }) {
            self.frames.record_input_rejected();
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => anyhow::anyhow!("input queue busy"),
                mpsc::error::TrySendError::Closed(_) => anyhow::anyhow!("input writer stopped"),
            });
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.frames.record_input_rejected();
                bail!("input writer stopped")
            }
            Err(_) => {
                self.frames.record_input_rejected();
                bail!("input write timed out")
            }
        }
    }
    pub async fn tap(&self, x: i32, y: i32) -> Result<()> {
        let (w, h) = *self.size.read().await;
        validate_point(x, y, w, h)?;
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
        validate_point(x, y, w, h)?;
        if radius > MAX_TAP_RADIUS {
            bail!("tap radius must be at most {MAX_TAP_RADIUS} pixels")
        }
        let (tx, ty) = random_point(x, y, radius);
        self.tap(
            tx.clamp(0, w.saturating_sub(1) as i32),
            ty.clamp(0, h.saturating_sub(1) as i32),
        )
        .await
    }
    pub async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, duration_ms: u64) -> Result<()> {
        let (w, h) = *self.size.read().await;
        if !(1..=MAX_SWIPE_DURATION_MS).contains(&duration_ms) {
            bail!("swipe duration_ms must be between 1 and {MAX_SWIPE_DURATION_MS}")
        }
        validate_point(x1, y1, w, h)?;
        validate_point(x2, y2, w, h)?;
        let steps = (duration_ms / 16).clamp(2, MAX_SWIPE_STEPS);
        let delay = Duration::from_millis(duration_ms / (steps - 1));
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
        if !(1..=MAX_SWIPE_DURATION_MS).contains(&duration_ms) {
            bail!("long_press duration_ms must be between 1 and {MAX_SWIPE_DURATION_MS}")
        }
        validate_point(x, y, w, h)?;
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
        if points.is_empty() || points.len() > MAX_MULTI_TOUCH {
            bail!("multi_tap requires between 1 and {MAX_MULTI_TOUCH} points")
        }
        for &(x, y) in points {
            validate_point(x, y, w, h)?;
        }
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
        if code > 0xffff {
            bail!("key code must be between 0 and 65535")
        }
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
        if value.len() > 4 * 1024 {
            bail!("text must be at most 4096 bytes")
        }
        self.send_input(vec![(control::text(value), Duration::ZERO)])
            .await
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.alive.store(false, Ordering::Relaxed);
        self.input.lock().await.take();
        self.frames.detach_script_now();
        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let child = self.server.lock().await.take();
        cleanup_process_and_forwards(self.serial.clone(), self.forward_ports, child).await;
        Ok(())
    }
}

async fn cleanup_process_and_forwards(serial: String, ports: [u16; 2], mut server: Option<Child>) {
    if let Some(mut child) = server.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    let adb = Adb::default();
    for port in ports {
        let port_arg = format!("tcp:{port}");
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            adb.output(&["-s", &serial, "forward", "--remove", &port_arg]),
        )
        .await;
    }
}

fn spawn_server_stderr_collector(serial: String, mut stderr: tokio::process::ChildStderr) {
    const MAX_SERVER_STDERR: usize = 64 * 1024;
    tokio::spawn(async move {
        let mut ring = VecDeque::with_capacity(MAX_SERVER_STDERR);
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) => break,
                Ok(count) => {
                    if count >= MAX_SERVER_STDERR {
                        ring.clear();
                        ring.extend(buffer[count - MAX_SERVER_STDERR..count].iter().copied());
                    } else {
                        ring.extend(buffer[..count].iter().copied());
                        while ring.len() > MAX_SERVER_STDERR {
                            ring.pop_front();
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, %serial, "scrcpy server stderr reader stopped");
                    break;
                }
            }
        }
        if !ring.is_empty() {
            let diagnostic = String::from_utf8_lossy(ring.make_contiguous())
                .trim()
                .to_owned();
            if !diagnostic.is_empty() {
                tracing::debug!(serial = %serial, stderr = %diagnostic, "scrcpy server stderr");
            }
        }
    });
}

fn validate_point(x: i32, y: i32, width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > u16::MAX as u32
        || height > u16::MAX as u32
        || x < 0
        || y < 0
        || x >= width as i32
        || y >= height as i32
    {
        bail!("input coordinates are outside the device screen")
    }
    Ok(())
}

fn validate_video_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || width > MAX_VIDEO_DIMENSION
        || height > MAX_VIDEO_DIMENSION
    {
        bail!("video dimensions are outside the supported range")
    }
    Ok(())
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
mod frame_hub_tests {
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

fn free_port_pair() -> Result<[u16; 2]> {
    // Bind both sockets before releasing either one. This guarantees distinct
    // reservations within this process and narrows the window before adb
    // claims the ports; connect() retries the complete transaction if another
    // process wins the remaining OS-level race.
    let first = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let first_port = first.local_addr()?.port();
    let second = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let second_port = second.local_addr()?.port();
    if first_port == second_port {
        bail!("OS returned duplicate local ports")
    }
    Ok([first_port, second_port])
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
        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut input) = self.input.try_lock() {
            input.take();
        }
        let child = self
            .server
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take());
        let serial = self.serial.clone();
        let ports = self.forward_ports;
        let cleanup = async move { cleanup_process_and_forwards(serial, ports, child).await };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(cleanup);
        } else {
            std::thread::Builder::new()
                .name("forge-session-cleanup".into())
                .spawn(move || {
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        tracing::error!("could not create cleanup runtime for dropped session");
                        return;
                    };
                    runtime.block_on(cleanup);
                })
                .ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_frame_is_injected_and_stale_generation_is_ignored() {
        let hub = FrameHub::new();
        let first_generation = hub.begin_script();
        hub.publish(Arc::new(
            VideoFrame::new_i420(2, 2, vec![128; 6], 0).with_sequence(7),
        ));

        let (first_sender, mut first_receiver) = watch::channel::<Option<Arc<VideoFrame>>>(None);
        assert!(hub.attach_script_for_generation(first_sender, first_generation));
        assert_eq!(
            first_receiver
                .borrow_and_update()
                .as_ref()
                .map(|frame| frame.frame_seq),
            Some(7)
        );
        hub.record_script_for_generation(Duration::from_millis(1), 7, false, first_generation);
        assert_eq!(hub.metrics_now().script_frames, 1);

        let second_generation = hub.begin_script();
        let (second_sender, _second_receiver) = watch::channel::<Option<Arc<VideoFrame>>>(None);
        assert!(hub.attach_script_for_generation(second_sender, second_generation));
        hub.record_script_for_generation(Duration::from_millis(1), 7, false, first_generation);
        assert_eq!(hub.metrics_now().script_frames, 0);
        hub.record_script_for_generation(Duration::from_millis(1), 7, true, second_generation);
        let metrics = hub.metrics_now();
        assert_eq!(metrics.script_frames, 1);
        assert_eq!(metrics.script_rescans, 1);
    }

    #[test]
    fn new_frame_hub_defaults_to_five_second_preview() {
        assert_eq!(FrameHub::new().preview_mode_now(), PreviewMode::FiveSeconds);
    }

    #[test]
    fn preview_lease_controls_activity_and_video_demand() {
        let hub = Arc::new(FrameHub::new());
        assert!(!hub.video_demand());
        let lease = hub.acquire_preview();
        assert_eq!(hub.preview_leases(), 1);
        assert_eq!(hub.activity().0, SessionActivityState::Active);
        assert!(hub.video_demand());
        drop(lease);
        assert_eq!(hub.preview_leases(), 0);
        assert_eq!(hub.activity().0, SessionActivityState::IdleGrace);
        assert!(hub.video_demand());
    }

    #[test]
    fn disabled_preview_lease_does_not_keep_decoder_active() {
        let hub = Arc::new(FrameHub::new());
        hub.set_preview_mode_now(PreviewMode::Off);
        let lease = hub.acquire_preview();
        assert_eq!(hub.preview_leases(), 1);
        assert_eq!(hub.activity().0, SessionActivityState::Suspended);
        assert!(!hub.video_demand());
        drop(lease);
    }

    #[tokio::test]
    async fn frame_request_wakes_decoder_when_preview_is_off() {
        let hub = Arc::new(FrameHub::new());
        hub.set_preview_mode_now(PreviewMode::Off);
        let notify = hub.demand_notify();
        let wake = notify.notified();
        let lease = hub.acquire_frame_request();
        assert!(tokio::time::timeout(Duration::from_millis(50), wake)
            .await
            .is_ok());
        assert_eq!(hub.frame_requests(), 1);
        assert_eq!(hub.activity().0, SessionActivityState::Active);
        assert!(hub.video_demand());
        drop(lease);
        assert_eq!(hub.frame_requests(), 0);
        assert_eq!(hub.activity().0, SessionActivityState::IdleGrace);
    }
}
