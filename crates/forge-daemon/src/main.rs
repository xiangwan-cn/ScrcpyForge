use std::{
    collections::{BTreeMap, HashMap, HashSet},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path, Request, State,
    },
    http::{header, HeaderMap, StatusCode},
    middleware::{from_fn, from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use forge_core::{lua, DeviceInfo, DeviceManager, InputAction, RunScriptRequest};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Semaphore};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

const MAX_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_SCRIPT_NAME_BYTES: usize = 128;
const MAX_SCRIPT_COUNT: usize = 1024;
const MAX_REQUEST_BYTES: usize = 512 * 1024;

#[derive(Clone)]
struct AppState {
    manager: DeviceManager,
    scripts: Arc<Mutex<HashMap<Uuid, ActiveRun>>>,
    script_catalog: Arc<Mutex<ScriptCatalog>>,
    // A manual stop suppresses the default auto-start policy until the
    // device disappears from a scan. An explicit start clears the entry.
    session_autostart_blocked: Arc<Mutex<HashSet<String>>>,
    shutdown: tokio::sync::broadcast::Sender<()>,
    auth_token: Option<Arc<str>>,
    api_gate: Arc<Semaphore>,
    ws_gate: Arc<Semaphore>,
}
#[derive(Default)]
struct ScriptCatalog {
    refreshed: Option<Instant>,
    names: Vec<String>,
}
struct ActiveRun {
    run: lua::LuaRun,
    serial: String,
    name: Option<String>,
    generation: u64,
    finished_at: Option<Instant>,
}

#[derive(Clone, Serialize)]
struct ScriptRunStatus {
    run_id: Uuid,
    serial: String,
    name: Option<String>,
    generation: u64,
    running: bool,
    stalled: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct SessionStateSnapshot {
    serial: String,
    activity_state: forge_core::session::SessionActivityState,
    script_active: bool,
    script_generation: u64,
    profile: forge_core::session::PerformanceProfile,
    preview_profile: forge_core::session::PerformanceProfile,
    preview_mode: forge_core::session::PreviewMode,
}

#[derive(Serialize)]
struct StateSnapshot {
    devices: Vec<DeviceInfo>,
    sessions: Vec<SessionStateSnapshot>,
    runs: Vec<ScriptRunStatus>,
    scripts: Vec<String>,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
}

#[derive(Deserialize)]
struct ConnectRequest {
    endpoint: String,
}

#[derive(Deserialize)]
struct PairRequest {
    endpoint: String,
    code: String,
}

#[derive(Serialize)]
struct RunResponse {
    run_id: Uuid,
}
#[derive(Deserialize)]
struct StartSessionRequest {
    codec: Option<String>,
    max_size: Option<u32>,
    bit_rate: Option<u32>,
    max_fps: Option<u32>,
    profile: Option<forge_core::session::PerformanceProfile>,
}
#[derive(Deserialize)]
struct PreviewRequest {
    mode: String,
}
#[derive(Deserialize)]
struct ProfileRequest {
    profile: forge_core::session::PerformanceProfile,
}
#[derive(Deserialize)]
struct RunNamedRequest {
    serial: String,
    name: String,
}
#[derive(Deserialize)]
struct RunAllRequest {
    name: String,
}
#[derive(Deserialize)]
struct RegionRequest {
    name: Option<String>,
    path: Option<String>,
    x1: u32,
    y1: u32,
    x2: u32,
    y2: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "forge_daemon=info,tower_http=info".into()),
        )
        .init();
    tokio::fs::create_dir_all(scripts_dir()).await?;
    tokio::fs::create_dir_all(templates_dir()).await?;
    let (shutdown, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let addr = std::env::var("SCRCPYFORGE_ADDR").unwrap_or_else(|_| "0.0.0.0:27180".into());
    let socket_addr: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("SCRCPYFORGE_ADDR must be a socket address: {addr}"))?;
    let auth_token = std::env::var("SCRCPYFORGE_AUTH_TOKEN")
        .ok()
        .filter(|value| {
            !value.trim().is_empty()
                && value.len() >= 16
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .map(Arc::<str>::from);
    if std::env::var_os("SCRCPYFORGE_AUTH_TOKEN").is_some() && auth_token.is_none() {
        anyhow::bail!("SCRCPYFORGE_AUTH_TOKEN must be at least 16 ASCII graphic bytes");
    }
    if !socket_addr.ip().is_loopback() && auth_token.is_none() {
        tracing::warn!(
            %addr,
            "daemon is reachable from the LAN without a bearer token; set SCRCPYFORGE_AUTH_TOKEN to protect API routes"
        );
    }
    let app_state = AppState {
        manager: DeviceManager::new(),
        scripts: Default::default(),
        script_catalog: Default::default(),
        session_autostart_blocked: Default::default(),
        shutdown,
        auth_token,
        api_gate: Arc::new(Semaphore::new(128)),
        ws_gate: Arc::new(Semaphore::new(64)),
    };
    spawn_polling(
        app_state.manager.clone(),
        app_state.session_autostart_blocked.clone(),
        app_state.shutdown.subscribe(),
    );
    spawn_script_reaper(app_state.scripts.clone(), app_state.shutdown.subscribe());
    let app = Router::new()
        .route("/", get(index))
        .route("/api/v1/health", get(health))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/shutdown", post(request_shutdown))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/state", get(state))
        .route("/api/v1/metrics", get(metrics))
        .route("/api/v1/devices/scan", post(scan))
        .route("/api/v1/devices/connect", post(connect))
        .route("/api/v1/devices/pairing-services", get(pairing_services))
        .route("/api/v1/devices/pair", post(pair))
        .route("/api/v1/devices/{serial}/screenshot", get(screenshot))
        .route("/api/v1/devices/{serial}/input", post(input))
        .route("/api/v1/sessions/{serial}/start", post(start_session))
        .route("/api/v1/sessions/{serial}/stop", post(stop_session))
        .route("/api/v1/sessions/start-all", post(start_all_sessions))
        .route("/api/v1/sessions/stop-all", post(stop_all_sessions))
        .route("/api/v1/sessions", get(list_sessions))
        .route("/api/v1/sessions/{serial}/preview-mode", post(preview_mode))
        .route("/api/v1/sessions/{serial}/profile", post(set_profile))
        .route(
            "/api/v1/sessions/{serial}/script-profile",
            post(set_script_profile),
        )
        .route(
            "/api/v1/sessions/{serial}/preview-profile",
            post(set_preview_profile),
        )
        .route("/api/v1/sessions/{serial}/metrics", get(session_metrics))
        .route("/api/v1/sessions/{serial}/frame.jpg", get(latest_frame))
        .route("/api/v1/sessions/{serial}/preview", get(preview_stream))
        .route("/api/v1/sessions/{serial}/regions", post(save_region))
        .route("/api/v1/scripts", get(list_scripts))
        .route("/api/v1/scripts/runs", get(list_script_runs))
        .route("/api/v1/scripts/run-named", post(run_named))
        .route("/api/v1/scripts/run-all", post(run_all))
        .route("/api/v1/scripts/stop-all", post(stop_all_scripts))
        .route("/api/v1/scripts/run", post(run_script))
        .route("/api/v1/scripts/{run_id}/stop", post(stop_script))
        .route(
            "/api/v1/scripts/devices/{serial}/stop",
            post(stop_device_script),
        )
        .route("/api/v1/events", get(events))
        // Same-origin browser access needs no CORS response. Omitting a
        // wildcard layer prevents arbitrary web pages from calling control
        // endpoints on a local daemon.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(from_fn(request_size_limit))
        .layer(from_fn_with_state(app_state.clone(), auth_middleware))
        .layer(from_fn(request_id))
        .layer(from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(app_state.clone());
    let listener = tokio::net::TcpListener::bind(socket_addr).await?;
    tracing::info!(%addr, "ScrcpyForge backend listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await?;
    let active: Vec<_> = app_state
        .scripts
        .lock()
        .await
        .drain()
        .map(|(_, item)| item)
        .collect();
    for item in active {
        item.run.cancel();
    }
    app_state.manager.stop_all_sessions().await;
    Ok(())
}

async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let public = matches!(request.uri().path(), "/" | "/api/v1/health");
    if !public {
        if let Some(expected) = state.auth_token.as_deref() {
            let supplied = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));
            if supplied != Some(expected) {
                let mut response = json_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_required",
                    "authentication required",
                );
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    header::HeaderValue::from_static("Bearer"),
                );
                return response;
            }
        }
        // Without a bearer token, same-origin browser requests remain
        // convenient while browser requests initiated by an unrelated origin
        // are rejected. The Origin header is absent for the desktop client
        // and direct non-browser requests, so those callers remain compatible.
        if state.auth_token.is_none()
            && request.headers().get(header::ORIGIN).is_some_and(|origin| {
                let origin = origin.to_str().ok();
                let host = request
                    .headers()
                    .get(header::HOST)
                    .and_then(|value| value.to_str().ok());
                !origin.is_some_and(|origin| host.is_some_and(|host| same_origin(origin, host)))
            })
        {
            return json_error(
                StatusCode::FORBIDDEN,
                "cross_origin_rejected",
                "cross-origin control request rejected",
            );
        }
    }
    let Ok(_permit) = state.api_gate.clone().try_acquire_owned() else {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many concurrent requests",
        );
    };
    next.run(request).await
}

