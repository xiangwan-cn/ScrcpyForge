use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use mlua::{Error as LuaError, HookTriggers, Lua, Table, VmState};
use tokio::runtime::Handle;
use uuid::Uuid;

use crate::{adb::Adb, session::ScrcpySession, video::VideoFrame, ForgeEvent, InputAction};

pub struct LuaRun {
    pub id: Uuid,
    cancelled: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    processing: Arc<AtomicBool>,
    last_progress: Arc<std::sync::Mutex<Instant>>,
    error: Arc<std::sync::Mutex<Option<String>>>,
}
impl LuaRun {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn is_running(&self) -> bool {
        !self.finished.load(Ordering::Relaxed)
    }
    /// True only when a frame callback has remained inside Lua/native vision for
    /// several seconds. Waiting for a new (possibly static-screen) frame is not a stall.
    pub fn is_stalled(&self) -> bool {
        self.is_running()
            && self.processing.load(Ordering::Relaxed)
            && self.last_progress.lock().unwrap().elapsed() > Duration::from_secs(3)
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}
impl Drop for LuaRun {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn run(
    source: String,
    serial: String,
    events: tokio::sync::broadcast::Sender<ForgeEvent>,
) -> LuaRun {
    let id = Uuid::new_v4();
    let cancelled = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let error = Arc::new(std::sync::Mutex::new(None));
    let error_out = error.clone();
    let done = finished.clone();
    let token = cancelled.clone();
    let runtime = Handle::current();
    std::thread::spawn(move || {
        let _ = events.send(ForgeEvent::ScriptStarted {
            run_id: id,
            serial: serial.clone(),
        });
        let result = execute(&source, &serial, id, token, events.clone(), runtime);
        let message = result.err().map(|e| e.to_string());
        *error_out.lock().unwrap() = message.clone();
        let _ = events.send(ForgeEvent::ScriptStopped {
            run_id: id,
            error: message,
        });
        done.store(true, Ordering::Relaxed);
    });
    LuaRun {
        id,
        cancelled,
        finished,
        processing: Arc::new(AtomicBool::new(false)),
        last_progress: Arc::new(std::sync::Mutex::new(Instant::now())),
        error,
    }
}

/// Start a latency-first frame-driven Lua script. While Lua processes a frame,
/// newer frames replace the pending slot so the next callback is always current.
pub fn run_frames(
    source: String,
    script_dir: Option<PathBuf>,
    session: Arc<ScrcpySession>,
    events: tokio::sync::broadcast::Sender<ForgeEvent>,
) -> (LuaRun, tokio::sync::watch::Sender<Option<Arc<VideoFrame>>>) {
    let serial = session.serial.clone();
    let id = Uuid::new_v4();
    let cancelled = Arc::new(AtomicBool::new(false));
    let token = cancelled.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let done = finished.clone();
    let processing = Arc::new(AtomicBool::new(false));
    let processing_out = processing.clone();
    let last_progress = Arc::new(std::sync::Mutex::new(Instant::now()));
    let progress_out = last_progress.clone();
    let error = Arc::new(std::sync::Mutex::new(None));
    let error_out = error.clone();
    let (tx, mut rx) = tokio::sync::watch::channel::<Option<Arc<VideoFrame>>>(None);
    let runtime = Handle::current();
    std::thread::spawn(move || {
        let _ = events.send(ForgeEvent::ScriptStarted {
            run_id: id,
            serial: serial.clone(),
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let lua = Lua::new();
            let deadline = Arc::new(std::sync::Mutex::new(None));
            install_limits(&lua, token.clone(), Some(deadline.clone()))?;
            let forge = lua.create_table()?;
            forge.set(
                "serial",
                lua.create_function({
                    let serial = serial.clone();
                    move |_, ()| Ok(serial.clone())
                })?,
            )?;
            let clock = std::time::Instant::now();
            forge.set(
                "monotonic_ms",
                lua.create_function(move |_, ()| Ok(clock.elapsed().as_secs_f64() * 1000.0))?,
            )?;
            add_script_paths(&lua, &forge, script_dir)?;
            forge.set(
                "log",
                lua.create_function({
                    let events = events.clone();
                    move |_, message: String| {
                        let _ = events.send(ForgeEvent::ScriptLog {
                            run_id: id,
                            message,
                        });
                        Ok(())
                    }
                })?,
            )?;
            for (name, value) in [
                ("KEY_BACK", 4),
                ("KEY_HOME", 3),
                ("KEY_ENTER", 66),
                ("KEY_POWER", 26),
                ("KEY_VOLUME_UP", 24),
                ("KEY_VOLUME_DOWN", 25),
                ("KEY_MENU", 82),
            ] {
                forge.set(name, value)?
            }
            forge.set("device_name", session.device_name.clone())?;
            add_wait(&lua, &forge, token.clone())?;
            add_session_calls(
                &lua,
                &forge,
                session.clone(),
                token.clone(),
                runtime.clone(),
            )?;
            lua.globals().set("forge", forge)?;
            lua.load(include_str!("lua_vision.lua"))
                .set_name("scrcpyforge.vision")
                .exec()?;
            lua.load(&source).set_name("frame_automation").exec()?;
            let callback: mlua::Function = lua
                .globals()
                .get("on_frame")
                .map_err(|_| anyhow::anyhow!("Lua script must define on_frame(frame)"))?;
            let current = Arc::new(std::sync::RwLock::new(None));
            let table = create_frame_table(&lua, current.clone())?;
            loop {
                if runtime.block_on(rx.changed()).is_err() {
                    break;
                }
                ensure_running(&token)?;
                let Some(frame) = rx.borrow_and_update().clone() else {
                    continue;
                };
                *current.write().unwrap() = Some(frame.clone());
                table.set("width", frame.width)?;
                table.set("height", frame.height)?;
                table.set("pts_us", frame.presentation_time_us)?;
                let started = Instant::now();
                *progress_out.lock().unwrap() = started;
                *deadline.lock().unwrap() = Some(started + Duration::from_secs(5));
                processing_out.store(true, Ordering::Relaxed);
                let callback_result = callback.call::<()>(table.clone());
                processing_out.store(false, Ordering::Relaxed);
                *deadline.lock().unwrap() = None;
                *progress_out.lock().unwrap() = Instant::now();
                callback_result?;
                session.frames.record_script(started.elapsed());
            }
            Ok(())
        }));
        let result = result.unwrap_or_else(|panic| {
            Err(anyhow::anyhow!(
                "Lua worker panicked: {}",
                if let Some(v) = panic.downcast_ref::<&str>() {
                    *v
                } else if let Some(v) = panic.downcast_ref::<String>() {
                    v.as_str()
                } else {
                    "unknown panic"
                }
            ))
        });
        let message = result.err().map(|e| e.to_string());
        if let Some(ref value) = message {
            tracing::warn!(run_id=%id,error=%value,"Lua frame script stopped")
        }
        *error_out.lock().unwrap() = message.clone();
        let _ = events.send(ForgeEvent::ScriptStopped {
            run_id: id,
            error: message,
        });
        done.store(true, Ordering::Relaxed);
    });
    (
        LuaRun {
            id,
            cancelled,
            finished,
            processing,
            last_progress,
            error,
        },
        tx,
    )
}

fn parse_roi(roi: Option<Table>) -> mlua::Result<Option<(u32, u32, u32, u32)>> {
    roi.map(|v| Ok((v.get(1)?, v.get(2)?, v.get(3)?, v.get(4)?)))
        .transpose()
}
fn match_table(lua: &Lua, m: &crate::cv::Match) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("x", m.x)?;
    t.set("y", m.y)?;
    t.set("w", m.width)?;
    t.set("h", m.height)?;
    t.set("confidence", m.confidence)?;
    Ok(t)
}
type CurrentFrame = Arc<std::sync::RwLock<Option<Arc<VideoFrame>>>>;
fn current_frame(value: &CurrentFrame) -> mlua::Result<Arc<VideoFrame>> {
    value
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| LuaError::RuntimeError("frame unavailable".into()))
}
fn create_frame_table(lua: &Lua, current: CurrentFrame) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "pixel",
        lua.create_function({
            let c = current.clone();
            move |_, (_, x, y): (Table, u32, u32)| {
                let f = current_frame(&c)?;
                if x >= f.width || y >= f.height {
                    return Err(LuaError::RuntimeError("pixel out of bounds".into()));
                }
                let rgb = f.rgb().map_err(LuaError::external)?;
                let i = (y * f.width * 3 + x * 3) as usize;
                Ok((rgb[i], rgb[i + 1], rgb[i + 2], 255u8))
            }
        })?,
    )?;
    for (name, mode) in [
        ("find", 0),
        ("find_fast", 1),
        ("find_gray", 2),
        ("find_fast_gray", 3),
    ] {
        table.set(name,lua.create_function({let c=current.clone();move|lua,(_,path,threshold,roi):(Table,String,Option<f32>,Option<Table>)|{let f=current_frame(&c)?;let roi=parse_roi(roi)?;let found=match mode{1=>crate::cv::find_fast(&f,Path::new(&path),threshold.unwrap_or(0.8),roi),2=>crate::cv::find_gray(&f,Path::new(&path),threshold.unwrap_or(0.8),roi,false),3=>crate::cv::find_gray(&f,Path::new(&path),threshold.unwrap_or(0.8),roi,true),_=>crate::cv::find(&f,Path::new(&path),threshold.unwrap_or(0.8),roi)}.map_err(LuaError::external)?;found.map(|m|match_table(lua,&m)).transpose()}})?)?;
    }
    table.set(
        "find_first",
        lua.create_function({
            let c = current.clone();
            move |lua, (_, paths, threshold, roi): (Table, Table, Option<f32>, Option<Table>)| {
                let f = current_frame(&c)?;
                let paths = paths
                    .sequence_values::<String>()
                    .map(|v| v.map(PathBuf::from))
                    .collect::<mlua::Result<Vec<_>>>()?;
                match crate::cv::find_first(&f, &paths, threshold.unwrap_or(0.8), parse_roi(roi)?)
                    .map_err(LuaError::external)?
                {
                    Some((index, m)) => {
                        let result = match_table(lua, &m)?;
                        result.set("index", index + 1)?;
                        Ok(Some(result))
                    }
                    None => Ok(None),
                }
            }
        })?,
    )?;
    table.set(
        "find_multiscale",
        lua.create_function({
            let c = current.clone();
            move |lua, (_, path, threshold, roi): (Table, String, Option<f32>, Option<Table>)| {
                let f = current_frame(&c)?;
                crate::cv::find_multiscale(
                    &f,
                    Path::new(&path),
                    threshold.unwrap_or(0.8),
                    parse_roi(roi)?,
                )
                .map_err(LuaError::external)?
                .map(|m| match_table(lua, &m))
                .transpose()
            }
        })?,
    )?;
    table.set(
        "find_all",
        lua.create_function({
            let c = current.clone();
            move |lua, (_, path, threshold, roi): (Table, String, Option<f32>, Option<Table>)| {
                let f = current_frame(&c)?;
                let found = crate::cv::find_all(
                    &f,
                    Path::new(&path),
                    threshold.unwrap_or(0.8),
                    parse_roi(roi)?,
                )
                .map_err(LuaError::external)?;
                let list = lua.create_table()?;
                for (i, m) in found.iter().enumerate() {
                    list.set(i + 1, match_table(lua, m)?)?
                }
                Ok(list)
            }
        })?,
    )?;
    table.set(
        "save",
        lua.create_function({
            let c = current.clone();
            move |_, (_, path): (Table, String)| {
                let f = current_frame(&c)?;
                crate::cv::save(&f, Path::new(&path)).map_err(LuaError::external)
            }
        })?,
    )?;
    table.set(
        "crop",
        lua.create_function(
            move |_, (_, path, x1, y1, x2, y2): (Table, String, u32, u32, u32, u32)| {
                let f = current_frame(&current)?;
                crate::cv::crop(&f, Path::new(&path), (x1, y1, x2, y2)).map_err(LuaError::external)
            },
        )?,
    )?;
    Ok(table)
}

