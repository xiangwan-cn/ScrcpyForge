use eframe::egui::{self, ColorImage, Context, TextureHandle};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_API: &str = "http://127.0.0.1:27180/api/v1";

#[derive(Clone, Deserialize)]
struct Device {
    serial: String,
    state: String,
    model: Option<String>,
    wireless: bool,
}
#[derive(Clone, Deserialize)]
struct PairingService {
    name: String,
    endpoint: String,
}
#[derive(Clone, Deserialize)]
struct ScriptRun {
    run_id: String,
    serial: String,
    name: Option<String>,
    running: bool,
    #[serde(default)]
    stalled: bool,
}
#[derive(Clone, Deserialize, Default)]
struct Metrics {
    decoded_fps: f64,
    preview_fps: f64,
    script_fps: f64,
    average_script_ms: f64,
    script_p50_ms: f64,
    script_p95_ms: f64,
    dropped_script_frames: u64,
    #[serde(default)]
    script_rescans: u64,
    #[serde(default)]
    last_publish_us: u64,
    #[serde(default)]
    input_failures: u64,
    #[serde(default)]
    last_input_us: u64,
    #[serde(default)]
    video_packet_errors: u64,
    #[serde(default)]
    video_decode_errors: u64,
    #[serde(default)]
    latest_frame_seq: u64,
    #[serde(default)]
    preview_dropped_frames: u64,
    #[serde(default)]
    preview_leases: u64,
    #[serde(default)]
    activity_state: String,
    profile: String,
    preview_profile: String,
}
#[derive(Clone, Deserialize)]
struct SessionSnapshot {
    serial: String,
    metrics: Metrics,
}
#[derive(Deserialize)]
struct StateSnapshot {
    devices: Vec<Device>,
    sessions: Vec<SessionSnapshot>,
    runs: Vec<ScriptRun>,
    scripts: Vec<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Realtime,
    FiveSeconds,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Inspect {
    None,
    Point,
    Region,
}
impl Mode {
    fn api(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Realtime => "realtime",
            Self::FiveSeconds => "five_seconds",
        }
    }
}
enum Command {
    Refresh,
    Connect(String),
    DiscoverPairingServices,
    Pair(String, String),
    StartSession(String),
    StopSession(String),
    StartAll,
    StopAllSessions,
    SetMode(String, Mode),
    SetScriptProfile(String, String),
    SetPreviewProfile(String, String),
    RunScript(String, String),
    StopScript(String),
    StopAllScripts,
    SaveRegion(String, String, u32, u32, u32, u32),
}
enum Event {
    Devices(Vec<Device>),
    PairingServices(Result<Vec<PairingService>, String>),
    PairingFinished(Result<(), String>),
    Scripts(Vec<String>),
    Runs(Vec<ScriptRun>),
    Sessions(Vec<String>),
    Metrics(HashMap<String, Metrics>),
    Saved(String, String),
    Status(String),
}

struct DecodedFrame {
    size: [usize; 2],
    rgba: Vec<u8>,
}
type LatestFrames = Arc<Mutex<HashMap<String, DecodedFrame>>>;

fn store_decoded_frame(latest_frames: &LatestFrames, serial: String, frame: DecodedFrame) {
    latest_frames
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(serial, frame);
}

fn store_latest_frame(latest_frames: &LatestFrames, serial: String, bytes: Vec<u8>) -> bool {
    if let Ok(image) = image::load_from_memory(&bytes) {
        let rgba = image.to_rgba8();
        let decoded = DecodedFrame {
            size: [rgba.width() as usize, rgba.height() as usize],
            rgba: rgba.into_raw(),
        };
        store_decoded_frame(latest_frames, serial, decoded);
        true
    } else {
        false
    }
}