async fn request_size_limit(request: Request, next: Next) -> Response {
    let too_large = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_REQUEST_BYTES as u64);
    if too_large {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "request body exceeds 512 KiB",
        );
    }
    next.run(request).await
}

fn spawn_polling(
    manager: DeviceManager,
    session_autostart_blocked: Arc<Mutex<HashSet<String>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        // Scan promptly after daemon startup so a device that was already
        // connected does not need a manual refresh before its session starts.
        let mut delay = Duration::from_secs(1);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(delay) => {
                    match manager.scan(true).await {
                        Ok(devices) => {
                            let has_active_device = devices.iter().any(|device| {
                                matches!(device.state, forge_core::DeviceState::Device)
                            });
                            ensure_sessions_for_devices(
                                &manager,
                                &devices,
                                &session_autostart_blocked,
                            )
                            .await;
                            if has_active_device {
                                delay = Duration::from_secs(5);
                            } else {
                                delay = (delay * 2).min(Duration::from_secs(60));
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "device scan failed");
                            delay = (delay * 2).min(Duration::from_secs(60));
                        }
                    }
                    // A deep-idle session can stop itself without a request.
                    // Prune its map entry on the same low-frequency supervisor
                    // tick so the logical session does not retain an Arc
                    // indefinitely after its child and forwards are gone.
                    let _ = manager.sessions().await;
                }
            }
        }
    });
}