fn add_script_paths(lua: &Lua, forge: &Table, script_dir: Option<PathBuf>) -> mlua::Result<()> {
    if let Some(dir) = script_dir {
        forge.set("script_dir", dir.to_string_lossy().into_owned())?;
        forge.set(
            "asset",
            lua.create_function(move |_, relative: String| {
                let path = Path::new(&relative);
                if path.as_os_str().is_empty()
                    || path.is_absolute()
                    || path.components().any(|part| {
                        matches!(
                            part,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    })
                {
                    return Err(LuaError::RuntimeError(
                        "forge.asset() only accepts a relative path inside the script directory"
                            .into(),
                    ));
                }
                Ok(dir.join(path).to_string_lossy().into_owned())
            })?,
        )?;
    } else {
        forge.set(
            "asset",
            lua.create_function(|_, _: String| {
                Err::<String, _>(LuaError::RuntimeError(
                    "forge.asset() is available to named scripts only".into(),
                ))
            })?,
        )?;
    }
    Ok(())
}

fn execute(
    source: &str,
    serial: &str,
    run_id: Uuid,
    cancelled: Arc<AtomicBool>,
    events: tokio::sync::broadcast::Sender<ForgeEvent>,
    runtime: Handle,
) -> Result<()> {
    let lua = Lua::new();
    install_limits(&lua, cancelled.clone(), None)?;
    let forge = lua.create_table()?;
    forge.set(
        "serial",
        lua.create_function({
            let serial = serial.to_owned();
            move |_, ()| Ok(serial.clone())
        })?,
    )?;
    forge.set(
        "log",
        lua.create_function(move |_, message: String| {
            let _ = events.send(ForgeEvent::ScriptLog { run_id, message });
            Ok(())
        })?,
    )?;
    add_wait(&lua, &forge, cancelled.clone())?;
    add_adb_calls(&lua, &forge, serial.to_owned(), cancelled, runtime)?;
    lua.globals().set("forge", forge)?;
    lua.load(source).set_name("automation").exec()?;
    Ok(())
}

fn install_limits(
    lua: &Lua,
    cancelled: Arc<AtomicBool>,
    deadline: Option<Arc<std::sync::Mutex<Option<Instant>>>>,
) -> mlua::Result<()> {
    lua.set_memory_limit(64 * 1024 * 1024)?;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            if cancelled.load(Ordering::Relaxed) {
                return Err(LuaError::RuntimeError("script cancelled".into()));
            }
            if deadline
                .as_ref()
                .and_then(|value| *value.lock().unwrap())
                .is_some_and(|limit| Instant::now() >= limit)
            {
                return Err(LuaError::RuntimeError(
                    "frame callback exceeded 5 second hard limit".into(),
                ));
            }
            Ok(VmState::Continue)
        },
    );
    Ok(())
}

