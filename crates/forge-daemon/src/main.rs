use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use forge_core::{lua, DeviceInfo, DeviceManager, InputAction, RunScriptRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    manager: DeviceManager,
    scripts: Arc<Mutex<HashMap<Uuid, ActiveRun>>>,
    script_catalog: Arc<Mutex<ScriptCatalog>>,
    shutdown: tokio::sync::broadcast::Sender<()>,
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
struct SessionSnapshot {
    serial: String,
    metrics: forge_core::session::SessionMetrics,
}

#[derive(Serialize)]
struct StateSnapshot {
    devices: Vec<DeviceInfo>,
    sessions: Vec<SessionSnapshot>,
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
    server_jar: Option<String>,
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
    let app_state = AppState {
        manager: DeviceManager::new(),
        scripts: Default::default(),
        script_catalog: Default::default(),
        shutdown,
    };
    spawn_polling(app_state.manager.clone(), app_state.shutdown.subscribe());
    let app = Router::new()
        .route("/", get(index))
        .route("/api/v1/health", get(health))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/shutdown", post(request_shutdown))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/state", get(state))
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
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(app_state.clone());
    let addr = std::env::var("SCRCPYFORGE_ADDR").unwrap_or_else(|_| "127.0.0.1:27180".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
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

fn spawn_polling(manager: DeviceManager, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(5);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(delay) => {
                    match manager.scan(true).await {
                        Ok(devices) if devices.iter().any(|device| {
                            matches!(device.state, forge_core::DeviceState::Device)
                        }) => {
                            delay = Duration::from_secs(5);
                        }
                        Ok(_) => {
                            delay = (delay * 2).min(Duration::from_secs(60));
                        }
                        Err(error) => {
                            tracing::debug!(%error, "device scan failed");
                            delay = (delay * 2).min(Duration::from_secs(60));
                        }
                    }
                }
            }
        }
    });
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
        "script_profiles": ["auto", "eco", "balanced", "realtime"],
        "capture_profiles": {
            "eco": {"max_size": 720, "max_fps": 15, "bit_rate": 2000000},
            "balanced": {"max_size": 960, "max_fps": 30, "bit_rate": 4000000},
            "realtime": {"max_size": 1280, "max_fps": 60, "bit_rate": 8000000}
        },
        "demand_aware_preview": true,
        "etag": ["state", "frame"],
        "state_endpoint": "/api/v1/state",
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
        .map(|(serial, session)| SessionSnapshot {
            serial,
            metrics: session.frames.metrics_now(),
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
    // Age and rolling FPS are intentionally excluded from the validator: they
    // change while the underlying state is unchanged and would defeat 304.
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
async fn scan(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(s.manager.scan(true).await?))
}
async fn connect(
    State(s): State<AppState>,
    Json(body): Json<ConnectRequest>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(s.manager.connect(&body.endpoint).await?))
}
async fn pairing_services(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(s.manager.pairing_services().await?))
}
async fn pair(
    State(s): State<AppState>,
    Json(body): Json<PairRequest>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(s.manager.pair(&body.endpoint, &body.code).await?))
}
async fn screenshot(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<Response, ApiError> {
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
                let code = u32::try_from(code).context("key code must be non-negative")?;
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
    let profile = body
        .profile
        .unwrap_or(forge_core::session::PerformanceProfile::Auto);
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
    if let Some(v) = body.server_jar {
        options.server_jar = v.into()
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
            _ => forge_core::protocol::stream::Codec::H264,
        }
    }
    let session = s.manager.start_session(serial, options).await?;
    Ok(Json(serde_json::json!({
        "device_name": session.device_name,
        "codec": format!("{:?}", session.codec),
        "capture_profile": profile
    })))
}
async fn stop_session(State(s): State<AppState>, Path(serial): Path<String>) -> StatusCode {
    stop_script_for_serial(&s, &serial).await;
    if s.manager.stop_session(&serial).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
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
    let mut tasks = tokio::task::JoinSet::new();
    let concurrency = std::env::var("SCRCPYFORGE_SESSION_START_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(1, 8);
    let permits = Arc::new(Semaphore::new(concurrency));
    for device in s.manager.devices().await {
        let permit = permits.clone().acquire_owned().await?;
        let manager = s.manager.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let serial = device.serial;
            let result = manager
                .start_session(serial.clone(), Default::default())
                .await;
            result
                .map(|_| serial.clone())
                .map_err(|error| (serial, error.to_string()))
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
                failed.insert("worker".into(), error.to_string());
            }
        }
    }
    Ok(Json(serde_json::json!({"started":started,"failed":failed})))
}
async fn stop_all_sessions(State(s): State<AppState>) -> StatusCode {
    for device in s.manager.devices().await {
        stop_script_for_serial(&s, &device.serial).await;
        s.manager.stop_session(&device.serial).await;
    }
    StatusCode::NO_CONTENT
}
async fn preview_mode(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<PreviewRequest>,
) -> Result<StatusCode, ApiError> {
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    let mode = match body.mode.as_str() {
        "off" => forge_core::session::PreviewMode::Off,
        "five_seconds" => forge_core::session::PreviewMode::FiveSeconds,
        _ => forge_core::session::PreviewMode::Realtime,
    };
    session.frames.set_preview_mode(mode).await;
    Ok(StatusCode::NO_CONTENT)
}
async fn set_profile(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<ProfileRequest>,
) -> Result<StatusCode, ApiError> {
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
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
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    session.frames.set_preview_profile(body.profile).await;
    Ok(StatusCode::NO_CONTENT)
}
async fn session_metrics(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    Ok(Json(session.frames.metrics_now()))
}
async fn latest_frame(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    let _lease = session.frames.acquire_preview();
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
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    Ok(ws.on_upgrade(move |mut socket| async move {
        // The lease is held for exactly the lifetime of this WebSocket. It
        // keeps preview demand visible to FrameHub and lets the session enter
        // its idle state as soon as the last client disconnects.
        let _lease = session.frames.acquire_preview();
        let mut rx = session.frames.preview();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => if !session.is_alive() { break },
                result = rx.recv() => match result {
                    Ok(frame) => if let Ok(Ok(jpg)) = tokio::task::spawn_blocking(move || frame.jpeg()).await {
                        if socket.send(Message::Binary(jpg)).await.is_err() { break }
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
    let root = templates_dir();
    let path = if let Some(value) = body.path.filter(|v| !v.trim().is_empty()) {
        template_path(&root, &value)?
    } else {
        let name = body.name.context("template name or path is required")?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ApiError(anyhow::anyhow!("invalid template name")));
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
        .context("session not running")?;
    let _lease = session.frames.acquire_preview();
    let frame = wait_for_latest_frame(&session).await?;
    let crop_path = path.clone();
    let region = (body.x1, body.y1, body.x2, body.y2);
    tokio::task::spawn_blocking(move || forge_core::cv::crop(&frame, &crop_path, region)).await??;
    Ok(Json(
        serde_json::json!({"path":path,"serial":serial,"region":[body.x1,body.y1,body.x2,body.y2]}),
    ))
}

async fn wait_for_latest_frame(
    session: &std::sync::Arc<forge_core::session::ScrcpySession>,
) -> anyhow::Result<std::sync::Arc<forge_core::video::VideoFrame>> {
    let mut latest = session.frames.latest();
    if let Some(frame) = latest.borrow_and_update().clone() {
        return Ok(frame);
    }
    tokio::time::timeout(Duration::from_secs(3), latest.changed())
        .await
        .context("timed out waiting for a decoded frame")?
        .context("frame stream stopped")?;
    let frame = latest
        .borrow_and_update()
        .clone()
        .context("no decoded frame yet")?;
    Ok(frame)
}
async fn list_scripts(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    Ok(Json(cached_script_names(&s).await?))
}
async fn script_names() -> Result<Vec<String>, ApiError> {
    let mut names = vec![];
    let mut entries = tokio::fs::read_dir(scripts_dir()).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().join("script.lua").is_file() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned())
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
    let mut ids = vec![];
    for device in devices {
        if s.manager.session(&device.serial).await.is_some() {
            ids.push(
                start_script(
                    &s,
                    RunScriptRequest {
                        serial: device.serial,
                        source: source.clone(),
                        name: Some(body.name.clone()),
                    },
                )
                .await?,
            )
        }
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"run_ids":ids})),
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
    validate_script_name(name)?;
    Ok(tokio::fs::read_to_string(scripts_dir().join(name).join("script.lua")).await?)
}

fn validate_script_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError(anyhow::anyhow!("invalid script name")));
    }
    Ok(())
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
    let relative = std::path::Path::new(value.trim());
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
        return Err(ApiError(anyhow::anyhow!(
            "template path must be relative to the templates directory"
        )));
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
        return Err(ApiError(anyhow::anyhow!(
            "template output must be a PNG inside the templates directory"
        )));
    }
    let mut current = root.to_owned();
    if let Ok(relative) = path.strip_prefix(root) {
        let components = relative.components().collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current.push(component.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(ApiError(anyhow::anyhow!(
                        "template parent may not contain a symbolic link"
                    )))
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(ApiError(anyhow::anyhow!(
                        "template parent is not a directory"
                    )))
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
        return Err(ApiError(anyhow::anyhow!(
            "template output may not replace a symbolic link"
        )));
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
        let metrics = &session.metrics;
        metrics.latest_frame_seq.hash(&mut hasher);
        metrics.decoded_frames.hash(&mut hasher);
        metrics.preview_frames.hash(&mut hasher);
        metrics.preview_dropped_frames.hash(&mut hasher);
        metrics.script_frames.hash(&mut hasher);
        metrics.dropped_script_frames.hash(&mut hasher);
        metrics.script_published.hash(&mut hasher);
        metrics.script_rescans.hash(&mut hasher);
        metrics.script_generation.hash(&mut hasher);
        metrics.input_batches.hash(&mut hasher);
        metrics.input_failures.hash(&mut hasher);
        metrics.video_packet_errors.hash(&mut hasher);
        metrics.video_decode_errors.hash(&mut hasher);
        metrics.video_dimension_changes.hash(&mut hasher);
        metrics.preview_leases.hash(&mut hasher);
        metrics.script_active.hash(&mut hasher);
        metrics.profile.hash(&mut hasher);
        metrics.preview_profile.hash(&mut hasher);
        metrics.activity_state.hash(&mut hasher);
        metrics.last_video_error.hash(&mut hasher);
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
    let session = s
        .manager
        .session(&body.serial)
        .await
        .context("scrcpy session must be started before a frame script")?;
    let serial = body.serial;
    stop_script_for_serial(s, &serial).await;
    let name = body.name;
    if let Some(value) = name.as_deref() {
        validate_script_name(value)?;
    }
    let script_dir = name.as_ref().map(|value| scripts_dir().join(value));
    let generation = session.frames.begin_script();
    let (run, frames) = lua::run_frames_for_generation(
        body.source,
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
        return Err(ApiError(anyhow::anyhow!(
            "script was superseded before attach"
        )));
    }
    if session.frames.script_generation() != generation {
        run.cancel();
        session.frames.detach_script_if(generation);
        return Err(ApiError(anyhow::anyhow!(
            "script was superseded during start"
        )));
    }
    let id = run.id;
    let mut scripts = s.scripts.lock().await;
    if session.frames.script_generation() != generation {
        drop(scripts);
        run.cancel();
        session.frames.detach_script_if(generation);
        return Err(ApiError(anyhow::anyhow!(
            "script was superseded during registration"
        )));
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
async fn stop_script(State(s): State<AppState>, Path(id): Path<Uuid>) -> StatusCode {
    if let Some(active) = s.scripts.lock().await.remove(&id) {
        active.run.cancel();
        if let Some(session) = s.manager.session(&active.serial).await {
            session.frames.detach_script_if(active.generation);
        }
        StatusCode::ACCEPTED
    } else {
        StatusCode::NOT_FOUND
    }
}
async fn stop_device_script(State(s): State<AppState>, Path(serial): Path<String>) -> StatusCode {
    if stop_script_for_serial(&s, &serial).await {
        StatusCode::ACCEPTED
    } else {
        StatusCode::NOT_FOUND
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
async fn events(ws: WebSocketUpgrade, State(s): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| event_socket(socket, s.manager.subscribe()))
}
async fn event_socket(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<forge_core::ForgeEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                if socket
                    .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}
async fn shutdown_signal(mut requested: tokio::sync::broadcast::Receiver<()>) {
    tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=requested.recv()=>{} }
}

struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(value: E) -> Self {
        Self(value.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": self.0.to_string()})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{frame_etag, matches_etag};
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
}