async fn ensure_sessions_for_devices(
    manager: &DeviceManager,
    devices: &[DeviceInfo],
    blocked: &Arc<Mutex<HashSet<String>>>,
) {
    let mut active_serials = devices
        .iter()
        .filter(|device| matches!(device.state, forge_core::DeviceState::Device))
        .map(|device| device.serial.clone())
        .collect::<Vec<_>>();
    active_serials.sort();
    active_serials.dedup();

    // A serial can be reused after a disconnect. Drop old manual-stop
    // suppressions as soon as a scan no longer reports the device as ready.
    let blocked_snapshot = {
        let mut guard = blocked.lock().await;
        let active = active_serials.iter().collect::<HashSet<_>>();
        guard.retain(|serial| active.contains(serial));
        guard.clone()
    };
    let targets = active_serials
        .into_iter()
        .filter(|serial| !blocked_snapshot.contains(serial))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return;
    }

    let concurrency = std::env::var("SCRCPYFORGE_SESSION_START_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(1, 8);
    let permits = Arc::new(Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    for serial in targets {
        let permit = match permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let manager = manager.clone();
        let blocked = blocked.clone();
        tasks.spawn(async move {
            let _permit = permit;
            // Re-check after waiting for a start slot so a simultaneous
            // manual stop wins over an auto-start that was queued earlier.
            if blocked.lock().await.contains(&serial) {
                return;
            }
            if manager.session(&serial).await.is_some() {
                return;
            }
            match manager
                .start_session(
                    serial.clone(),
                    forge_core::session::SessionOptions::default(),
                )
                .await
            {
                Ok(_) => tracing::info!(%serial, "automatically started scrcpy session"),
                Err(error) => tracing::warn!(%serial, %error, "automatic session start failed"),
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

fn spawn_script_reaper(
    scripts: Arc<Mutex<HashMap<Uuid, ActiveRun>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tick.tick() => {
                    let mut active_runs = scripts.lock().await;
                    retain_script_runs(&mut active_runs);
                },
            }
        }
    });
}

fn same_origin(origin: &str, host: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https") && authority == host && !authority.is_empty()
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' blob: data:; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        header::HeaderValue::from_static("same-origin"),
    );
    response
}

async fn request_id(mut request: Request, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    request.extensions_mut().insert(id.clone());
    let mut response = next.run(request).await;
    if let Ok(value) = header::HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static("x-request-id"), value);
    }
    if response.status().is_client_error() || response.status().is_server_error() {
        let (parts, body) = response.into_parts();
        match to_bytes(body, 64 * 1024).await {
            Ok(bytes) => {
                if let Ok(mut payload) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let Some(error) = payload
                        .get_mut("error")
                        .and_then(|value| value.as_object_mut())
                    {
                        error.insert("request_id".into(), serde_json::Value::String(id));
                        let encoded =
                            serde_json::to_vec(&payload).unwrap_or_else(|_| bytes.to_vec());
                        let mut parts = parts;
                        parts.headers.remove(header::CONTENT_LENGTH);
                        response = Response::from_parts(parts, Body::from(encoded));
                    } else {
                        response = Response::from_parts(parts, Body::from(bytes));
                    }
                } else {
                    response = Response::from_parts(parts, Body::from(bytes));
                }
            }
            Err(error) => {
                tracing::debug!(%error, "unable to attach request id to error response");
                response = Response::from_parts(parts, Body::empty());
            }
        }
    }
    response
}