fn ensure_running(cancelled: &AtomicBool) -> mlua::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err(LuaError::RuntimeError("script cancelled".into()))
    } else {
        Ok(())
    }
}

fn add_wait(lua: &Lua, forge: &Table, cancelled: Arc<AtomicBool>) -> mlua::Result<()> {
    forge.set(
        "wait",
        lua.create_function(move |_, millis: u64| {
            let mut remaining = millis;
            while remaining > 0 {
                ensure_running(&cancelled)?;
                let step = remaining.min(50);
                std::thread::sleep(Duration::from_millis(step));
                remaining -= step;
            }
            Ok(())
        })?,
    )
}

fn add_adb_calls(
    lua: &Lua,
    forge: &Table,
    serial: String,
    cancelled: Arc<AtomicBool>,
    runtime: Handle,
) -> mlua::Result<()> {
    let call = move |action: InputAction| -> mlua::Result<()> {
        ensure_running(&cancelled)?;
        runtime
            .block_on(Adb::default().input(&serial, &action))
            .map_err(LuaError::external)
    };
    let shared = Arc::new(call);
    forge.set(
        "tap",
        lua.create_function({
            let call = shared.clone();
            move |_, (x, y, radius): (i32, i32, Option<u32>)| {
                let (x, y) = random_point(x, y, radius.unwrap_or(0));
                call(InputAction::Tap { x, y })
            }
        })?,
    )?;
    forge.set(
        "swipe",
        lua.create_function({
            let call = shared.clone();
            move |_, (x1, y1, x2, y2, duration_ms): (i32, i32, i32, i32, u64)| {
                call(InputAction::Swipe {
                    x1,
                    y1,
                    x2,
                    y2,
                    duration_ms,
                })
            }
        })?,
    )?;
    forge.set(
        "text",
        lua.create_function({
            let call = shared.clone();
            move |_, value: String| call(InputAction::Text { value })
        })?,
    )?;
    forge.set(
        "key",
        lua.create_function({
            let call = shared.clone();
            move |_, code: i32| call(InputAction::Key { code })
        })?,
    )?;
    Ok(())
}

