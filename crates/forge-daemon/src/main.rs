use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use forge_core::{lua, DeviceManager, InputAction, RunScriptRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    manager: DeviceManager,
    scripts: Arc<Mutex<HashMap<Uuid, ActiveRun>>>,
    shutdown: tokio::sync::broadcast::Sender<()>,
}
struct ActiveRun {
    run: lua::LuaRun,
    serial: String,
    name: Option<String>,
}

#[derive(Clone, Serialize)]
struct ScriptRunStatus {
    run_id: Uuid,
    serial: String,
    name: Option<String>,
    running: bool,
    stalled: bool,
    error: Option<String>,
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
    let state = AppState {
        manager: DeviceManager::new(),
        scripts: Default::default(),
        shutdown,
    };
    spawn_polling(state.manager.clone(), state.shutdown.subscribe());
    let app = Router::new()
        .route("/", get(index))
        .route("/api/v1/health", get(health))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/shutdown", post(request_shutdown))
        .route("/api/v1/devices", get(devices))
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
        .with_state(state.clone());
    let addr = std::env::var("SCRCPYFORGE_ADDR").unwrap_or_else(|_| "127.0.0.1:27180".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "ScrcpyForge backend listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_rx))
        .await?;
    let active: Vec<_> = state
        .scripts
        .lock()
        .await
        .drain()
        .map(|(_, item)| item)
        .collect();
    for item in active {
        item.run.cancel();
    }
    state.manager.stop_all_sessions().await;
    Ok(())
}

fn spawn_polling(manager: DeviceManager, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {_ = shutdown.recv()=>break,_ = timer.tick()=>if let Err(error) = manager.scan(true).await { tracing::debug!(%error, "device scan failed"); }}
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
    Json(
        serde_json::json!({"api_version":1,"scrcpy_server":"4.0","codecs":["h264","h265","av1"],"frame_storage":"i420","vision_modes":["color","gray","multiscale"],"lua":"5.4","preview":["websocket_jpeg","five_seconds","off"],"script_profiles":["auto","eco","balanced","realtime"],"wireless_pairing":true}),
    )
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
            InputAction::Text { value } => session.text(&value).await?,
            other => s.manager.input(&serial, &other).await?,
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
    let mut options = forge_core::session::SessionOptions::default();
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
    Ok(Json(
        serde_json::json!({"device_name":session.device_name,"codec":format!("{:?}",session.codec)}),
    ))
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
    let mut serials = vec![];
    for device in s.manager.devices().await {
        if s.manager.session(&device.serial).await.is_some() {
            serials.push(device.serial)
        }
    }
    Json(serials)
}
async fn start_all_sessions(State(s): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let mut tasks = tokio::task::JoinSet::new();
    for device in s.manager.devices().await {
        let manager = s.manager.clone();
        tasks.spawn(async move {
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
    Ok(Json(session.frames.metrics().await))
}
async fn latest_frame(
    State(s): State<AppState>,
    Path(serial): Path<String>,
) -> Result<Response, ApiError> {
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    let frame = session
        .frames
        .latest()
        .borrow()
        .clone()
        .context("no decoded frame yet")?;
    let jpg = tokio::task::spawn_blocking(move || frame.jpeg()).await??;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-store")
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
    Ok(ws.on_upgrade(move|mut socket|async move{let mut rx=session.frames.preview();let mut tick=tokio::time::interval(Duration::from_millis(500));loop{tokio::select!{_ = tick.tick()=>if !session.is_alive(){break},result=rx.recv()=>match result{Ok(frame)=>if let Ok(Ok(jpg))=tokio::task::spawn_blocking(move||frame.jpeg()).await{if socket.send(Message::Binary(jpg)).await.is_err(){break}},Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>continue,Err(_)=>break}}}}))
}
async fn save_region(
    State(s): State<AppState>,
    Path(serial): Path<String>,
    Json(body): Json<RegionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let path = if let Some(value) = body.path.filter(|v| !v.trim().is_empty()) {
        let mut p = PathBuf::from(value);
        if p.is_relative() {
            p = templates_dir().join(p)
        }
        p
    } else {
        let name = body.name.context("template name or path is required")?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ApiError(anyhow::anyhow!("invalid template name")));
        }
        templates_dir().join(format!("{name}.png"))
    };
    let path = if path.extension().is_none() {
        path.with_extension("png")
    } else {
        path
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?
    }
    let session = s
        .manager
        .session(&serial)
        .await
        .context("session not running")?;
    let frame = session
        .frames
        .latest()
        .borrow()
        .clone()
        .context("no decoded frame yet")?;
    forge_core::cv::crop(&frame, &path, (body.x1, body.y1, body.x2, body.y2))?;
    Ok(Json(
        serde_json::json!({"path":path,"serial":serial,"region":[body.x1,body.y1,body.x2,body.y2]}),
    ))
}
async fn list_scripts() -> Result<impl IntoResponse, ApiError> {
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
    Ok(Json(names))
}
async fn list_script_runs(State(s): State<AppState>) -> impl IntoResponse {
    let scripts = s.scripts.lock().await;
    Json(
        scripts
            .iter()
            .map(|(id, active)| ScriptRunStatus {
                run_id: *id,
                serial: active.serial.clone(),
                name: active.name.clone(),
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
            session.frames.detach_script().await
        }
    }
    StatusCode::ACCEPTED
}
async fn load_named(name: &str) -> Result<String, ApiError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError(anyhow::anyhow!("invalid script name")));
    }
    Ok(tokio::fs::read_to_string(scripts_dir().join(name).join("script.lua")).await?)
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
    let script_dir = name.as_ref().map(|value| scripts_dir().join(value));
    let (run, frames) = lua::run_frames(
        body.source,
        script_dir,
        session.clone(),
        s.manager.event_sender(),
    );
    session.frames.attach_script(frames).await;
    let id = run.id;
    s.scripts
        .lock()
        .await
        .insert(id, ActiveRun { run, serial, name });
    Ok(id)
}
async fn stop_script(State(s): State<AppState>, Path(id): Path<Uuid>) -> StatusCode {
    if let Some(active) = s.scripts.lock().await.remove(&id) {
        active.run.cancel();
        if let Some(session) = s.manager.session(&active.serial).await {
            session.frames.detach_script().await
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
    let ids = s
        .scripts
        .lock()
        .await
        .iter()
        .filter_map(|(id, active)| (active.serial == serial).then_some(*id))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return false;
    }
    let mut scripts = s.scripts.lock().await;
    for id in ids {
        if let Some(active) = scripts.remove(&id) {
            active.run.cancel();
        }
    }
    drop(scripts);
    if let Some(session) = s.manager.session(serial).await {
        session.frames.detach_script().await
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