fn json_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
async fn capabilities() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "api_version": 1,
        "scrcpy_server": "4.0",
        "codecs": ["h264", "h265", "av1"],
        "frame_storage": "i420",
        "vision_modes": ["color", "gray", "multiscale"],
        "vision_api": ["find_candidates", "roi_rgb", "pixel_yuv"],
        "lua": "5.4",
        "preview": ["websocket_jpeg", "five_seconds", "off"],
        "auto_session_start": true,
        "default_preview_mode": "five_seconds",
        "script_profiles": ["auto", "eco", "balanced", "realtime"],
        "capture_profiles": {
            "eco": {"max_size": 720, "max_fps": 15, "bit_rate": 2000000},
            "balanced": {"max_size": 960, "max_fps": 30, "bit_rate": 4000000},
            "realtime": {"max_size": 1280, "max_fps": 60, "bit_rate": 8000000}
        },
        "demand_aware_preview": true,
        "etag": ["state", "frame"],
        "state_endpoint": "/api/v1/state",
        "metrics_endpoint": "/api/v1/metrics",
        "wireless_pairing": true
    }))
}
async fn request_shutdown(State(s): State<AppState>) -> StatusCode {
    let _ = s.shutdown.send(());
    StatusCode::ACCEPTED
}
async fn index() -> impl IntoResponse {
    axum::response::Html(include_str!("../web/index.html"))
}
async fn devices(State(s): State<AppState>) -> impl IntoResponse {
    Json(s.manager.devices().await)
}
async fn state(State(s): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let devices = s.manager.devices().await;
    let mut session_snapshots = s
        .manager
        .sessions()
        .await
        .into_iter()
        .map(|(serial, session)| {
            let (
                activity_state,
                script_active,
                script_generation,
                profile,
                preview_profile,
                preview_mode,
            ) = session.frames.stable_state_now();
            SessionStateSnapshot {
                serial,
                activity_state,
                script_active,
                script_generation,
                profile,
                preview_profile,
                preview_mode,
            }
        })
        .collect::<Vec<_>>();
    session_snapshots.sort_by(|a, b| a.serial.cmp(&b.serial));
    let mut scripts = s.scripts.lock().await;
    retain_script_runs(&mut scripts);
    let mut runs = scripts
        .iter()
        .filter(|(_, active)| active.run.is_running())
        .map(|(id, active)| ScriptRunStatus {
            run_id: *id,
            serial: active.serial.clone(),
            name: active.name.clone(),
            generation: active.generation,
            running: active.run.is_running(),
            stalled: active.run.is_stalled(),
            error: active.run.error(),
        })
        .collect::<Vec<_>>();
    runs.sort_by(|a, b| a.serial.cmp(&b.serial).then(a.run_id.cmp(&b.run_id)));
    drop(scripts);
    let snapshot = StateSnapshot {
        devices,
        sessions: session_snapshots,
        runs,
        scripts: cached_script_names(&s).await?,
    };
    // The state representation contains only stable session fields. Rolling
    // FPS, frame sequence and cumulative counters are served by `/metrics` so
    // they do not invalidate this validator on every decoded frame.
    let etag = state_etag(&snapshot);
    if matches_etag(&headers, &etag) {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::empty())
            .unwrap());
    }
    let body = serde_json::to_vec(&snapshot)?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap())
}
async fn metrics(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let mut values = BTreeMap::new();
    for (serial, session) in s.manager.sessions().await {
        values.insert(serial, session.frames.metrics_now());
    }
    Ok(Json(values))
}
async fn scan(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let devices = s.manager.scan(true).await?;
    ensure_sessions_for_devices(&s.manager, &devices, &s.session_autostart_blocked).await;
    Ok(Json(devices))
}
async fn connect(
    State(s): State<AppState>,
    Json(body): Json<ConnectRequest>,
) -> Result<impl IntoResponse, ApiError> {
    forge_core::adb::validate_endpoint(&body.endpoint)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let devices = s.manager.connect(&body.endpoint).await?;
    ensure_sessions_for_devices(&s.manager, &devices, &s.session_autostart_blocked).await;
    Ok(Json(devices))
}
async fn pairing_services(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(s.manager.pairing_services().await?))
}
async fn pair(
    State(s): State<AppState>,
    Json(body): Json<PairRequest>,
) -> Result<impl IntoResponse, ApiError> {
    forge_core::adb::validate_endpoint(&body.endpoint)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if body.code.len() != 6 || !body.code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::bad_request(
            "pairing code must contain exactly six digits",
        ));
    }
    let devices = s.manager.pair(&body.endpoint, &body.code).await?;
    ensure_sessions_for_devices(&s.manager, &devices, &s.session_autostart_blocked).await;
    Ok(Json(devices))
}
async fn screenshot(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<Response, ApiError> {
    validate_serial(&serial)?;
    let png = s.manager.adb().screenshot(&serial).await?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .body(Body::from(png))
        .unwrap())
}
async fn input(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(action): Json<InputAction>,
) -> Result<StatusCode, ApiError> {
    validate_serial(&serial)?;
    action.validate().map_err(ApiError::bad_request)?;
    if let Some(session) = s.manager.session(&serial).await {
        match action {
            InputAction::Tap { x, y } => session.tap(x, y).await?,
            InputAction::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
            } => session.swipe(x1, y1, x2, y2, duration_ms).await?,
            InputAction::Text { value } => session.text(&value).await?,
            InputAction::Key { code } => {
                let code = u32::try_from(code)
                    .map_err(|_| ApiError::bad_request("key code must be non-negative"))?;
                session.key(code, false).await?
            }
        }
    } else {
        s.manager.input(&serial, &action).await?
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn start_session(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<StartSessionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_serial(&serial)?;
    let profile = body
        .profile
        .unwrap_or(forge_core::session::PerformanceProfile::Auto);
    if body
        .max_size
        .is_some_and(|value| !(240..=4096).contains(&value))
    {
        return Err(ApiError::bad_request(
            "max_size must be between 240 and 4096",
        ));
    }
    if body
        .max_fps
        .is_some_and(|value| !(1..=120).contains(&value))
    {
        return Err(ApiError::bad_request("max_fps must be between 1 and 120"));
    }
    if body
        .bit_rate
        .is_some_and(|value| !(100_000..=50_000_000).contains(&value))
    {
        return Err(ApiError::bad_request(
            "bit_rate must be between 100000 and 50000000",
        ));
    }
    let mut options = forge_core::session::SessionOptions::default();
    if let Some((max_size, max_fps, bit_rate)) = profile.capture_defaults() {
        if body.max_size.is_none() {
            options.max_size = max_size;
        }
        if body.max_fps.is_none() {
            options.max_fps = max_fps;
        }
        if body.bit_rate.is_none() {
            options.bit_rate = bit_rate;
        }
    }
    if let Some(v) = body.max_size {
        options.max_size = v
    }
    if let Some(v) = body.bit_rate {
        options.bit_rate = v
    }
    if let Some(v) = body.max_fps {
        options.max_fps = v
    }
    if let Some(v) = body.codec {
        options.codec = match v.as_str() {
            "h265" => forge_core::protocol::stream::Codec::H265,
            "av1" => forge_core::protocol::stream::Codec::Av1,
            "h264" => forge_core::protocol::stream::Codec::H264,
            _ => return Err(ApiError::bad_request("unsupported codec")),
        }
    }
    s.session_autostart_blocked.lock().await.remove(&serial);
    let session = s.manager.start_session(serial, options).await?;
    Ok(Json(serde_json::json!({
        "device_name": session.device_name,
        "codec": format!("{:?}", session.codec),
        "capture_profile": profile
    })))
}
async fn stop_session(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<StatusCode, ApiError> {
    validate_serial(&serial)?;
    s.session_autostart_blocked
        .lock()
        .await
        .insert(serial.clone());
    stop_script_for_serial(&s, &serial).await;
    if s.manager.stop_session(&serial).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("session not running"))
    }
}
async fn list_sessions(State(s): State<AppState>) -> impl IntoResponse {
    let mut serials = s
        .manager
        .sessions()
        .await
        .into_iter()
        .map(|(serial, _)| serial)
        .collect::<Vec<_>>();
    serials.sort();
    Json(serials)
}
async fn start_all_sessions(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    {
        let devices = s.manager.devices().await;
        let mut blocked = s.session_autostart_blocked.lock().await;
        for device in devices {
            if matches!(device.state, forge_core::DeviceState::Device) {
                blocked.remove(&device.serial);
            }
        }
    }
    let mut tasks = tokio::task::JoinSet::new();
    let concurrency = std::env::var("SCRCPYFORGE_SESSION_START_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(1, 8);
    let permits = Arc::new(Semaphore::new(concurrency));
    for device in s.manager.devices().await {
        if !matches!(device.state, forge_core::DeviceState::Device) {
            continue;
        }
        let permit = permits.clone().acquire_owned().await?;
        let manager = s.manager.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let serial = device.serial;
            let result = manager
                .start_session(serial.clone(), Default::default())
                .await;
            result.map(|_| serial.clone()).map_err(|error| {
                tracing::warn!(serial = %serial, error = %error, "session start failed");
                (serial, "session start failed".to_owned())
            })
        });
    }
    let mut started = vec![];
    let mut failed = HashMap::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(serial)) => started.push(serial),
            Ok(Err((serial, error))) => {
                failed.insert(serial, error);
            }
            Err(error) => {
                tracing::warn!(error = %error, "session start worker failed");
                failed.insert("worker".into(), "session start failed".into());
            }
        }
    }
    Ok(Json(serde_json::json!({"started":started,"failed":failed})))
}
async fn stop_all_sessions(State(s): State<AppState>) -> StatusCode {
    // Drain the script registry first. A script can outlive a device scan or
    // a dead session, so deriving this list from `manager.sessions()` would
    // leave a run attached to an offline serial behind.
    let _ = stop_all_scripts(State(s.clone())).await;
    {
        let devices = s.manager.devices().await;
        let mut blocked = s.session_autostart_blocked.lock().await;
        for device in devices {
            if matches!(device.state, forge_core::DeviceState::Device) {
                blocked.insert(device.serial);
            }
        }
    }
    s.manager.stop_all_sessions().await;
    StatusCode::NO_CONTENT
}
async fn preview_mode(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<PreviewRequest>,
) -> Result<StatusCode, ApiError> {
    validate_serial(&serial)?;
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    let mode = match body.mode.as_str() {
        "off" => forge_core::session::PreviewMode::Off,
        "five_seconds" => forge_core::session::PreviewMode::FiveSeconds,
        "realtime" => forge_core::session::PreviewMode::Realtime,
        _ => return Err(ApiError::bad_request("unsupported preview mode")),
    };
    session.frames.set_preview_mode(mode).await;
    Ok(StatusCode::NO_CONTENT)
}
async fn set_profile(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<ProfileRequest>,
) -> Result<StatusCode, ApiError> {
    validate_serial(&serial)?;
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    session.frames.set_script_profile(body.profile).await;
    Ok(StatusCode::NO_CONTENT)
}
async fn set_script_profile(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<ProfileRequest>,
) -> Result<StatusCode, ApiError> {
    set_profile(State(s), Path(serial), Json(body)).await
}
async fn set_preview_profile(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<ProfileRequest>,
) -> Result<StatusCode, ApiError> {
    validate_serial(&serial)?;
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    session.frames.set_preview_profile(body.profile).await;
    Ok(StatusCode::NO_CONTENT)
}
async fn session_metrics(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    validate_serial(&serial)?;
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    Ok(Json(session.frames.metrics_now()))
}
async fn latest_frame(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    validate_serial(&serial)?;
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    let _lease = session.frames.acquire_frame_request();
    let frame = wait_for_latest_frame(&session).await?;
    let etag = frame_etag(&serial, frame.frame_seq);
    if matches_etag(&headers, &etag) {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::empty())
            .unwrap());
    }
    let jpg = tokio::task::spawn_blocking(move || frame.jpeg()).await??;
    if jpg.len() > 8 * 1024 * 1024 {
        return Err(ApiError::too_large("encoded frame exceeds 8 MiB"));
    }
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, etag)
        .body(Body::from(jpg))
        .unwrap())
}
async fn preview_stream(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    validate_serial(&serial)?;
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    let permit = s
        .ws_gate
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::busy("preview connection limit reached"))?;
    Ok(ws
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |mut socket| async move {
        let _permit = permit;
        // The lease is held for exactly the lifetime of this WebSocket. It
        // keeps preview demand visible to FrameHub and lets the session enter
        // its idle state as soon as the last client disconnects.
        let _lease = session.frames.acquire_preview();
        let mut rx = session.frames.preview();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if !session.is_alive() { break }
                    let send = tokio::time::timeout(
                        Duration::from_secs(5),
                        socket.send(Message::Ping(Bytes::new())),
                    ).await;
                    if !matches!(send, Ok(Ok(()))) { break }
                },
                result = rx.recv() => match result {
                    Ok(frame) => if let Ok(Ok(jpg)) = tokio::task::spawn_blocking(move || frame.jpeg()).await {
                        if jpg.len() > 8 * 1024 * 1024 {
                            tracing::warn!(serial = %session.serial, bytes = jpg.len(), "preview frame exceeds size limit");
                            break
                        }
                        let send = tokio::time::timeout(
                            Duration::from_secs(5),
                            socket.send(Message::Binary(jpg)),
                        ).await;
                        if !matches!(send, Ok(Ok(()))) { break }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        session.frames.record_preview_dropped(count);
                        continue
                    },
                    Err(_) => break,
                }
            }
        }
        }))
}
async fn save_region(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<RegionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_serial(&serial)?;
    if body.x1 >= body.x2 || body.y1 >= body.y2 {
        return Err(ApiError::bad_request(
            "region coordinates must form a non-empty rectangle",
        ));
    }
    let root = templates_dir();
    let path = if let Some(value) = body.path.filter(|v| !v.trim().is_empty()) {
        template_path(&root, &value)?
    } else {
        let name = body
            .name
            .ok_or_else(|| ApiError::bad_request("template name or path is required"))?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ApiError::bad_request("invalid template name"));
        }
        root.join(format!("{name}.png"))
    };
    let path = ensure_png_path(&root, path)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?
    }
    let session = s
        .manager
        .session(&serial)
        .await
        .ok_or_else(|| ApiError::not_found("session not running"))?;
    let _lease = session.frames.acquire_frame_request();
    let frame = wait_for_latest_frame(&session).await?;
    if body.x2 > frame.width || body.y2 > frame.height {
        return Err(ApiError::bad_request(
            "region coordinates exceed the current frame dimensions",
        ));
    }
    // Write beside the destination and atomically rename. This prevents a
    // reader from observing a partial PNG and avoids following a destination
    // symlink during the final replacement.
    let crop_path = path.with_file_name(format!(
        ".{}.tmp-{}.png",
        path.file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("template"),
        Uuid::new_v4()
    ));
    let output_path = path.clone();
    let region = (body.x1, body.y1, body.x2, body.y2);
    let crop_target = crop_path.clone();
    let crop_result =
        tokio::task::spawn_blocking(move || forge_core::cv::crop(&frame, &crop_target, region))
            .await
            .map_err(anyhow::Error::from)?;
    if let Err(error) = crop_result {
        let _ = tokio::fs::remove_file(&crop_path).await;
        return Err(error.into());
    }
    let sync_result = async {
        let file = tokio::fs::File::open(&crop_path).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = sync_result {
        let _ = tokio::fs::remove_file(&crop_path).await;
        return Err(error.into());
    }
    if let Err(error) = tokio::fs::rename(&crop_path, &output_path).await {
        let _ = tokio::fs::remove_file(&crop_path).await;
        return Err(error.into());
    }
    Ok(Json(
        serde_json::json!({"path":path,"serial":serial,"region":[body.x1,body.y1,body.x2,body.y2]}),
    ))
}