struct App {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<Event>,
    latest_frames: LatestFrames,
    devices: Vec<Device>,
    scripts: Vec<String>,
    runs: HashMap<String, ScriptRun>,
    sessions: HashSet<String>,
    selected: HashMap<String, String>,
    textures: HashMap<String, TextureHandle>,
    modes: HashMap<String, Mode>,
    metrics: HashMap<String, Metrics>,
    inspect: HashMap<String, Inspect>,
    points: HashMap<String, (u32, u32)>,
    paths: HashMap<String, String>,
    drag_starts: HashMap<String, egui::Pos2>,
    endpoint: String,
    pairing_open: bool,
    pairing_endpoint: String,
    pairing_code: String,
    pairing_services: Vec<PairingService>,
    pairing_status: String,
    pairing_busy: bool,
    status: String,
    last_tick: Instant,
}
impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure(&cc.egui_ctx);
        let (commands, rx) = mpsc::channel();
        let (tx, events) = mpsc::channel();
        let latest_frames = LatestFrames::default();
        spawn_backend(rx, tx, latest_frames.clone(), cc.egui_ctx.clone());
        let _ = commands.send(Command::Refresh);
        Self {
            commands,
            events,
            latest_frames,
            devices: vec![],
            scripts: vec![],
            runs: HashMap::new(),
            sessions: HashSet::new(),
            selected: HashMap::new(),
            textures: HashMap::new(),
            modes: HashMap::new(),
            metrics: HashMap::new(),
            inspect: HashMap::new(),
            points: HashMap::new(),
            paths: HashMap::new(),
            drag_starts: HashMap::new(),
            endpoint: String::new(),
            pairing_open: false,
            pairing_endpoint: String::new(),
            pairing_code: String::new(),
            pairing_services: vec![],
            pairing_status: String::new(),
            pairing_busy: false,
            status: "正在连接后端…".into(),
            last_tick: Instant::now(),
        }
    }
    fn receive(&mut self, ctx: &Context) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                Event::Devices(v) => self.devices = v,
                Event::PairingServices(result) => match result {
                    Ok(services) => {
                        self.pairing_services = services;
                        if let Some(service) = self.pairing_services.first() {
                            self.pairing_endpoint = service.endpoint.clone();
                            self.pairing_status =
                                format!("发现 {} 个配对服务", self.pairing_services.len());
                        } else {
                            self.pairing_status = "未自动发现设备，请手动输入手机显示的地址".into();
                        }
                    }
                    Err(error) => {
                        self.pairing_services.clear();
                        self.pairing_status = format!("搜索失败：{error}");
                    }
                },
                Event::PairingFinished(result) => {
                    self.pairing_busy = false;
                    self.pairing_code.clear();
                    match result {
                        Ok(()) => {
                            self.pairing_open = false;
                            self.status = "无线调试配对成功".into();
                            let _ = self.commands.send(Command::Refresh);
                        }
                        Err(error) => {
                            self.pairing_status = format!("配对失败：{error}");
                        }
                    }
                }
                Event::Scripts(v) => self.scripts = v,
                Event::Runs(v) => {
                    self.runs = v
                        .into_iter()
                        .filter(|r| r.running)
                        .map(|r| (r.serial.clone(), r))
                        .collect()
                }
                Event::Sessions(v) => {
                    for serial in &v {
                        self.modes.entry(serial.clone()).or_insert(Mode::Realtime);
                    }
                    self.sessions = v.into_iter().collect();
                    self.textures
                        .retain(|serial, _| self.sessions.contains(serial));
                }
                Event::Metrics(v) => self.metrics = v,
                Event::Status(v) => self.status = v,
                Event::Saved(serial, path) => {
                    self.status = format!("{serial} 模板已保存：{path}");
                }
            }
        }
        let frames = {
            let mut latest = self
                .latest_frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *latest)
        };
        changed |= !frames.is_empty();
        for (serial, frame) in frames {
            let color = ColorImage::from_rgba_unmultiplied(frame.size, &frame.rgba);
            if let Some(texture) = self.textures.get_mut(&serial) {
                texture.set(color, egui::TextureOptions::LINEAR)
            } else {
                self.textures.insert(
                    serial.clone(),
                    ctx.load_texture(
                        format!("device-{serial}"),
                        color,
                        egui::TextureOptions::LINEAR,
                    ),
                );
            }
        }
        changed
    }
}