fn add_session_calls(
    lua: &Lua,
    forge: &Table,
    session: Arc<ScrcpySession>,
    cancelled: Arc<AtomicBool>,
    runtime: Handle,
) -> mlua::Result<()> {
    let check = {
        let cancelled = cancelled.clone();
        move || ensure_running(&cancelled)
    };
    forge.set(
        "tap",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            let check = check.clone();
            move |_, (x, y, radius): (i32, i32, Option<u32>)| {
                check()?;
                r.block_on(s.tap_random(x, y, radius.unwrap_or(0)))
                    .map_err(LuaError::external)
            }
        })?,
    )?;
    forge.set(
        "swipe",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            let check = check.clone();
            move |_, (x1, y1, x2, y2, d): (i32, i32, i32, i32, Option<u64>)| {
                check()?;
                r.block_on(s.swipe(x1, y1, x2, y2, d.unwrap_or(300)))
                    .map_err(LuaError::external)
            }
        })?,
    )?;
    forge.set(
        "long_press",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            let check = check.clone();
            move |_, (x, y, d): (i32, i32, u64)| {
                check()?;
                r.block_on(s.long_press(x, y, d))
                    .map_err(LuaError::external)
            }
        })?,
    )?;
    forge.set(
        "multi_tap",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            let check = check.clone();
            move |_, points: Table| {
                check()?;
                let mut parsed = vec![];
                for pair in points.sequence_values::<Table>() {
                    let pair = pair?;
                    parsed.push((pair.get(1)?, pair.get(2)?))
                }
                r.block_on(s.multi_tap(&parsed)).map_err(LuaError::external)
            }
        })?,
    )?;
    forge.set(
        "press_key",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            let check = check.clone();
            move |_, (code, long): (u32, Option<bool>)| {
                check()?;
                r.block_on(s.key(code, long.unwrap_or(false)))
                    .map_err(LuaError::external)
            }
        })?,
    )?;
    forge.set(
        "press_back",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            let check = check.clone();
            move |_, ()| {
                check()?;
                r.block_on(s.back()).map_err(LuaError::external)
            }
        })?,
    )?;
    forge.set(
        "input_text",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            move |_, value: String| r.block_on(s.text(&value)).map_err(LuaError::external)
        })?,
    )?;
    forge.set(
        "screen_size",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            move |_, ()| Ok(r.block_on(async { *s.size.read().await }))
        })?,
    )?;
    forge.set(
        "performance_profile",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            move |_, ()| {
                Ok(match r.block_on(s.frames.profile()) {
                    crate::session::PerformanceProfile::Auto => "auto",
                    crate::session::PerformanceProfile::Eco => "eco",
                    crate::session::PerformanceProfile::Balanced => "balanced",
                    crate::session::PerformanceProfile::Realtime => "realtime",
                })
            }
        })?,
    )?;
    forge.set(
        "recommended_interval_ms",
        lua.create_function({
            let s = session.clone();
            let r = runtime.clone();
            move |_, ()| {
                Ok(match r.block_on(s.frames.profile()) {
                    crate::session::PerformanceProfile::Realtime => 0,
                    crate::session::PerformanceProfile::Balanced => 30,
                    crate::session::PerformanceProfile::Eco => 150,
                    crate::session::PerformanceProfile::Auto => 50,
                })
            }
        })?,
    )?;
    Ok(())
}