async fn wait_for_latest_frame(
    session: &std::sync::Arc<forge_core::session::ScrcpySession>,
) -> anyhow::Result<std::sync::Arc<forge_core::video::VideoFrame>> {
    let mut latest = session.frames.latest();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(frame) = latest.borrow_and_update().clone() {
            return Ok(frame);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for a decoded frame");
        }
        tokio::time::timeout(remaining, latest.changed())
            .await
            .context("timed out waiting for a decoded frame")?
            .context("frame stream stopped")?;
    }
}
async fn list_scripts(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(cached_script_names(&s).await?))
}
async fn script_names() -> Result<Vec<String>, ApiError> {
    let mut names = vec![];
    let mut entries = tokio::fs::read_dir(scripts_dir()).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let script = path.join("script.lua");
        let dir_ok = tokio::fs::symlink_metadata(&path)
            .await
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        let file_ok = tokio::fs::symlink_metadata(&script)
            .await
            .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        if dir_ok && file_ok {
            if let Some(name) = entry.file_name().to_str() {
                if name.len() <= MAX_SCRIPT_NAME_BYTES && validate_script_name(name).is_ok() {
                    names.push(name.to_owned());
                }
                if names.len() >= MAX_SCRIPT_COUNT {
                    break;
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

async fn cached_script_names(state: &AppState) -> Result<Vec<String>, ApiError> {
    {
        let cache = state.script_catalog.lock().await;
        if cache
            .refreshed
            .is_some_and(|refreshed| refreshed.elapsed() < Duration::from_secs(2))
        {
            return Ok(cache.names.clone());
        }
    }
    let names = script_names().await?;
    let mut cache = state.script_catalog.lock().await;
    cache.refreshed = Some(Instant::now());
    cache.names = names.clone();
    Ok(names)
}
async fn list_script_runs(State(s): State<AppState>) -> impl IntoResponse {
    let mut scripts = s.scripts.lock().await;
    retain_script_runs(&mut scripts);
    Json(
        scripts
            .iter()
            .map(|(id, active)| ScriptRunStatus {
                run_id: *id,
                serial: active.serial.clone(),
                name: active.name.clone(),
                generation: active.generation,
                running: active.run.is_running(),
                stalled: active.run.is_stalled(),
                error: active.run.error(),
            })
            .collect::<Vec<_>>(),
    )
}
async fn run_named(
    State(s): State<AppState>,
    Json(body): Json<RunNamedRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let source = load_named(&body.name).await?;
    run_script(
        State(s),
        Json(RunScriptRequest {
            serial: body.serial,
            source,
            name: Some(body.name),
        }),
    )
    .await
}
async fn run_all(
    State(s): State<AppState>,
    Json(body): Json<RunAllRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let source = load_named(&body.name).await?;
    let devices = s.manager.devices().await;
    let concurrency = std::env::var("SCRCPYFORGE_SCRIPT_START_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(1, 8);
    let permits = Arc::new(Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    for device in devices {
        if matches!(device.state, forge_core::DeviceState::Device)
            && s.manager.session(&device.serial).await.is_some()
        {
            let permit = permits.clone().acquire_owned().await?;
            let state = s.clone();
            let serial = device.serial;
            let script_name = body.name.clone();
            let script_source = source.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let result = start_script(
                    &state,
                    RunScriptRequest {
                        serial: serial.clone(),
                        source: script_source,
                        name: Some(script_name),
                    },
                )
                .await;
                result
                    .map(|run_id| (serial.clone(), run_id))
                    .map_err(|error| (serial, error.client_message()))
            });
        }
    }
    let mut ids = Vec::new();
    let mut failed = HashMap::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok((_, run_id))) => ids.push(run_id),
            Ok(Err((serial, error))) => {
                failed.insert(serial, error);
            }
            Err(error) => {
                tracing::warn!(error = %error, "script start worker failed");
                failed.insert("worker".into(), "script start failed".into());
            }
        }
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"run_ids":ids,"failed":failed})),
    ))
}
async fn stop_all_scripts(State(s): State<AppState>) -> StatusCode {
    let active: Vec<_> = s.scripts.lock().await.drain().map(|(_, v)| v).collect();
    for item in active {
        item.run.cancel();
        if let Some(session) = s.manager.session(&item.serial).await {
            session.frames.detach_script_if(item.generation);
        }
    }
    StatusCode::ACCEPTED
}
async fn load_named(name: &str) -> Result<String, ApiError> {
    let path = safe_script_dir(name)?.join("script.lua");
    if std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(ApiError::bad_request(
            "script file may not be a symbolic link",
        ));
    }
    let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found("named script not found")
        } else {
            error.into()
        }
    })?;
    if !metadata.is_file() {
        return Err(ApiError::bad_request("script.lua must be a regular file"));
    }
    if metadata.len() > MAX_SCRIPT_BYTES as u64 {
        return Err(ApiError::bad_request("script is too large"));
    }
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_SCRIPT_BYTES as u64) as usize);
    file.take((MAX_SCRIPT_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_SCRIPT_BYTES {
        return Err(ApiError::bad_request("script is too large"));
    }
    String::from_utf8(bytes).map_err(|_| ApiError::bad_request("script must be valid UTF-8"))
}

fn validate_script_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty()
        || name.len() > MAX_SCRIPT_NAME_BYTES
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request("invalid script name"));
    }
    Ok(())
}