impl eframe::App for App {
    #[allow(clippy::possible_missing_else)]
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let window_active = viewport_active(&ctx);
        if self.receive(&ctx) && window_active {
            ctx.request_repaint();
        }
        if window_active && !self.pairing_busy && self.last_tick.elapsed() > Duration::from_secs(2)
        {
            let _ = self.commands.send(Command::Refresh);
            self.last_tick = Instant::now();
        }
        egui::Frame::NONE.show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.heading("ScrcpyForge");
                ui.separator();
                ui.label(&self.status);
                if ui.button("刷新").clicked() {
                    let _ = self.commands.send(Command::Refresh);
                }
                if ui.button("启动全部会话").clicked() {
                    let _ = self.commands.send(Command::StartAll);
                }
                if ui.button("停止全部会话").clicked() {
                    let _ = self.commands.send(Command::StopAllSessions);
                }
                if ui.button("停止全部脚本").clicked() {
                    let _ = self.commands.send(Command::StopAllScripts);
                }
            });
            ui.horizontal(|ui| {
                ui.label("无线设备");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.endpoint)
                        .id_source("wireless-endpoint")
                        .hint_text("192.168.1.10:端口")
                        .desired_width(220.0),
                );
                let submit = ui.button("连接").clicked()
                    || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if submit && !self.endpoint.trim().is_empty() {
                    let _ = self
                        .commands
                        .send(Command::Connect(self.endpoint.trim().to_owned()));
                }
                if ui.button("无线配对").clicked() {
                    self.pairing_open = true;
                    self.pairing_code.clear();
                    self.pairing_status = "正在搜索附近的配对服务…".into();
                    let _ = self.commands.send(Command::DiscoverPairingServices);
                }
            });
            ui.add_space(6.0);
        });
        if self.pairing_open {
            self.pairing_code
                .retain(|character| character.is_ascii_digit());
            self.pairing_code.truncate(self.pairing_code.len().min(6));
            let mut open = self.pairing_open;
            egui::Window::new("无线调试配对")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(420.0)
                .show(&ctx, |ui| {
                    ui.label("先在 Android 的“无线调试”中选择“使用配对码配对设备”。");
                    ui.small("配对端口与连接端口通常不同，成功后会自动查找连接端口。");
                    ui.add_space(8.0);
                    ui.label("发现的配对服务");
                    let selected = self
                        .pairing_services
                        .iter()
                        .find(|service| service.endpoint == self.pairing_endpoint)
                        .map(|service| format!("{} · {}", service.name, service.endpoint))
                        .unwrap_or_else(|| "手动输入配对地址".into());
                    egui::ComboBox::from_id_salt("pairing-service")
                        .selected_text(selected)
                        .width(390.0)
                        .show_ui(ui, |ui| {
                            for service in self.pairing_services.clone() {
                                let label = format!("{} · {}", service.name, service.endpoint);
                                if ui
                                    .selectable_label(
                                        self.pairing_endpoint == service.endpoint,
                                        label,
                                    )
                                    .clicked()
                                {
                                    self.pairing_endpoint = service.endpoint;
                                }
                            }
                        });
                    if ui
                        .add_enabled(!self.pairing_busy, egui::Button::new("重新搜索"))
                        .clicked()
                    {
                        self.pairing_status = "正在搜索附近的配对服务…".into();
                        let _ = self.commands.send(Command::DiscoverPairingServices);
                    }
                    ui.add_space(6.0);
                    ui.label("配对地址");
                    ui.add_enabled(
                        !self.pairing_busy,
                        egui::TextEdit::singleline(&mut self.pairing_endpoint)
                            .id_source("pairing-endpoint")
                            .hint_text("192.168.1.10:配对端口")
                            .desired_width(390.0),
                    );
                    ui.label("六位配对码");
                    ui.add_enabled(
                        !self.pairing_busy,
                        egui::TextEdit::singleline(&mut self.pairing_code)
                            .id_source("pairing-code")
                            .password(true)
                            .char_limit(6)
                            .hint_text("000000")
                            .desired_width(180.0),
                    );
                    if !self.pairing_status.is_empty() {
                        ui.small(&self.pairing_status);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let valid = !self.pairing_busy
                            && endpoint_looks_valid(self.pairing_endpoint.trim())
                            && self.pairing_code.len() == 6;
                        if ui
                            .add_enabled(
                                valid,
                                egui::Button::new(if self.pairing_busy {
                                    "正在配对…"
                                } else {
                                    "配对"
                                }),
                            )
                            .clicked()
                        {
                            let endpoint = self.pairing_endpoint.trim().to_owned();
                            let code = std::mem::take(&mut self.pairing_code);
                            self.pairing_busy = true;
                            self.pairing_status = "正在配对并查找连接端口…".into();
                            let _ = self.commands.send(Command::Pair(endpoint, code));
                        }
                        if ui
                            .add_enabled(!self.pairing_busy, egui::Button::new("取消"))
                            .clicked()
                        {
                            self.pairing_code.clear();
                            self.pairing_open = false;
                        }
                    });
                });
            if !open {
                self.pairing_code.clear();
            }
            self.pairing_open &= open;
        }
        egui::Frame::central_panel(ui.style()).show(ui,|ui|{egui::ScrollArea::vertical().show(ui,|ui|{if self.devices.is_empty(){ui.vertical_centered(|ui|{ui.add_space(80.0);ui.heading("未发现设备");ui.label("请连接 USB 或无线 ADB 设备");});return;}
   let columns=if ui.available_width()<620.0{1}else if ui.available_width()<980.0{2}else{3};let width=(ui.available_width()-12.0*(columns-1)as f32)/columns as f32;
   for row in self.devices.chunks(columns){ui.horizontal_top(|ui|{for device in row{let serial=&device.serial;let session_running=self.sessions.contains(serial);let run=self.runs.get(serial);
    ui.allocate_ui_with_layout(egui::vec2(width,ui.available_height()),egui::Layout::top_down(egui::Align::Center),|ui|{egui::Frame::group(ui.style()).show(ui,|ui|{ui.set_width(width-16.0);let aspect=self.textures.get(serial).map(|t|t.aspect_ratio()).unwrap_or(0.5);let h=((width-24.0)/aspect).min(520.0);
     if let Some(texture)=self.textures.get(serial){let pixels=texture.size();let response=ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(width-24.0,h)).sense(egui::Sense::click_and_drag()));let inspect=*self.inspect.entry(serial.clone()).or_insert(Inspect::None);if inspect==Inspect::Point&&response.clicked(){if let Some(pos)=response.interact_pointer_pos(){self.points.insert(serial.clone(),screen_to_pixel(pos,response.rect,pixels));}}
      if inspect==Inspect::Region{if response.drag_started(){if let Some(pos)=response.interact_pointer_pos(){self.drag_starts.insert(serial.clone(),pos);}}if let(Some(start),Some(current))=(self.drag_starts.get(serial).copied(),response.interact_pointer_pos()){let rect=egui::Rect::from_two_pos(start,current).intersect(response.rect);ui.painter().rect_stroke(rect,0.0_f32,egui::Stroke::new(2.0_f32,egui::Color32::LIGHT_GREEN),egui::StrokeKind::Inside);}if response.drag_stopped(){if let(Some(start),Some(end))=(self.drag_starts.remove(serial),response.interact_pointer_pos()){let(a,b)=(screen_to_pixel(start,response.rect,pixels),screen_to_pixel(end,response.rect,pixels));let(x1,x2)=(a.0.min(b.0),a.0.max(b.0));let(y1,y2)=(a.1.min(b.1),a.1.max(b.1));if x2>x1&&y2>y1{let path=self.paths.entry(serial.clone()).or_insert_with(||default_template_name(serial)).clone();let _=self.commands.send(Command::SaveRegion(serial.clone(),path,x1,y1,x2,y2));}}}}
     }else{ui.allocate_ui(egui::vec2(width-24.0,h.min(360.0)),|ui|{ui.centered_and_justified(|ui|{ui.label(if session_running{"等待视频帧"}else{"会话未启动"});});});}
     ui.horizontal(|ui|{ui.strong(device.model.as_deref().unwrap_or(serial));ui.label(if device.wireless{"无线"}else{"USB"});ui.label(if session_running{"● 会话运行中"}else{"○ 会话停止"});});ui.small(format!("{} · {}",serial,device.state));
     ui.horizontal_wrapped(|ui|{if !session_running{if ui.button("启动会话").clicked(){let _=self.commands.send(Command::StartSession(serial.clone()));}}else if ui.button("停止会话").clicked(){let _=self.commands.send(Command::StopSession(serial.clone()));}let mode=self.modes.entry(serial.clone()).or_insert(Mode::Realtime);egui::ComboBox::from_id_salt(format!("mode-{serial}")).selected_text(match mode{Mode::Realtime=>"实时预览",Mode::FiveSeconds=>"五秒一图",Mode::Off=>"关闭预览"}).show_ui(ui,|ui|{for(value,label)in[(Mode::Realtime,"实时预览"),(Mode::FiveSeconds,"五秒一图"),(Mode::Off,"关闭预览")]{if ui.selectable_value(mode,value,label).changed(){let _=self.commands.send(Command::SetMode(serial.clone(),value));}}});});
     if let Some(metric)=self.metrics.get(serial){ui.small(format!("解码 {:.1} · 预览 {:.1} · 脚本 {:.1} FPS｜均值 {:.1}ms · P50 {:.1} · P95 {:.1} · 脚本丢帧 {} · 预览丢帧 {} · 重检 {} · 发布 {}μs · 输入 {}μs（失败 {}）· 视频错误 {} · 帧序 {} · {} 租约 {}",metric.decoded_fps,metric.preview_fps,metric.script_fps,metric.average_script_ms,metric.script_p50_ms,metric.script_p95_ms,metric.dropped_script_frames,metric.preview_dropped_frames,metric.script_rescans,metric.last_publish_us,metric.last_input_us,metric.input_failures,metric.video_packet_errors+metric.video_decode_errors,metric.latest_frame_seq,metric.activity_state,metric.preview_leases));ui.horizontal_wrapped(|ui|{let mut script_profile=metric.profile.clone();egui::ComboBox::from_id_salt(format!("script-profile-{serial}")).selected_text(format!("脚本：{}",script_profile)).show_ui(ui,|ui|{for(value,label)in[("auto","自动"),("eco","节能"),("balanced","均衡"),("realtime","实时")]{if ui.selectable_value(&mut script_profile,value.to_owned(),label).changed(){let _=self.commands.send(Command::SetScriptProfile(serial.clone(),value.to_owned()));}}});let mut preview_profile=metric.preview_profile.clone();egui::ComboBox::from_id_salt(format!("preview-profile-{serial}")).selected_text(format!("预览：{}",preview_profile)).show_ui(ui,|ui|{for(value,label)in[("auto","自动"),("eco","节能"),("balanced","均衡"),("realtime","实时")]{if ui.selectable_value(&mut preview_profile,value.to_owned(),label).changed(){let _=self.commands.send(Command::SetPreviewProfile(serial.clone(),value.to_owned()));}}});});}
     ui.horizontal_wrapped(|ui|{let inspect=self.inspect.entry(serial.clone()).or_insert(Inspect::None);if ui.selectable_label(*inspect==Inspect::Point,"点选坐标").clicked(){*inspect=if *inspect==Inspect::Point{Inspect::None}else{Inspect::Point};}if ui.selectable_label(*inspect==Inspect::Region,"框选模板").clicked(){*inspect=if *inspect==Inspect::Region{Inspect::None}else{Inspect::Region};}if let Some((x,y))=self.points.get(serial){ui.label(format!("坐标：{x}, {y}"));}});
     ui.horizontal(|ui|{ui.label("保存位置");ui.add(egui::TextEdit::singleline(self.paths.entry(serial.clone()).or_insert_with(||default_template_name(serial))).id_source(("template-path",serial)));});
     ui.separator();ui.horizontal_wrapped(|ui|{let default=self.scripts.first().cloned().unwrap_or_default();let choice=self.selected.entry(serial.clone()).or_insert(default);egui::ComboBox::from_id_salt(format!("script-{serial}")).selected_text(if choice.is_empty(){"无可用脚本"}else{choice.as_str()}).show_ui(ui,|ui|{for name in &self.scripts{ui.selectable_value(choice,name.clone(),name);}});if let Some(active)=run{ui.colored_label(if active.stalled{egui::Color32::LIGHT_RED}else{egui::Color32::LIGHT_GREEN},format!("{} {}",if active.stalled{"⚠ 疑似卡顿"}else{"●"},active.name.as_deref().unwrap_or("脚本"))).on_hover_text(format!("运行 ID: {}",active.run_id));if ui.button("停止脚本").clicked(){let _=self.commands.send(Command::StopScript(serial.clone()));}}else{let enabled=session_running&&!choice.is_empty();if ui.add_enabled(enabled,egui::Button::new("运行脚本")).clicked(){let _=self.commands.send(Command::RunScript(serial.clone(),choice.clone()));}}});
    });});}});ui.add_space(10.0);}
  });});
        if window_active {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

fn viewport_active(ctx: &Context) -> bool {
    ctx.input(|input| {
        input.viewport().visible().unwrap_or(true) && input.viewport().focused.unwrap_or(true)
    })
}

fn configure(ctx: &Context) {
    let font = std::env::var_os("SCRCPYFORGE_FONT")
        .map(std::path::PathBuf::from)
        .into_iter()
        .chain(
            [
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/droid-nonlatin/DroidSansFallbackFull.ttf",
                "/usr/share/fonts/droid-nonlatin/DroidSansFallback.ttf",
                "/System/Library/Fonts/PingFang.ttc",
                "C:\\Windows\\Fonts\\msyh.ttc",
            ]
            .into_iter()
            .map(Into::into),
        )
        .find(|path| path.is_file());
    if let Some(path) = font {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".into());
            ctx.set_fonts(fonts);
        }
    }
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(14, 17, 22);
    visuals.window_corner_radius = 12.into();
    ctx.set_visuals(visuals);
}
fn screen_to_pixel(pos: egui::Pos2, rect: egui::Rect, size: [usize; 2]) -> (u32, u32) {
    let x = ((pos.x - rect.left()) / rect.width() * size[0] as f32)
        .clamp(0.0, (size[0].saturating_sub(1)) as f32);
    let y = ((pos.y - rect.top()) / rect.height() * size[1] as f32)
        .clamp(0.0, (size[1].saturating_sub(1)) as f32);
    (x.round() as u32, y.round() as u32)
}
fn default_template_name(serial: &str) -> String {
    format!(
        "{}-template.png",
        serial
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    )
}
fn endpoint_looks_valid(endpoint: &str) -> bool {
    let parts = if let Some(rest) = endpoint.strip_prefix('[') {
        rest.split_once(']')
            .and_then(|(host, suffix)| suffix.strip_prefix(':').map(|port| (host, port)))
    } else {
        endpoint.rsplit_once(':')
    };
    parts.is_some_and(|(host, port)| {
        !host.is_empty()
            && !host.chars().any(char::is_whitespace)
            && port.parse::<u16>().is_ok_and(|value| value > 0)
    })
}
fn spawn_backend(
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    latest_frames: LatestFrames,
    ctx: Context,
) {
    thread::spawn(move || {
        let api = std::env::var("SCRCPYFORGE_API").unwrap_or_else(|_| DEFAULT_API.into());
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let mut modes: HashMap<String, (Mode, Instant)> = HashMap::new();
        let mut preview_profiles: HashMap<String, String> = HashMap::new();
        let mut streams: HashMap<String, Arc<AtomicBool>> = HashMap::new();
        let mut state_etag: Option<String> = None;
        let mut frame_etags: HashMap<String, String> = HashMap::new();
        loop {
            let command_wait = if viewport_active(&ctx) {
                Duration::from_millis(80)
            } else {
                Duration::from_millis(500)
            };
            match commands.recv_timeout(command_wait) {
                Ok(Command::Refresh) => {
                    match get_state::<StateSnapshot>(
                        &client,
                        &format!("{api}/state"),
                        &mut state_etag,
                    ) {
                        Ok(Some(snapshot)) => {
                            let StateSnapshot {
                                devices: d,
                                sessions: session_snapshots,
                                runs: r,
                                scripts: sc,
                            } = snapshot;
                            let se = session_snapshots
                                .iter()
                                .map(|session| session.serial.clone())
                                .collect::<Vec<_>>();
                            let metrics = session_snapshots
                                .into_iter()
                                .map(|session| (session.serial, session.metrics))
                                .collect::<HashMap<_, _>>();
                            for serial in &se {
                                modes.entry(serial.clone()).or_insert((
                                    Mode::Realtime,
                                    Instant::now() - Duration::from_secs(10),
                                ));
                            }
                            modes.retain(|serial, _| se.contains(serial));
                            latest_frames
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .retain(|serial, _| se.contains(serial));
                            for (serial, metric) in &metrics {
                                preview_profiles
                                    .insert(serial.clone(), metric.preview_profile.clone());
                            }
                            preview_profiles.retain(|serial, _| se.contains(serial));
                            frame_etags.retain(|serial, _| se.contains(serial));
                            let _ = events.send(Event::Devices(d));
                            let _ = events.send(Event::Scripts(sc));
                            let _ = events.send(Event::Runs(r));
                            let _ = events.send(Event::Sessions(se));
                            let _ = events.send(Event::Metrics(metrics));
                            let _ = events.send(Event::Status("后端运行中".into()));
                        }
                        Ok(None) => {}
                        _ => {
                            let _ = events.send(Event::Status("后端不可用".into()));
                        }
                    }
                }
                Ok(Command::Connect(endpoint)) => {
                    let result = post(
                        &client,
                        &format!("{api}/devices/connect"),
                        serde_json::json!({"endpoint":endpoint}),
                    );
                    let _ = events.send(Event::Status(match result {
                        Ok(()) => format!("已连接 {endpoint}"),
                        Err(e) => format!("连接失败：{e}"),
                    }));
                }
                Ok(Command::DiscoverPairingServices) => {
                    let result = get::<Vec<PairingService>>(
                        &client,
                        &format!("{api}/devices/pairing-services"),
                    )
                    .map_err(|error| error.to_string());
                    let _ = events.send(Event::PairingServices(result));
                }
                Ok(Command::Pair(endpoint, code)) => {
                    let result = client
                        .post(format!("{api}/devices/pair"))
                        .json(&serde_json::json!({"endpoint":endpoint,"code":code}))
                        .send()
                        .and_then(|response| response.error_for_status())
                        .and_then(|response| response.json::<Vec<Device>>());
                    match result {
                        Ok(devices) => {
                            let _ = events.send(Event::Devices(devices));
                            let _ = events.send(Event::PairingFinished(Ok(())));
                        }
                        Err(error) => {
                            let _ = events.send(Event::PairingFinished(Err(error.to_string())));
                        }
                    }
                }
                Ok(Command::SetScriptProfile(serial, profile)) => {
                    let result = post(
                        &client,
                        &format!("{api}/sessions/{}/script-profile", url(&serial)),
                        serde_json::json!({"profile":profile}),
                    );
                    if let Err(e) = result {
                        let _ = events.send(Event::Status(format!("脚本性能设置失败：{e}")));
                    }
                }
                Ok(Command::SetPreviewProfile(serial, profile)) => {
                    let result = post(
                        &client,
                        &format!("{api}/sessions/{}/preview-profile", url(&serial)),
                        serde_json::json!({"profile":profile}),
                    );
                    if result.is_ok() {
                        preview_profiles.insert(serial, profile);
                    } else if let Err(e) = result {
                        let _ = events.send(Event::Status(format!("预览性能设置失败：{e}")));
                    }
                }
                Ok(Command::StartSession(serial)) => {
                    let result = post(
                        &client,
                        &format!("{api}/sessions/{}/start", url(&serial)),
                        serde_json::json!({}),
                    );
                    let started = result.is_ok();
                    let status = match result {
                        Ok(()) => format!("{serial} 会话已启动"),
                        Err(error) => format!("启动失败：{error}"),
                    };
                    let _ = events.send(Event::Status(status));
                    if started {
                        modes.insert(
                            serial,
                            (Mode::Realtime, Instant::now() - Duration::from_secs(10)),
                        );
                    }
                }
                Ok(Command::StopSession(serial)) => {
                    let _ = client
                        .post(format!("{api}/sessions/{}/stop", url(&serial)))
                        .send();
                    modes.remove(&serial);
                    if let Some(token) = streams.remove(&serial) {
                        token.store(false, Ordering::Relaxed);
                    }
                    latest_frames
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&serial);
                    frame_etags.remove(&serial);
                }
                Ok(Command::StartAll) => {
                    let _ = client.post(format!("{api}/sessions/start-all")).send();
                }
                Ok(Command::StopAllSessions) => {
                    let _ = client.post(format!("{api}/sessions/stop-all")).send();
                    modes.clear();
                    for (_, token) in streams.drain() {
                        token.store(false, Ordering::Relaxed);
                    }
                    latest_frames
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clear();
                    frame_etags.clear();
                }
                Ok(Command::StopAllScripts) => {
                    let _ = client.post(format!("{api}/scripts/stop-all")).send();
                }
                Ok(Command::RunScript(serial, name)) => {
                    let result = post(
                        &client,
                        &format!("{api}/scripts/run-named"),
                        serde_json::json!({"serial":serial,"name":name}),
                    );
                    let status = match result {
                        Ok(()) => format!("{serial} 脚本已启动"),
                        Err(error) => format!("脚本启动失败：{error}"),
                    };
                    let _ = events.send(Event::Status(status));
                }
                Ok(Command::StopScript(serial)) => {
                    let _ = client
                        .post(format!("{api}/scripts/devices/{}/stop", url(&serial)))
                        .send();
                }
                Ok(Command::SaveRegion(serial, path, x1, y1, x2, y2)) => {
                    match client
                        .post(format!("{api}/sessions/{}/regions", url(&serial)))
                        .json(&serde_json::json!({"path":path,"x1":x1,"y1":y1,"x2":x2,"y2":y2}))
                        .send()
                        .and_then(|r| r.error_for_status())
                        .and_then(|r| r.json::<serde_json::Value>())
                    {
                        Ok(v) => {
                            let saved = v
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("模板")
                                .to_owned();
                            let _ = events.send(Event::Saved(serial, saved));
                        }
                        Err(e) => {
                            let _ = events.send(Event::Status(format!("模板保存失败：{e}")));
                        }
                    }
                }
                Ok(Command::SetMode(serial, mode)) => {
                    let _ = post(
                        &client,
                        &format!("{api}/sessions/{}/preview-mode", url(&serial)),
                        serde_json::json!({"mode":mode.api()}),
                    );
                    if mode != Mode::Realtime {
                        if let Some(token) = streams.remove(&serial) {
                            token.store(false, Ordering::Relaxed);
                        }
                        latest_frames
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&serial);
                        frame_etags.remove(&serial);
                    }
                    modes.insert(serial, (mode, Instant::now() - Duration::from_secs(10)));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !viewport_active(&ctx) {
                for token in streams.values() {
                    token.store(false, Ordering::Relaxed);
                }
                streams.clear();
                latest_frames
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                continue;
            }
            streams.retain(|serial, token| {
                modes
                    .get(serial)
                    .is_some_and(|(mode, _)| *mode == Mode::Realtime)
                    && token.load(Ordering::Relaxed)
            });
            for (serial, (mode, last)) in &mut modes {
                if *mode == Mode::Realtime
                    && !streams.contains_key(serial)
                    && last.elapsed() >= Duration::from_secs(1)
                {
                    let token = Arc::new(AtomicBool::new(true));
                    spawn_preview_ws(
                        api.clone(),
                        serial.clone(),
                        latest_frames.clone(),
                        token.clone(),
                        ctx.clone(),
                    );
                    streams.insert(serial.clone(), token);
                    *last = Instant::now();
                }
            }
            for (serial, (mode, last)) in &mut modes {
                let due =
                    matches!(mode, Mode::FiveSeconds) && last.elapsed() >= Duration::from_secs(5);
                if due {
                    let mut request =
                        client.get(format!("{api}/sessions/{}/frame.jpg", url(serial)));
                    if let Some(etag) = frame_etags.get(serial) {
                        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
                    }
                    if let Ok(response) = request.send() {
                        if response.status().as_u16() != 304 {
                            let etag = response
                                .headers()
                                .get(reqwest::header::ETAG)
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned);
                            if let Ok(response) = response.error_for_status() {
                                if let Ok(bytes) = response.bytes() {
                                    if store_latest_frame(
                                        &latest_frames,
                                        serial.clone(),
                                        bytes.to_vec(),
                                    ) && viewport_active(&ctx)
                                    {
                                        ctx.request_repaint();
                                    }
                                    if let Some(etag) = etag {
                                        frame_etags.insert(serial.clone(), etag);
                                    }
                                }
                            }
                        }
                    }
                    *last = Instant::now();
                }
            }
        }
    });
}
fn spawn_preview_ws(
    api: String,
    serial: String,
    latest_frames: LatestFrames,
    running: Arc<AtomicBool>,
    ctx: Context,
) {
    thread::spawn(move || {
        let base = if let Some(rest) = api.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = api.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            api
        };
        let endpoint = format!("{base}/sessions/{}/preview", url(&serial));
        if let Ok((mut socket, _)) = tungstenite::connect(endpoint.as_str()) {
            if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
            }
            while running.load(Ordering::Relaxed) {
                match socket.read() {
                    Ok(message) if message.is_binary() => {
                        if store_latest_frame(
                            &latest_frames,
                            serial.clone(),
                            message.into_data().to_vec(),
                        ) && viewport_active(&ctx)
                        {
                            ctx.request_repaint();
                        }
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(_) => break,
                }
            }
            let _ = socket.close(None);
        }
        running.store(false, Ordering::Relaxed);
    });
}
fn get<T: serde::de::DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<T, reqwest::Error> {
    client.get(url).send()?.error_for_status()?.json()
}
fn get_state<T: serde::de::DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
    etag: &mut Option<String>,
) -> Result<Option<T>, reqwest::Error> {
    let mut request = client.get(url);
    if let Some(value) = etag.as_deref() {
        request = request.header(reqwest::header::IF_NONE_MATCH, value);
    }
    let response = request.send()?;
    if response.status().as_u16() == 304 {
        return Ok(None);
    }
    let value = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let response = response.error_for_status()?;
    let result = response.json()?;
    if value.is_some() {
        *etag = value;
    }
    Ok(Some(result))
}
fn post(
    client: &reqwest::blocking::Client,
    url: &str,
    body: serde_json::Value,
) -> Result<(), reqwest::Error> {
    client.post(url).json(&body).send()?.error_for_status()?;
    Ok(())
}
fn url(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}
fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ScrcpyForge")
            .with_inner_size([520.0, 780.0])
            .with_min_inner_size([340.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ScrcpyForge",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::{endpoint_looks_valid, store_decoded_frame, DecodedFrame, LatestFrames};

    #[test]
    fn validates_pairing_endpoints() {
        assert!(endpoint_looks_valid("192.168.1.2:37123"));
        assert!(endpoint_looks_valid("[fe80::1]:37123"));
        assert!(!endpoint_looks_valid("192.168.1.2"));
        assert!(!endpoint_looks_valid("192.168.1.2:0"));
    }

    #[test]
    fn keeps_only_latest_frame_per_device() {
        let frames = LatestFrames::default();
        let frame = |value| DecodedFrame {
            size: [1, 1],
            rgba: vec![value, 0, 0, 255],
        };
        store_decoded_frame(&frames, "device-1".into(), frame(1));
        store_decoded_frame(&frames, "device-1".into(), frame(2));
        store_decoded_frame(&frames, "device-2".into(), frame(3));

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames["device-1"].rgba, vec![2, 0, 0, 255]);
        assert_eq!(frames["device-2"].rgba, vec![3, 0, 0, 255]);
    }
}