fn random_point(x: i32, y: i32, radius: u32) -> (i32, i32) {
    use rand::Rng;
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
    use super::*;

    #[test]
    fn declarative_vision_batches_targets_and_dispatches_action() -> mlua::Result<()> {
        let lua = Lua::new();
        let forge = lua.create_table()?;
        forge.set("monotonic_ms", lua.create_function(|_, ()| Ok(1000.0))?)?;
        forge.set(
            "tap",
            lua.create_function(|lua, (x, y, radius): (i32, i32, u32)| {
                let tap = lua.create_table()?;
                tap.set(1, x)?;
                tap.set(2, y)?;
                tap.set(3, radius)?;
                lua.globals().set("last_tap", tap)
            })?,
        )?;
        lua.globals().set("forge", forge)?;
        lua.load(include_str!("lua_vision.lua")).exec()?;
        lua.load(
            r#"
            forge.vision {
                threshold = 0.9,
                action = {type = "tap", radius = 5},
                targets = {
                    {name = "first", template = "first.png"},
                    {name = "second", template = "second.png"},
                },
            }
        "#,
        )
        .exec()?;

        let frame = lua.create_table()?;
        frame.set(
            "find_first",
            lua.create_function(
                |lua, (_frame, paths, threshold, _roi): (Table, Table, f32, Option<Table>)| {
                    assert_eq!(paths.raw_len(), 2);
                    assert!((threshold - 0.9).abs() < f32::EPSILON);
                    let matched = lua.create_table()?;
                    matched.set("x", 120)?;
                    matched.set("y", 80)?;
                    matched.set("confidence", 0.95)?;
                    matched.set("index", 2)?;
                    Ok(matched)
                },
            )?,
        )?;
        let on_frame: mlua::Function = lua.globals().get("on_frame")?;
        on_frame.call::<()>(frame)?;
        let tap: Table = lua.globals().get("last_tap")?;
        let (x, y, radius): (i32, i32, u32) = (tap.get(1)?, tap.get(2)?, tap.get(3)?);
        assert_eq!((x, y, radius), (120, 80, 5));
        Ok(())
    }

    #[test]
    fn asset_paths_are_portable_and_cannot_escape_script_directory() -> mlua::Result<()> {
        let lua = Lua::new();
        let forge = lua.create_table()?;
        add_script_paths(&lua, &forge, Some(PathBuf::from("/scripts/demo")))?;
        lua.globals().set("forge", forge)?;
        let value: String = lua
            .load(r#"return forge.asset("images/button.png")"#)
            .eval()?;
        assert_eq!(value, "/scripts/demo/images/button.png");
        assert!(lua
            .load(r#"return forge.asset("../secret")"#)
            .eval::<String>()
            .is_err());
        Ok(())
    }

    #[test]
    fn instruction_hook_interrupts_cancelled_infinite_loop() -> mlua::Result<()> {
        let lua = Lua::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        install_limits(&lua, cancelled.clone(), None)?;
        cancelled.store(true, Ordering::Relaxed);
        assert!(lua.load("while true do end").exec().is_err());
        Ok(())
    }
}