fn validate_serial(serial: &str) -> Result<(), ApiError> {
    if serial.is_empty()
        || serial.len() > 255
        || serial.starts_with('-')
        || serial
            .bytes()
            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'/' | b'\\'))
    {
        return Err(ApiError::bad_request("invalid device serial"));
    }
    Ok(())
}

fn safe_script_dir(name: &str) -> Result<PathBuf, ApiError> {
    validate_script_name(name)?;
    let root = scripts_dir();
    let root_canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let path = root_canonical.join(name);
    if std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(ApiError::bad_request(
            "script directory may not be a symbolic link",
        ));
    }
    let canonical =
        std::fs::canonicalize(&path).map_err(|_| ApiError::not_found("named script not found"))?;
    if !canonical.starts_with(&root_canonical) || !canonical.is_dir() {
        return Err(ApiError::bad_request(
            "script directory is outside the scripts root",
        ));
    }
    Ok(canonical)
}

fn retain_script_runs(scripts: &mut HashMap<Uuid, ActiveRun>) {
    let now = Instant::now();
    scripts.retain(|_, active| {
        if active.run.is_running() {
            active.finished_at = None;
            true
        } else {
            let finished_at = active.finished_at.get_or_insert(now);
            finished_at.elapsed() < Duration::from_secs(60)
        }
    });
}

fn template_path(root: &std::path::Path, value: &str) -> Result<PathBuf, ApiError> {
    let value = value.trim();
    if value.len() > 1024 {
        return Err(ApiError::bad_request(
            "template path must be at most 1024 bytes",
        ));
    }
    let relative = std::path::Path::new(value);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(ApiError::bad_request(
            "template path must be relative to the templates directory",
        ));
    }
    Ok(root.join(relative))
}

fn ensure_png_path(root: &std::path::Path, path: PathBuf) -> Result<PathBuf, ApiError> {
    let path = if path.extension().is_none() {
        path.with_extension("png")
    } else {
        path
    };
    if path.extension().and_then(|value| value.to_str()) != Some("png")
        || !path.starts_with(root)
        || std::fs::symlink_metadata(root)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(ApiError::bad_request(
            "template output must be a PNG inside the templates directory",
        ));
    }
    let mut current = root.to_owned();
    if let Ok(relative) = path.strip_prefix(root) {
        let components = relative.components().collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current.push(component.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(ApiError::bad_request(
                        "template parent may not contain a symbolic link",
                    ))
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(ApiError::bad_request("template parent is not a directory"))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    if std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(ApiError::bad_request(
            "template output may not replace a symbolic link",
        ));
    }
    Ok(path)
}

fn data_dir(env_name: &str, leaf: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(env_name) {
        return path.into();
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(leaf));
            candidates.push(dir.join("resources").join(leaf));
            candidates.push(dir.join("../share/scrcpyforge").join(leaf));
            if let Some(root) = dir.parent().and_then(|path| path.parent()) {
                candidates.push(root.join(leaf));
                candidates.push(root.join("scrcpyforge-rs").join(leaf));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("scrcpyforge-rs").join(leaf));
        candidates.push(cwd.join(leaf));
    }
    candidates
        .iter()
        .find(|path| path.is_dir())
        .cloned()
        .unwrap_or_else(|| user_data_root().join(leaf))
}
fn user_data_root() -> PathBuf {
    if let Some(path) = std::env::var_os("SCRCPYFORGE_DATA_DIR") {
        return path.into();
    }
    #[cfg(target_os = "windows")]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("ScrcpyForge");
    }
    #[cfg(target_os = "macos")]
    if let Some(path) = std::env::var_os("HOME") {
        return PathBuf::from(path).join("Library/Application Support/ScrcpyForge");
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path).join("scrcpyforge");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".local/share/scrcpyforge"))
        .unwrap_or_else(|| PathBuf::from("scrcpyforge-data"))
}
fn scripts_dir() -> PathBuf {
    data_dir("SCRCPYFORGE_SCRIPTS_DIR", "scripts")
}
fn templates_dir() -> PathBuf {
    data_dir("SCRCPYFORGE_TEMPLATES_DIR", "templates")
}

fn state_etag(snapshot: &StateSnapshot) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for device in &snapshot.devices {
        device.serial.hash(&mut hasher);
        device.state.hash(&mut hasher);
        device.product.hash(&mut hasher);
        device.model.hash(&mut hasher);
        device.transport_id.hash(&mut hasher);
        device.wireless.hash(&mut hasher);
    }
    for session in &snapshot.sessions {
        session.serial.hash(&mut hasher);
        session.activity_state.hash(&mut hasher);
        session.script_active.hash(&mut hasher);
        session.script_generation.hash(&mut hasher);
        session.profile.hash(&mut hasher);
        session.preview_profile.hash(&mut hasher);
        session.preview_mode.hash(&mut hasher);
    }
    for run in &snapshot.runs {
        run.run_id.hash(&mut hasher);
        run.serial.hash(&mut hasher);
        run.name.hash(&mut hasher);
        run.generation.hash(&mut hasher);
        run.running.hash(&mut hasher);
        run.stalled.hash(&mut hasher);
        run.error.hash(&mut hasher);
    }
    snapshot.scripts.hash(&mut hasher);
    format!("\"state-{:016x}\"", hasher.finish())
}

fn frame_etag(serial: &str, frame_seq: u64) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serial.hash(&mut hasher);
    frame_seq.hash(&mut hasher);
    format!("\"frame-{:016x}\"", hasher.finish())
}

fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .map(|item| item.strip_prefix("W/").unwrap_or(item))
                .any(|item| item == "*" || item == etag)
        })
}
async fn run_script(
    State(s): State<AppState>,
    Json(body): Json<RunScriptRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let id = start_script(&s, body).await?;
    Ok((StatusCode::ACCEPTED, Json(RunResponse { run_id: id })))
}
async fn start_script(s: &AppState, body: RunScriptRequest) -> Result<Uuid, ApiError> {
    let RunScriptRequest {
        serial,
        source,
        name,
    } = body;
    validate_serial(&serial)?;
    if source.trim().is_empty() {
        return Err(ApiError::bad_request("script source must not be empty"));
    }
    if source.len() > MAX_SCRIPT_BYTES {
        return Err(ApiError::bad_request("script is too large"));
    }
    if let Some(value) = name.as_deref() {
        validate_script_name(value)?;
    }
    lua::validate_source(&source)
        .map_err(|error| ApiError::bad_request(format!("invalid Lua source: {error}")))?;
    let session = s.manager.session(&serial).await.ok_or_else(|| {
        ApiError::not_found("scrcpy session must be started before a frame script")
    })?;
    let script_dir = name.as_deref().map(safe_script_dir).transpose()?;
    // Only a validated replacement is allowed to stop the current run.
    stop_script_for_serial(s, &serial).await;
    let generation = session.frames.begin_script();
    let (run, frames) = lua::run_frames_for_generation(
        source,
        script_dir,
        session.clone(),
        generation,
        s.manager.event_sender(),
    );
    if !session
        .frames
        .attach_script_for_generation(frames, generation)
    {
        run.cancel();
        session.frames.detach_script_if(generation);
        return Err(ApiError::conflict("script was superseded before attach"));
    }
    if session.frames.script_generation() != generation {
        run.cancel();
        session.frames.detach_script_if(generation);
        return Err(ApiError::conflict("script was superseded during start"));
    }
    let id = run.id;
    let mut scripts = s.scripts.lock().await;
    if session.frames.script_generation() != generation {
        drop(scripts);
        run.cancel();
        session.frames.detach_script_if(generation);
        return Err(ApiError::conflict(
            "script was superseded during registration",
        ));
    }
    let replaced = scripts
        .iter()
        .filter_map(|(run_id, active)| (active.serial == serial).then_some(*run_id))
        .collect::<Vec<_>>();
    let replaced = replaced
        .into_iter()
        .filter_map(|run_id| scripts.remove(&run_id))
        .collect::<Vec<_>>();
    scripts.insert(
        id,
        ActiveRun {
            run,
            serial,
            name,
            generation,
            finished_at: None,
        },
    );
    drop(scripts);
    for active in replaced {
        active.run.cancel();
    }
    Ok(id)
}
async fn stop_script(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if let Some(active) = s.scripts.lock().await.remove(&id) {
        active.run.cancel();
        if let Some(session) = s.manager.session(&active.serial).await {
            session.frames.detach_script_if(active.generation);
        }
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::not_found("script run not found"))
    }
}
async fn stop_device_script(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<StatusCode, ApiError> {
    validate_serial(&serial)?;
    if stop_script_for_serial(&s, &serial).await {
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(ApiError::not_found("script run not found for device"))
    }
}
async fn stop_script_for_serial(s: &AppState, serial: &str) -> bool {
    let mut scripts = s.scripts.lock().await;
    let ids = scripts
        .iter()
        .filter_map(|(id, active)| (active.serial == serial).then_some(*id))
        .collect::<Vec<_>>();
    let active = ids
        .into_iter()
        .filter_map(|id| scripts.remove(&id))
        .collect::<Vec<_>>();
    drop(scripts);
    if active.is_empty() {
        return false;
    }
    for item in &active {
        item.run.cancel();
    }
    if let Some(session) = s.manager.session(serial).await {
        for item in &active {
            session.frames.detach_script_if(item.generation);
        }
    }
    true
}
async fn events(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let permit = s
        .ws_gate
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::busy("event connection limit reached"))?;
    Ok(ws
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            event_socket(socket, s.manager.subscribe()).await;
        }))
}
async fn event_socket(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<forge_core::ForgeEvent>,
) {
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let send = tokio::time::timeout(
                    Duration::from_secs(5),
                    socket.send(Message::Ping(Bytes::new())),
                ).await;
                if !matches!(send, Ok(Ok(()))) { break }
            }
            result = rx.recv() => match result {
                Ok(event) => {
                    let Ok(payload) = serde_json::to_string(&event) else {
                        tracing::warn!("failed to serialize forge event");
                        break;
                    };
                    if payload.len() > 64 * 1024 {
                        tracing::warn!(bytes = payload.len(), "event payload exceeds size limit");
                        continue;
                    }
                    let send = tokio::time::timeout(
                        Duration::from_secs(5),
                        socket.send(Message::Text(payload.into())),
                    )
                    .await;
                    if !matches!(send, Ok(Ok(()))) {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }
}
async fn shutdown_signal(mut requested: tokio::sync::broadcast::Receiver<()>) {
    tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=requested.recv()=>{} }
}

struct ApiError {
    error: anyhow::Error,
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            error: anyhow::Error::msg(message.into()),
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            error: anyhow::Error::msg(message.into()),
            status: StatusCode::CONFLICT,
            code: "conflict",
        }
    }

    fn busy(message: impl Into<String>) -> Self {
        Self {
            error: anyhow::Error::msg(message.into()),
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "busy",
        }
    }

    fn too_large(message: impl Into<String>) -> Self {
        Self {
            error: anyhow::Error::msg(message.into()),
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            error: anyhow::Error::msg(message.into()),
            status: StatusCode::NOT_FOUND,
            code: "not_found",
        }
    }

    fn client_message(&self) -> String {
        if self.code == "internal_error" {
            "internal server error".to_owned()
        } else {
            self.error.to_string()
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(value: E) -> Self {
        Self {
            error: value.into(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.code == "internal_error" {
            tracing::error!(error = %self.error, "request failed");
        }
        let message = self.client_message();
        (
            self.status,
            Json(serde_json::json!({
                "error": {
                    "code": self.code,
                    "message": message
                }
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{frame_etag, matches_etag, same_origin};
    use axum::http::{header, HeaderMap, HeaderValue};

    #[test]
    fn conditional_etags_accept_lists_and_wildcards() {
        let etag = frame_etag("device:5555", 42);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("\"old\", {etag}")).unwrap(),
        );
        assert!(matches_etag(&headers, &etag));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(matches_etag(&headers, &etag));
    }

    #[test]
    fn frame_etag_changes_only_when_frame_identity_changes() {
        assert_eq!(frame_etag("device", 1), frame_etag("device", 1));
        assert_ne!(frame_etag("device", 1), frame_etag("device", 2));
        assert_ne!(frame_etag("device", 1), frame_etag("other", 1));
    }

    #[test]
    fn origin_guard_requires_exact_http_authority_match() {
        assert!(same_origin("http://127.0.0.1:27180", "127.0.0.1:27180"));
        assert!(!same_origin("http://127.0.0.1:27181", "127.0.0.1:27180"));
        assert!(!same_origin("null", "127.0.0.1:27180"));
        assert!(!same_origin("file://", "127.0.0.1:27180"));
    }
}
