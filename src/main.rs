#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod backend;
mod engine;
mod file_ops;
mod http_download;
mod media;
mod open_request;
mod player;
mod player_setup;
mod settings;
mod subscriptions;
mod trackers;
mod tray;

use eframe::egui::{self, Color32, RichText};
use serde_json::{Value, json};
use std::{sync::mpsc, thread, time::Duration};

enum Command {
    RemoveMany(Vec<String>, String),
    Retry,
    Shutdown,
    Select(String),
    Play(String, usize, String),
    Post(&'static str, Value),
}
enum Event {
    BatchProgress(usize, usize),
    Starting(String),
    StartupFailed(String),
    Duplicate,
    Show,
    State(Value),
    RefreshFailed(bool, String),
    Error(String),
    Done,
    Play(String, String, String),
    PlayError(String),
    Open(String),
}

struct DownloadApp {
    clipboard_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    clipboard_worker: Option<thread::JoinHandle<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    starting: bool,
    player: player::Player,
    media_choice: Option<(String, Vec<Value>)>,
    pending_open: std::collections::VecDeque<String>,
    tray: Option<tray::Tray>,
    exit_requested: bool,
    add_paused: bool,
    remove_mode: String,
    remove_targets: Vec<String>,
    selection: std::collections::BTreeSet<String>,
    selection_anchor: Option<egui::Pos2>,
    selection_base: std::collections::BTreeSet<String>,
    file_edit: Option<(String, std::collections::BTreeSet<usize>)>,
    history: std::collections::BTreeMap<String, std::collections::VecDeque<[f64; 3]>>,
    sample_clock: std::time::Instant,
    last_sample: f64,
    curve_global: bool,
    pending_action: bool,
    settings_edit: Option<settings::Settings>,
    subscription_edit: Option<subscriptions::Config>,
    tx: mpsc::Sender<Command>,
    rx: mpsc::Receiver<Event>,
    state: Value,
    selected: String,
    filter: usize,
    tab: usize,
    search: String,
    source: String,
    save_path: String,
    trackers: String,
    add_open: bool,
    tracker_open: bool,
    remove_open: bool,
    dark: bool,
    connected: bool,
    message: String,
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_owned()
}
fn num(v: &Value, key: &str) -> f64 {
    v[key].as_f64().unwrap_or(0.0)
}
fn bytes(mut n: f64) -> String {
    n = n.abs();
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut i = 0;
    while n >= 1024.0 && i < 4 {
        n /= 1024.0;
        i += 1;
    }
    format!("{n:.1} {}", units[i])
}
fn list(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}
fn task_progress(ui: &mut egui::Ui, task: &Value) {
    let progress = num(task, "progress").clamp(0.0, 1.0) as f32;
    let checking = text(task, "state") == "checking";
    let (rect, response) = ui.allocate_exact_size(egui::vec2(150.0, 28.0), egui::Sense::hover());
    let track = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - 3.0),
        egui::vec2(94.0, 6.0),
    );
    let background = if ui.visuals().dark_mode {
        Color32::from_rgb(58, 66, 76)
    } else {
        Color32::from_rgb(222, 228, 234)
    };
    let color = if !text(task, "error").is_empty() {
        Color32::from_rgb(208, 89, 89)
    } else if task["paused"] == true && progress < 1.0 {
        Color32::from_rgb(138, 149, 163)
    } else {
        Color32::from_rgb(24, 157, 140)
    };
    ui.painter().rect_filled(track, 3.0, background);
    if progress > 0.0 {
        let fill = egui::Rect::from_min_size(
            track.min,
            egui::vec2(track.width() * progress, track.height()),
        );
        ui.painter().rect_filled(fill, 3.0, color);
    }
    ui.painter().text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        if checking {
            "校验中".into()
        } else if text(task, "state") == "loading" {
            "恢复中".into()
        } else {
            format!("{:.1}%", progress * 100.0)
        },
        egui::FontId::proportional(12.0),
        ui.visuals().text_color(),
    );
    response.on_hover_text(text(task, "diagnosis"));
}
fn status(t: &Value) -> &str {
    if !text(t, "error").is_empty() {
        "错误"
    } else if t["paused"].as_bool().unwrap_or(false) {
        if num(t, "progress") >= 1.0 {
            "已完成"
        } else {
            "已暂停"
        }
    } else if num(t, "progress") >= 1.0 {
        if text(t, "kind") == "http" {
            "已完成"
        } else {
            "做种中"
        }
    } else if text(t, "state").contains("checking") {
        "校验中"
    } else if num(t, "download_rate") > 0.0 {
        "下载中"
    } else if text(t, "state") == "loading" {
        "载入中"
    } else if num(t, "peers") > 0.0 {
        "等待数据"
    } else {
        "等待来源"
    }
}

impl DownloadApp {
    fn new(cc: &eframe::CreationContext<'_>, root: std::path::PathBuf) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        for path in ["C:/Windows/Fonts/msyh.ttc", "C:/Windows/Fonts/simhei.ttf"] {
            if let Ok(data) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert("chinese".into(), egui::FontData::from_owned(data).into());
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "chinese".into());
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .push("chinese".into());
                break;
            }
        }
        cc.egui_ctx.set_fonts(fonts);
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        cc.egui_ctx.set_style(style);
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        let request_root = root.clone();
        use raw_window_handle::HasWindowHandle;
        let native = cc
            .window_handle()
            .ok()
            .and_then(|h| match h.as_raw() {
                raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
                _ => None,
            })
            .unwrap_or(0);
        let clipboard_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clipboard_enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            std::fs::read(root.join("data/settings.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<settings::Settings>(&b).ok())
                .is_none_or(|s| s.clipboard_watch),
        ));
        let clipboard_worker = {
            let stop = clipboard_stop.clone();
            let enabled = clipboard_enabled.clone();
            let events = events.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let mut clipboard = None;
                // Ignore clipboard contents from before Flow was launched.
                // A fresh copy advances the Windows sequence even for identical text.
                let mut sequence = open_request::clipboard_sequence();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let current = open_request::clipboard_sequence();
                    if current != sequence {
                        if !enabled.load(std::sync::atomic::Ordering::Relaxed) {
                            sequence = current;
                        } else {
                            if clipboard.is_none() {
                                clipboard = arboard::Clipboard::new().ok();
                            }
                            if let Some(value) = clipboard
                                .as_mut()
                                .and_then(|c: &mut arboard::Clipboard| c.get_text().ok())
                            {
                                sequence = current;
                                if let Some(source) = open_request::copied_magnet(&value) {
                                    if events.send(Event::Open(source)).is_err() {
                                        break;
                                    }
                                    tray::wake_window(native);
                                    ctx.request_repaint();
                                }
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            })
        };
        let worker = thread::spawn(move || {
            'startup: loop {
                let _ = events.send(Event::Starting("正在启动下载引擎…".into()));
                ctx.request_repaint();
                let mut backend = match backend::Backend::launch_in(request_root.clone(), false) {
                    Ok(backend) => backend,
                    Err(error) => {
                        let _ = events.send(Event::StartupFailed(error));
                        ctx.request_repaint();
                        return;
                    }
                };
                let mut last_stage = String::new();
                loop {
                    if let Some(result) = backend.poll_ready() {
                        match result {
                            Ok(()) => break,
                            Err(error) => {
                                if error.contains("此项目已有下载引擎运行") {
                                    let _ = std::fs::write(
                                        request_root.join("data/show-window.request"),
                                        b"show",
                                    );
                                    let _ = events.send(Event::Duplicate);
                                    ctx.request_repaint();
                                    return;
                                }
                                let _ = events.send(Event::StartupFailed(error));
                                ctx.request_repaint();
                                drop(backend);
                                loop {
                                    match commands.recv() {
                                        Ok(Command::Retry) => continue 'startup,
                                        Ok(Command::Shutdown) | Err(_) => return,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    let stage =
                        std::fs::read_to_string(request_root.join("data/startup-stage.txt"))
                            .unwrap_or_else(|_| "正在初始化引擎…".into());
                    if stage != last_stage {
                        last_stage = stage.clone();
                        let _ = events.send(Event::Starting(stage));
                        ctx.request_repaint();
                    }
                    match commands.recv_timeout(Duration::from_millis(100)) {
                        Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Ok(Command::Retry) => {
                            drop(backend);
                            continue 'startup;
                        }
                        _ => {}
                    }
                }
                let base = backend.base.clone();
                let token = backend.token.clone();

                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(4))
                    .no_proxy()
                    .build()
                    .unwrap();
                let mut selected = String::new();
                loop {
                    let show = request_root.join("data/show-window.request");
                    if show.exists() {
                        let _ = std::fs::remove_file(show);
                        tray::wake_window(native);
                        let _ = events.send(Event::Show);
                        ctx.request_repaint();
                    }
                    match open_request::take(&request_root) {
                        Ok(Some(source)) => {
                            if events.send(Event::Open(source)).is_err() {
                                break;
                            }
                            tray::wake_window(native);
                            ctx.request_repaint();
                        }
                        Err(error) => {
                            let _ = events.send(Event::Error(error.to_string()));
                        }
                        _ => {}
                    }
                    match commands.recv_timeout(Duration::from_millis(800)) {
                        Ok(Command::Shutdown) => return,
                        Ok(Command::Retry) => {
                            drop(backend);
                            continue 'startup;
                        }
                        Ok(Command::Select(id)) => selected = id,
                        Ok(Command::Play(id, index, title)) => {
                            let result = client
                                .post(format!("{base}/api/play"))
                                .bearer_auth(&token)
                                .json(&json!({"id":id,"index":index}))
                                .send()
                                .and_then(|r| r.json::<Value>());
                            match result {
                                Ok(v) if v["path"].is_string() => {
                                    let _ = events.send(Event::Play(
                                        format!("{}{}", base, text(&v, "path")),
                                        title,
                                        token.clone(),
                                    ));
                                }
                                Ok(v) => {
                                    let _ = events.send(Event::PlayError(text(&v, "error")));
                                }
                                Err(e) => {
                                    let _ = events.send(Event::PlayError(e.to_string()));
                                }
                            }
                        }

                        Ok(Command::RemoveMany(ids, mode)) => {
                            let mut failed = Vec::new();
                            let total = ids.len();
                            for (index, id) in ids.into_iter().enumerate() {
                                let _ = events.send(Event::BatchProgress(index + 1, total));
                                ctx.request_repaint();
                                let result = client
                                    .post(format!("{base}/api/action"))
                                    .bearer_auth(&token)
                                    .timeout(Duration::from_secs(60))
                                    .json(&json!({"id":id,"action":"remove","delete_mode":mode}))
                                    .send()
                                    .and_then(|r| r.json::<Value>());
                                match result {
                                    Ok(v) if v.get("error").is_none() => {}
                                    Ok(v) => failed.push(format!("{id}: {}", text(&v, "error"))),
                                    Err(e) => failed
                                        .push(format!("{id}: {e}（未自动重试，请刷新后确认结果）")),
                                }
                            }
                            let message = if failed.is_empty() {
                                format!("已移除 {total} 个任务")
                            } else {
                                format!(
                                    "{total} 个任务中有 {} 项未能确认删除：{}",
                                    failed.len(),
                                    failed.join("；")
                                )
                            };
                            let _ = events.send(Event::Error(message));
                        }
                        Ok(Command::Post(path, data)) => {
                            let result = client
                                .post(format!("{base}{path}"))
                                .bearer_auth(&token)
                                .json(&data)
                                .send()
                                .and_then(|r| r.json::<Value>());
                            match result {
                                Ok(v) if v.get("error").is_none() => {
                                    let _ = events.send(Event::Done);
                                }
                                Ok(v) => {
                                    let _ = events.send(Event::Error(text(&v, "error")));
                                }
                                Err(e) => {
                                    let _ = events.send(Event::Error(e.to_string()));
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let response = client
                        .get(format!("{base}/api/state"))
                        .query(&[("selected", &selected)])
                        .bearer_auth(&token)
                        .send()
                        .and_then(|r| r.error_for_status())
                        .and_then(|r| r.json::<Value>());
                    let event = match response {
                        Ok(state) => {
                            clipboard_enabled.store(
                                state["settings"]["clipboard_watch"]
                                    .as_bool()
                                    .unwrap_or(true),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            Event::State(state)
                        }
                        Err(e) => {
                            let alive = client
                                .get(format!("{base}/api/health"))
                                .bearer_auth(&token)
                                .timeout(Duration::from_secs(2))
                                .send()
                                .and_then(|r| r.error_for_status())
                                .is_ok();
                            Event::RefreshFailed(
                                alive,
                                if alive {
                                    format!("引擎仍在运行，状态刷新暂时失败，正在自动重试。{e}")
                                } else {
                                    format!("引擎暂时无响应，正在自动重连。{e}")
                                },
                            )
                        }
                    };
                    if events.send(event).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                return;
            }
        });
        Self {
            clipboard_stop,
            clipboard_worker: Some(clipboard_worker),
            worker: Some(worker),
            starting: true,
            player: player::Player::new(root.clone()),
            media_choice: None,
            pending_open: Default::default(),
            tray: tray::Tray::new(cc).ok(),
            exit_requested: false,
            add_paused: false,
            remove_mode: "keep".into(),
            remove_targets: Vec::new(),
            selection: Default::default(),
            selection_anchor: None,
            selection_base: Default::default(),
            file_edit: None,
            history: Default::default(),
            sample_clock: std::time::Instant::now(),
            last_sample: -1.0,
            curve_global: false,
            pending_action: false,
            settings_edit: None,
            subscription_edit: None,
            tx,
            rx,
            state: json!({}),
            selected: String::new(),
            filter: 0,
            tab: 0,
            search: String::new(),
            source: String::new(),
            save_path: root.join("downloads").display().to_string(),
            trackers: String::new(),
            add_open: false,
            tracker_open: false,
            remove_open: false,
            dark: false,
            connected: false,
            message: String::new(),
        }
    }

    fn action(&mut self, action: &str) {
        if self.pending_action {
            return;
        }
        self.pending_action = true;
        let _ = self.tx.send(Command::Post(
            "/api/action",
            json!({"id":self.selected,"action":action}),
        ));
    }

    fn play_media(&mut self, id: String, file: &Value) {
        if let Some(source) = file["source"].as_str() {
            self.player.play(source.into(), text(file, "path"), None);
        } else {
            self.player.open = false;
            self.message = "正在准备播放文件…".into();
            let _ = self.tx.send(Command::Play(
                id,
                file["index"].as_u64().unwrap_or(0) as usize,
                text(file, "path"),
            ));
        }
    }
    fn play_task(&mut self, task: &Value) {
        let mut files = list(&task["media_files"]);
        files.sort_by(|a, b| num(b, "size").total_cmp(&num(a, "size")));
        if files.len() == 1 {
            self.play_media(text(task, "id"), &files[0]);
        } else if !files.is_empty() {
            self.media_choice = Some((text(task, "id"), files));
        }
    }

    fn new_task(&mut self) {
        if let Some(path) = self.state["settings"]["download_dir"].as_str() {
            self.save_path = path.into();
        }
        self.add_open = true;
    }
    fn open_remove(&mut self) {
        if self.pending_action {
            return;
        }
        self.remove_targets = list(&self.state["tasks"])
            .iter()
            .filter(|t| {
                matches_filter(t, self.filter)
                    && text(t, "name")
                        .to_lowercase()
                        .contains(&self.search.to_lowercase())
            })
            .filter(|t| self.selection.contains(&text(t, "id")))
            .map(|t| text(t, "id"))
            .collect();
        if self.remove_targets.is_empty() {
            return;
        }
        self.remove_mode = "keep".into();
        self.remove_open = true;
    }
    fn chart(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.curve_global, "显示全部任务合计");
        ui.label("最近 5 分钟 · 绿色：下载，紫色：上传 · 仅保留本次运行数据");
        let key = if self.curve_global {
            ""
        } else {
            self.selected.as_str()
        };
        let history = self.history.get(key);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().clamp(350.0, 1000.0), 150.0),
            egui::Sense::hover(),
        );
        let plot = rect.shrink2(egui::vec2(12.0, 20.0));
        let max = history
            .map(|h| h.iter().flat_map(|p| [p[1], p[2]]).fold(1024.0, f64::max))
            .unwrap_or(1024.0);
        for i in 0..=4 {
            let y = egui::lerp(plot.bottom()..=plot.top(), i as f32 / 4.0);
            ui.painter().line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
        }
        ui.painter().text(
            rect.left_top(),
            egui::Align2::LEFT_TOP,
            format!("{}/s", bytes(max)),
            egui::FontId::proportional(11.0),
            ui.visuals().text_color(),
        );
        if let Some(h) = history {
            let end = self.sample_clock.elapsed().as_secs_f64();
            for (index, color) in [
                (1, Color32::from_rgb(24, 157, 140)),
                (2, Color32::from_rgb(157, 111, 231)),
            ] {
                let points: Vec<_> = h
                    .iter()
                    .filter(|p| end - p[0] <= 300.0)
                    .map(|p| {
                        egui::pos2(
                            plot.right() - ((end - p[0]) / 300.0) as f32 * plot.width(),
                            plot.bottom() - (p[index] / max) as f32 * plot.height(),
                        )
                    })
                    .collect();
                if points.len() > 1 {
                    ui.painter()
                        .add(egui::Shape::line(points, egui::Stroke::new(2.0_f32, color)));
                }
            }
        }
        ui.painter().text(
            rect.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            "−5 分钟",
            egui::FontId::proportional(11.0),
            ui.visuals().text_color(),
        );
        ui.painter().text(
            rect.right_bottom(),
            egui::Align2::RIGHT_BOTTOM,
            "现在",
            egui::FontId::proportional(11.0),
            ui.visuals().text_color(),
        );
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (i, label) in [
                "概览",
                "文件",
                "Tracker",
                "对等连接",
                "诊断日志",
                "速度曲线",
            ]
            .iter()
            .enumerate()
            {
                ui.selectable_value(&mut self.tab, i, *label);
            }
        });
        ui.separator();
        egui::ScrollArea::both().auto_shrink([false,false]).id_salt("details_scroll").show(ui, |ui| {
            let task = list(&self.state["tasks"]).into_iter().find(|t| text(t, "id") == self.selected);
            if self.tab == 5 { self.chart(ui); }
            else if self.tab == 4 {
                if ui.button("复制当前诊断 JSON").clicked() {
                    ui.ctx().copy_text(serde_json::to_string_pretty(&self.state).unwrap_or_default());
                    self.message = "诊断已复制到剪贴板（包含本机路径和对等连接地址）".into();
                }
                egui::Grid::new("events").striped(true).num_columns(3).show(ui, |ui| {
                    for e in list(&self.state["events"]).iter().rev().filter(|e| self.selected.is_empty() || text(e,"task").is_empty() || text(e,"task") == self.selected).take(150) {
                        ui.label(text(e, "time"));
                        ui.colored_label(if text(e,"level") == "error" {Color32::from_rgb(205,65,65)} else {ui.visuals().text_color()}, text(e,"level"));
                        ui.label(text(e, "message")); ui.end_row();
                    }
                });
            } else if let Some(t) = task {
                match self.tab {
                    0 => {
                        ui.label(RichText::new(text(&t,"name")).strong().size(17.0));
                        ui.add_space(6.0);
                        ui.colored_label(if text(&t, "error").is_empty() { Color32::from_rgb(36,140,125) } else { Color32::from_rgb(208,89,89) }, text(&t,"diagnosis"));
                        ui.add_space(8.0);
                        egui::Grid::new("overview").num_columns(4).spacing([24.0, 10.0]).show(ui, |ui| {
                            for (a,b,c,d) in [
                                ("已完成", bytes(num(&t,"done")), "总大小", bytes(num(&t,"total"))),
                                ("下载速度", format!("{}/s",bytes(num(&t,"download_rate"))), "上传速度", format!("{}/s",bytes(num(&t,"upload_rate")))),
                                ("已连接用户", format!("{:.0}",num(&t,"peers")), "已连接做种者", if t["seeds"].is_number(){format!("{:.0}",num(&t,"seeds"))}else{"未知".into()}),
                                ("资源可用率", if num(&t,"availability") < 0.0 {"未知".into()} else {format!("{:.3}",num(&t,"availability"))}, "引擎状态", text(&t,"state")),
                            ] { ui.weak(a); ui.label(b); ui.weak(c); ui.label(d); ui.end_row(); }
                        });
                        ui.add_space(12.0);
                        ui.label(format!("保存位置：{}",text(&t,"save_path")));
                        ui.weak("默认完成后停止做种；可在设置中开启完成后继续做种。未知指标表示引擎没有公开该数据；Tracker 做种统计可在 Tracker 页查看。");
                    }
                    1 => {
                        let files = list(&self.state["detail"]["files"]);
                        let bt = text(&t,"kind") != "http";
                        if bt && !files.is_empty() {
                            if self.file_edit.as_ref().is_none_or(|(id,_)|id != &self.selected) {
                                self.file_edit = Some((self.selected.clone(),files.iter().filter(|f|f["selected"]==true).filter_map(|f|f["index"].as_u64().map(|i|i as usize)).collect()));
                            }
                            ui.horizontal(|ui| {
                                if ui.button("全选").clicked() { self.file_edit.as_mut().unwrap().1 = (0..files.len()).collect(); }
                                if ui.button("清空选择").clicked() { self.file_edit.as_mut().unwrap().1.clear(); }
                                if ui.button("应用文件选择").clicked() {
                                    let selected = &self.file_edit.as_ref().unwrap().1;
                                    if selected.is_empty() { self.message="至少选择一个文件".into(); }
                                    else { let _ = self.tx.send(Command::Post("/api/files",json!({"id":self.selected,"files":selected}))); }
                                }
                            });
                            ui.weak("取消勾选不删除已有数据；相邻文件可能保留共享分块。新建时可勾选“添加后暂停”再选择文件。");
                        }
                        egui::Grid::new("files").striped(true).num_columns(4).min_col_width(60.0).show(ui, |ui| {
                            ui.strong("下载"); ui.strong("文件路径"); ui.strong("大小"); ui.strong("完成度"); ui.end_row();
                            for f in files {
                                if bt {
                                    let index = f["index"].as_u64().unwrap_or(0) as usize;
                                    let selected = &mut self.file_edit.as_mut().unwrap().1;
                                    let mut checked = selected.contains(&index);
                                    if ui.checkbox(&mut checked, "").changed() { if checked { selected.insert(index); } else { selected.remove(&index); } }
                                } else { ui.label("✓"); }
                                ui.horizontal(|ui| {
                                    ui.label(text(&f,"path"));
                                    if bt && media::playable(&text(&f,"path")) && ui.small_button("▶ 播放").on_hover_text("选择此文件并继续下载，边下边播").clicked() {
                                        self.player.open=false;
                                        self.message="正在准备播放文件…".into();
                                        let _=self.tx.send(Command::Play(self.selected.clone(),f["index"].as_u64().unwrap_or(0) as usize,text(&f,"path")));
                                    }
                                }); ui.label(bytes(num(&f,"size")));
                                ui.label(format!("{:.1}%", if num(&f,"size") == 0.0 {100.0} else {num(&f,"done")/num(&f,"size")*100.0})); ui.end_row();
                            }
                        });
                    }
                    2 => {
                        if text(&t,"kind") == "http" { ui.label("HTTP 直链任务不使用 Tracker。"); return; }
                        ui.horizontal(|ui| {
                            let running = self.state["detail"]["discovery"]["running"].as_bool().unwrap_or(false);
                            if ui.add_enabled(!running, egui::Button::new(if running {"正在发现…"} else {"发现与健康检查"})).clicked() {self.action("discover");}
                            if ui.button("添加 Tracker…").clicked() {self.tracker_open = true;}
                            if ui.button("订阅设置…").clicked() {
                                self.subscription_edit = serde_json::from_value(self.state["subscriptions"]["config"].clone()).ok();
                            }
                            if ui.button("重新查询").clicked() {self.action("announce");}
                            if ui.button("应用候选（重新校验）").clicked() {self.action("apply_trackers");}
                        });
                        ui.label(text(&self.state["detail"]["discovery"], "message"));
                        ui.weak("健康分仅衡量 Tracker 响应，不代表下载速度；报告做种数只占少量权重。实际有效节点按已传输数据缓存，无法可靠归因到单个 Tracker。");
                        egui::Grid::new("trackers").striped(true).num_columns(8).show(ui, |ui| {
                            for title in ["健康分", "Tracker 地址", "报告做种数", "响应耗时", "成功 / 失败", "来源", "状态", "响应 / 错误原因"] {ui.strong(title);} ui.end_row();
                            for t in list(&self.state["detail"]["trackers"]) {
                                ui.label(if t["score"].is_number() {format!("{:.1}",num(&t,"score"))} else {"—".into()});
                                ui.label(text(&t,"url"));
                                ui.label(if t["seeders"].is_number() {format!("{:.0}",num(&t,"seeders"))} else {"—".into()});
                                ui.label(if t["latency_ms"].is_number() {format!("{:.0} ms",num(&t,"latency_ms"))} else {"—".into()});
                                ui.label(format!("{:.0} / {:.0}",num(&t,"successes"),num(&t,"failures")));
                                ui.label(text(&t,"source")); ui.label(text(&t,"status")); ui.label(text(&t,"message")); ui.end_row();
                            }
                        });
                    }
                    3 => {
                        egui::Grid::new("peers").striped(true).num_columns(4).min_col_width(120.0).show(ui, |ui| {
                            for h in ["地址", "客户端", "累计接收", "连接状态"] {ui.strong(h);} ui.end_row();
                            for p in list(&self.state["detail"]["peers"]) {
                                ui.label(text(&p,"address")); ui.label(text(&p,"client"));
                                ui.label(bytes(num(&p,"downloaded")));
                                ui.label(text(&p,"state")); ui.end_row();
                            }
                        });
                    }
                    _ => {}
                }
            } else {ui.weak("选择一个任务，查看文件、Tracker 和连接详情。");}
        });
    }
}

impl eframe::App for DownloadApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.player.ui(ctx);
        if let Some(tray) = &self.tray {
            while let Ok(action) = tray.events.try_recv() {
                match action {
                    tray::Action::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    tray::Action::Exit => {
                        self.exit_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
        if ctx.input(|i| i.viewport().close_requested())
            && !self.exit_requested
            && self.connected
            && self.tray.is_some()
            && self.state["settings"]["background_on_close"]
                .as_bool()
                .unwrap_or(true)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Starting(message) => {
                    self.starting = true;
                    self.connected = false;
                    self.message = message;
                }
                Event::StartupFailed(message) => {
                    self.starting = false;
                    self.connected = false;
                    self.message = message;
                }
                Event::Duplicate => {
                    self.exit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Event::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Event::Open(source) => {
                    if !(self.add_open && self.source == source)
                        && !self.pending_open.contains(&source)
                    {
                        self.pending_open.push_back(source);
                    }
                }
                Event::BatchProgress(index, total) => {
                    self.message = format!("正在移除任务 {index}/{total}…");
                }
                Event::PlayError(error) => {
                    self.player.message = error;
                    self.player.open = true;
                }
                Event::Play(source, title, token) => {
                    self.message.clear();
                    self.player.play(source, title, Some(&token));
                }
                Event::State(v) => {
                    if self.starting
                        || self.message.starts_with("引擎仍在运行")
                        || self.message.starts_with("引擎暂时无响应")
                    {
                        self.message.clear();
                    }
                    self.starting = false;
                    let elapsed = self.sample_clock.elapsed().as_secs_f64();
                    if elapsed - self.last_sample >= 1.0 {
                        self.last_sample = elapsed;
                        let mut aggregate = [elapsed, 0.0, 0.0];
                        let tasks = list(&v["tasks"]);
                        self.history.retain(|id, _| {
                            id.is_empty() || tasks.iter().any(|t| text(t, "id") == *id)
                        });
                        for t in tasks {
                            let point = [elapsed, num(&t, "download_rate"), num(&t, "upload_rate")];
                            aggregate[1] += point[1];
                            aggregate[2] += point[2];
                            let h = self.history.entry(text(&t, "id")).or_default();
                            h.push_back(point);
                            while h.len() > 301 {
                                h.pop_front();
                            }
                        }
                        let h = self.history.entry(String::new()).or_default();
                        h.push_back(aggregate);
                        while h.len() > 301 {
                            h.pop_front();
                        }
                    }
                    self.state = v;
                    self.connected = true;
                }
                Event::Error(e) => {
                    self.pending_action = false;
                    if e.starts_with("内置引擎连接失败") {
                        self.connected = false;
                    }
                    self.message = e;
                }
                Event::RefreshFailed(alive, message) => {
                    self.connected = alive;
                    self.message = message;
                }
                Event::Done => {
                    self.pending_action = false;
                    self.message = "操作成功".into();
                }
            }
        }
        if !self.add_open {
            if let Some(source) = self.pending_open.pop_front() {
                self.new_task();
                self.source = source;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        let tasks = list(&self.state["tasks"]);
        self.selection.retain(|id| {
            tasks.iter().any(|t| {
                text(t, "id") == *id
                    && matches_filter(t, self.filter)
                    && text(t, "name")
                        .to_lowercase()
                        .contains(&self.search.to_lowercase())
            })
        });
        if self.selected.is_empty() && !tasks.is_empty() {
            self.selected = text(&tasks[0], "id");
            self.selection.insert(self.selected.clone());
            let _ = self.tx.send(Command::Select(self.selected.clone()));
        }
        let has_selection = tasks.iter().any(|t| text(t, "id") == self.selected);
        let selected_task = tasks.iter().find(|t| text(t, "id") == self.selected);
        let can_resume = !self.pending_action
            && selected_task.is_some_and(|t| t["paused"] == true || !text(t, "error").is_empty());
        let can_pause = !self.pending_action
            && selected_task.is_some_and(|t| t["paused"] != true && text(t, "error").is_empty());
        if has_selection
            && !ctx.wants_keyboard_input()
            && !self.add_open
            && !self.remove_open
            && self.settings_edit.is_none()
            && self.subscription_edit.is_none()
            && !self.tracker_open
        {
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) {
                self.open_remove();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                if can_resume {
                    self.action("resume");
                } else if can_pause {
                    self.action("pause");
                }
            }
        }
        if !has_selection && !self.selected.is_empty() {
            self.selected.clear();
            let _ = self.tx.send(Command::Select(String::new()));
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("FLOW")
                        .strong()
                        .size(23.0)
                        .color(Color32::from_rgb(24, 147, 127)),
                );
                ui.weak("下载工作台");
                ui.separator();
                if ui
                    .add_enabled(self.connected, egui::Button::new("＋ 新建任务"))
                    .clicked()
                {
                    self.new_task();
                }
                if ui.button("播放器…").clicked() {
                    self.player.open = true;
                }
                if ui
                    .button("退出程序")
                    .on_hover_text("停止下载并完全退出，包括后台引擎")
                    .clicked()
                {
                    self.exit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                ui.add_enabled_ui(self.connected && has_selection, |ui| {
                    if ui
                        .add_enabled(can_resume, egui::Button::new("▶ 继续"))
                        .clicked()
                    {
                        self.action("resume");
                    }
                    if ui
                        .add_enabled(can_pause, egui::Button::new("Ⅱ 暂停"))
                        .clicked()
                    {
                        self.action("pause");
                    }
                    if ui
                        .add_enabled(
                            selected_task.is_some_and(|t| text(t, "kind") != "http"),
                            egui::Button::new("校验文件"),
                        )
                        .clicked()
                    {
                        self.action("recheck");
                    }
                    if ui.button("移除任务").clicked() {
                        self.open_remove();
                    }
                });
                if ui
                    .add_enabled(self.connected, egui::Button::new("设置…"))
                    .clicked()
                {
                    self.settings_edit =
                        serde_json::from_value(self.state["settings"].clone()).ok();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.checkbox(&mut self.dark, "深色").changed() {
                        ctx.set_visuals(if self.dark {
                            egui::Visuals::dark()
                        } else {
                            egui::Visuals::light()
                        });
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("搜索任务…")
                            .desired_width(160.0),
                    );
                });
            });
            ui.add_space(5.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(
                    if self.connected {
                        Color32::from_rgb(24, 147, 127)
                    } else {
                        Color32::from_rgb(210, 90, 60)
                    },
                    if self.connected {
                        "● 引擎已连接"
                    } else {
                        if self.starting {
                            "● 引擎启动中"
                        } else {
                            "● 引擎未连接"
                        }
                    },
                );
                if !self.connected && !self.starting && ui.button("重试启动").clicked() {
                    self.starting = true;
                    let _ = self.tx.send(Command::Retry);
                }
                ui.separator();
                ui.label(text(&self.state, "engine"));
                ui.separator();
                ui.label(format!(
                    "↓ {}/s",
                    bytes(tasks.iter().map(|t| num(t, "download_rate")).sum())
                ));
                ui.label(format!(
                    "↑ {}/s",
                    bytes(tasks.iter().map(|t| num(t, "upload_rate")).sum())
                ));
                ui.separator();
                ui.weak(format!("监听端口 {}", num(&self.state, "listen_port")));
            });
            if !self.message.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(&self.message);
                    if ui.small_button("清除").clicked() {
                        self.message.clear();
                    }
                });
            }
        });
        egui::SidePanel::left("sidebar")
            .exact_width(155.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.weak("任务分类");
                ui.add_space(8.0);
                for (i, label) in ["全部任务", "下载中", "已暂停", "已完成", "错误"]
                    .iter()
                    .enumerate()
                {
                    let count = tasks.iter().filter(|t| matches_filter(t, i)).count();
                    ui.selectable_value(&mut self.filter, i, format!("{label}    {count}"));
                    ui.add_space(4.0);
                }
                ui.separator();
                ui.weak("本机下载引擎");
                ui.label("BT / HTTP(S)");
                ui.add_space(12.0);
                ui.weak(
                    "自动保存续传状态\nCtrl + 左键：多选任务\n右键 / Delete：批量删除\n空格：暂停 / 继续",
                );
            });
        egui::TopBottomPanel::bottom("detail_panel")
            .resizable(true)
            .default_height(290.0)
            .min_height(240.0)
            .show(ctx, |ui| self.details(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(["全部任务", "下载中", "已暂停", "已完成", "错误"][self.filter]);
                if !self.selection.is_empty() { ui.label(format!("已选 {} 项",self.selection.len())); }
                ui.weak(format!("{} 个任务", tasks.iter().filter(|t| matches_filter(t, self.filter) && text(t, "name").to_lowercase().contains(&self.search.to_lowercase())).count()));
            });
            ui.add_space(6.0);
            if tasks.is_empty() {
                ui.add_space(55.0);
                ui.vertical_centered(|ui| {
                    if !self.connected {
                        if self.starting {
                            ui.spinner();
                            ui.heading("正在准备下载引擎");
                        } else {
                            ui.heading("下载引擎未能连接");
                        }
                        ui.add_space(8.0);
                        ui.label(&self.message);
                        ui.weak("任务列表将在引擎连接后加载；当前空白不代表任务已丢失。");
                        if ui.button(if self.starting { "取消并重新启动" } else { "重试启动" }).clicked() {
                            self.starting = true;
                            let _ = self.tx.send(Command::Retry);
                        }
                        ui.weak("启动诊断保存在 data/startup.log。");
                        return;
                    }
                    ui.heading("从一个下载任务开始");
                    ui.add_space(8.0);
                    ui.weak("添加磁力链接或本机种子文件，实时查看下载进度与连接诊断。");
                    ui.add_space(15.0);
                    if ui
                        .add_enabled(
                            self.connected,
                            egui::Button::new("＋ 新建下载任务").min_size(egui::vec2(160.0, 36.0)),
                        )
                        .clicked()
                    {
                        self.add_open = true;
                    }
                    ui.add_space(12.0);
                    ui.weak("续传已有文件时，将保存目录设为原下载的根目录。");
                });
            } else {
                let mut row_rects = Vec::new();
                let table_hovered = ui.ui_contains_pointer();
                let table = egui::ScrollArea::both()
                    .id_salt("tasks_scroll")
                    .auto_shrink([false,false])
                    .scroll_source(egui::scroll_area::ScrollSource { drag: false, ..Default::default() })
                    .show(ui, |ui| {
                        egui::Grid::new("task_table")
                            .striped(true)
                            .num_columns(9)
                            .spacing([16.0, 5.0])
                            .show(ui, |ui| {
                                for title in [
                                    "名称",
                                    "大小",
                                    "进度",
                                    "状态",
                                    "下载速度",
                                    "上传速度",
                                    "连接",
                                    "做种",
                                    "可用率",
                                ] {
                                    ui.strong(title);
                                }
                                ui.end_row();
                                let filter = self.filter;
                                let search = self.search.to_lowercase();
                                for t in tasks.iter().filter(|t| {
                                    matches_filter(t, filter)
                                        && text(t, "name").to_lowercase().contains(&search)
                                }) {
                                    let id = text(t, "id");
                                    let response = ui
                                        .add_sized(
                                            [310.0, 28.0],
                                            egui::Button::new(text(t, "name"))
                                                .selected(self.selection.contains(&id))
                                                .frame(false)
                                                .truncate(),
                                        )
                                        .on_hover_text(text(t, "name"));
                                    if response.clicked() || response.secondary_clicked() {
                                        if response.clicked() && ui.input(|i| i.modifiers.ctrl || i.modifiers.command) {
                                            if !self.selection.remove(&id) { self.selection.insert(id.clone()); }
                                        } else if !response.secondary_clicked() || !self.selection.contains(&id) {
                                            self.selection.clear();
                                            self.selection.insert(id.clone());
                                        }
                                        self.selected = id.clone();
                                        self.state["detail"] = json!({});
                                        let _ = self.tx.send(Command::Select(id.clone()));
                                    }
                                    let resumable =
                                        t["paused"] == true || !text(t, "error").is_empty();
                                    let pausable =
                                        t["paused"] != true && text(t, "error").is_empty();
                                    if response.double_clicked() {
                                        if resumable {
                                            self.action("resume");
                                        } else if pausable {
                                            self.action("pause");
                                        }
                                    }
                                    response.context_menu(|ui| {
                                        if self.selection.len() > 1 {
                                            ui.label(format!("已选择 {} 个任务",self.selection.len()));
                                            if ui.button("批量删除所选任务…").clicked() {self.open_remove();ui.close();}
                                            return;
                                        }
                                        let media_files=list(&t["media_files"]);
                                        let label=if media_files.len()>1 {"▶ 选择视频 / 音频播放…"} else if text(t,"kind")=="http" && num(t,"progress")<1.0 {"▶ 播放媒体直链"} else if num(t,"progress")<1.0 {"▶ 边下边播"} else {"▶ 播放"};
                                        if ui.add_enabled(!media_files.is_empty(),egui::Button::new(label))
                                            .on_hover_text(if media_files.is_empty() {"尚未识别到媒体文件，请等待元数据加载"} else {"自动识别 MP4、MKV 等音视频；BT 播放会继续下载，HTTP 未完成时直接播放原始媒体链接"})
                                            .clicked() {self.play_task(t); ui.close();}
                                        ui.separator();
                                        if ui
                                            .add_enabled(
                                                resumable || pausable,
                                                egui::Button::new(if resumable {
                                                    "继续下载"
                                                } else {
                                                    "暂停任务"
                                                }),
                                            )
                                            .clicked()
                                        {
                                            self.action(if resumable { "resume" } else { "pause" });
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui
                                            .add_enabled(
                                                !text(t, "magnet").is_empty(),
                                                egui::Button::new("复制磁力链接"),
                                            )
                                            .clicked()
                                        {
                                            ui.ctx().copy_text(text(t, "magnet"));
                                            ui.close();
                                        }
                                        if ui.button("复制原始链接 / 种子路径").clicked()
                                        {
                                            ui.ctx().copy_text(text(t, "source"));
                                            ui.close();
                                        }
                                        if ui.button("复制名称").clicked() {
                                            ui.ctx().copy_text(text(t, "name"));
                                            ui.close();
                                        }
                                        if ui.button("复制保存路径").clicked() {
                                            ui.ctx().copy_text(text(t, "save_path"));
                                            ui.close();
                                        }
                                        if ui.button("打开保存目录").clicked() {
                                            match std::process::Command::new("explorer.exe")
                                                .arg(text(t, "save_path"))
                                                .spawn()
                                            {
                                                Ok(_) => {}
                                                Err(e) => {
                                                    self.message = format!("无法打开目录：{e}")
                                                }
                                            }
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui
                                            .add_enabled(
                                                text(t, "kind") != "http",
                                                egui::Button::new("校验文件"),
                                            )
                                            .clicked()
                                        {
                                            self.action("recheck");
                                            ui.close();
                                        }
                                        if ui.button("删除任务…").clicked() {
                                            self.open_remove();
                                            ui.close();
                                        }
                                    });
                                    ui.label(bytes(num(t, "total")));
                                    task_progress(ui, t);
                                    ui.label(status(t));
                                    ui.label(format!("{}/s", bytes(num(t, "download_rate"))));
                                    ui.label(format!("{}/s", bytes(num(t, "upload_rate"))));
                                    ui.label(format!("{:.0}", num(t, "peers")));
                                    ui.label(if t["seeds"].is_number() {
                                        format!("{:.0}", num(t, "seeds"))
                                    } else {
                                        "—".into()
                                    });
                                    let last_cell = ui.label(if num(t, "availability") < 0.0 {
                                        "—".into()
                                    } else {
                                        format!("{:.2}", num(t, "availability"))
                                    });
                                    row_rects.push((id, response.rect.union(last_cell.rect)));
                                    ui.end_row();
                                }
                            });
                    });
                let (position, pressed, down, released, additive) = ctx.input(|i| (
                    i.pointer.interact_pos(), i.pointer.primary_pressed(), i.pointer.primary_down(),
                    i.pointer.primary_released(), i.modifiers.ctrl || i.modifiers.command
                ));
                if let Some(position) = position {
                    if pressed && table_hovered && table.inner_rect.contains(position)
                        && row_rects.first().is_some_and(|(_,r)| position.y >= r.top())
                        && !self.remove_open && !self.add_open {
                        self.selection_anchor = Some(position);
                        self.selection_base = if additive {self.selection.clone()} else {Default::default()};
                    }
                    if let Some(anchor) = self.selection_anchor {
                        if position.distance(anchor) > 4.0 && (down || released) {
                            let rect = egui::Rect::from_two_pos(anchor, position).intersect(table.inner_rect);
                            self.selection = self.selection_base.clone();
                            for (id,row) in &row_rects {
                                if row.intersect(table.inner_rect).is_positive() && rect.intersects(*row) { self.selection.insert(id.clone()); }
                            }
                            if down {
                                let painter = ui.painter().with_clip_rect(table.inner_rect);
                                painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(40,130,240,45));
                                painter.rect_stroke(rect,0.0,egui::Stroke::new(1.0_f32,Color32::from_rgb(40,130,240)),egui::StrokeKind::Inside);
                            }
                        }
                    }
                }
                if released || !down { self.selection_anchor = None; }
                let painter = ui.painter().with_clip_rect(table.inner_rect);
                for (id,row) in &row_rects {
                    if self.selection.contains(id) { painter.rect_filled(*row,2.0,Color32::from_rgba_unmultiplied(40,130,240,25)); }
                }
            }
        });
        if let Some((id, files)) = self.media_choice.clone() {
            let mut open = true;
            let mut selected = None;
            egui::Window::new("选择要播放的文件")
                .open(&mut open)
                .default_width(580.0)
                .show(ctx, |ui| {
                    ui.weak("已自动识别音视频文件，按大小排序。播放会继续相应 BT 任务。");
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .show(ui, |ui| {
                            for file in files {
                                if ui
                                    .button(format!(
                                        "▶ {}    {}",
                                        text(&file, "path"),
                                        bytes(num(&file, "size"))
                                    ))
                                    .clicked()
                                {
                                    selected = Some(file);
                                }
                            }
                        });
                });
            if let Some(file) = selected {
                self.play_media(id, &file);
                self.media_choice = None;
            } else if !open {
                self.media_choice = None;
            }
        }
        if self.add_open {
            let mut open = true;
            egui::Window::new("新建下载任务")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(550.0)
                .show(ctx, |ui| {
                    ui.label("磁力链接 / HTTP(S) 直链 / 本机 .torrent 文件路径");
                    if ui.button("选择种子文件…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("种子", &["torrent"])
                            .pick_file()
                        {
                            self.source = path.display().to_string();
                        }
                    }
                    ui.add(
                        egui::TextEdit::multiline(&mut self.source)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.label("保存目录");
                    if ui.button("浏览目录…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_directory(&self.save_path)
                            .pick_folder()
                        {
                            self.save_path = path.display().to_string();
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.save_path)
                            .desired_width(f32::INFINITY),
                    );
                    ui.weak("可复用已有下载：选择原保存目录后，引擎会校验已有数据。");
                    ui.checkbox(&mut self.add_paused, "添加后暂停（可在“文件”页选择下载内容）");
                    ui.add_space(10.0);
                    if ui
                        .add_enabled(
                            !self.source.trim().is_empty() && !self.save_path.trim().is_empty(),
                            egui::Button::new("开始下载"),
                        )
                        .clicked()
                    {
                        let _ = self.tx.send(Command::Post(
                            "/api/tasks",
                            json!({"source":self.source,"save_path":self.save_path,"paused":self.add_paused}),
                        ));
                        self.add_open = false;
                    }
                });
            self.add_open &= open;
        }
        if self.tracker_open {
            let mut open = true;
            egui::Window::new("添加 Tracker").open(&mut open).default_width(550.0).show(ctx, |ui| {
                ui.label("每行一个 http / https / udp 地址，最多 200 个");
                ui.add(egui::TextEdit::multiline(&mut self.trackers).desired_rows(10).desired_width(f32::INFINITY));
                if ui.button("保存候选").clicked() {
                    let _ = self.tx.send(Command::Post("/api/action",json!({"id":self.selected,"action":"trackers","trackers":self.trackers})));
                    self.tracker_open = false;
                }
            });
            self.tracker_open &= open;
        }
        if self.remove_open {
            let mut open = true;
            egui::Window::new("移除任务")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("删除任务及其本机任务记录。请选择如何处理下载数据：");
                    ui.strong(format!("将移除 {} 个任务", self.remove_targets.len()));
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .show(ui, |ui| {
                            for task in tasks
                                .iter()
                                .filter(|t| self.remove_targets.contains(&text(t, "id")))
                            {
                                ui.label(text(task, "name"));
                            }
                        });
                    ui.radio_value(
                        &mut self.remove_mode,
                        "keep".into(),
                        "保留全部下载文件（默认）",
                    );
                    ui.radio_value(
                        &mut self.remove_mode,
                        "incomplete".into(),
                        "删除未完成文件 / HTTP 临时文件，保留完整文件",
                    );
                    ui.radio_value(
                        &mut self.remove_mode,
                        "all".into(),
                        "删除此任务的全部数据文件（包括已完成文件）",
                    );
                    if self.remove_mode != "keep" {
                        ui.colored_label(
                            Color32::from_rgb(208, 89, 89),
                            "文件将永久删除，不经过回收站；同目录其他文件不会删除。",
                        );
                    }
                    if ui
                        .button(if self.remove_mode == "keep" {
                            "确认移除，保留文件"
                        } else {
                            "确认移除并删除所选范围的文件"
                        })
                        .clicked()
                    {
                        let _ = self.tx.send(Command::RemoveMany(
                            self.remove_targets.clone(),
                            self.remove_mode.clone(),
                        ));
                        self.pending_action = true;
                        self.remove_open = false;
                    }
                });
            self.remove_open &= open;
        }
        if let Some(mut config) = self.settings_edit.take() {
            let mut open = true;
            let mut saved = false;
            egui::Window::new("下载设置")
                .open(&mut open)
                .default_width(570.0)
                .show(ctx, |ui| {
                    ui.label("默认下载目录（仅影响新任务）");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut config.download_dir)
                                .desired_width(430.0),
                        );
                        if ui.button("浏览…").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_directory(&config.download_dir)
                                .pick_folder()
                            {
                                config.download_dir = path.display().to_string();
                            }
                        }
                    });
                    ui.separator();
                    ui.label("全局限速，0 表示不限速；保存后立即生效。");
                    ui.checkbox(
                        &mut config.seed_after_download,
                        "下载完成后继续做种（默认关闭）",
                    );
                    ui.weak("关闭后自动停止已完成任务的上传；下载过程中仍可能上传已有分片。");
                    ui.checkbox(
                        &mut config.background_on_close,
                        "关闭窗口后在系统托盘继续下载",
                    );
                    ui.weak("托盘双击显示窗口；托盘菜单“退出并停止下载”会保存状态并退出。");
                    ui.checkbox(&mut config.clipboard_watch, "复制磁力链接时弹出新建任务（包括网页中嵌入的磁力地址）");
                    ui.weak("仅在本机识别，不访问分享网页；确认后才开始下载。Flow 需保持运行或驻留托盘。");
                    ui.horizontal(|ui| {
                        ui.label("下载 KiB/s");
                        ui.add(egui::DragValue::new(&mut config.download_kib).range(0..=4_000_000));
                    });
                    ui.horizontal(|ui| {
                        ui.label("上传 KiB/s");
                        ui.add(egui::DragValue::new(&mut config.upload_kib).range(0..=4_000_000));
                    });
                    ui.horizontal(|ui| {
                        ui.label("连接上限（重启后生效）");
                        ui.add(egui::DragValue::new(&mut config.peer_limit).range(10..=2000));
                    });
                    if let Err(e) = config.validate() {
                        ui.colored_label(Color32::from_rgb(208, 89, 89), e.to_string());
                    }
                    if ui
                        .add_enabled(config.validate().is_ok(), egui::Button::new("保存设置"))
                        .clicked()
                    {
                        let _ = self.tx.send(Command::Post(
                            "/api/settings",
                            serde_json::to_value(&config).unwrap(),
                        ));
                        saved = true;
                    }
                });
            if open && !saved {
                self.settings_edit = Some(config);
            }
        }
        if let Some(mut config) = self.subscription_edit.take() {
            let mut open = true;
            let mut saved = false;
            egui::Window::new("Tracker 订阅设置").open(&mut open).default_width(660.0).show(ctx, |ui| {
                ui.label("同一来源的镜像按顺序尝试；全部失败时沿用缓存。");
                ui.horizontal(|ui| {
                    ui.label("列表更新（小时）"); ui.add(egui::DragValue::new(&mut config.refresh_hours).range(1..=168));
                    ui.label("健康检查（分钟）"); ui.add(egui::DragValue::new(&mut config.health_minutes).range(5..=1440));
                });
                let mut remove = None;
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for (index, source) in config.sources.iter_mut().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut source.enabled, "启用");
                                ui.text_edit_singleline(&mut source.name);
                                if ui.small_button("移除订阅").clicked() { remove = Some(index); }
                            });
                            let mut urls = source.urls.join("\n");
                            ui.label("镜像地址，每行一个");
                            if ui.add(egui::TextEdit::multiline(&mut urls).desired_rows(3).desired_width(f32::INFINITY)).changed() {
                                    source.urls = urls.split('\n').map(str::to_string).collect();
                            }
                        });
                    }
                });
                if let Some(index) = remove { config.sources.remove(index); }
                if config.sources.len() < 16 && ui.button("新增订阅源").clicked() {
                    config.sources.push(subscriptions::Source { name: format!("自定义 {}", config.sources.len() + 1), enabled: true, urls: vec![String::new()] });
                }
                ui.weak("后台检查不会中断下载。新候选通过“应用候选”载入引擎；移除订阅不会删除任务原有 Tracker。");
                if let Err(e) = config.validate() { ui.colored_label(Color32::from_rgb(208,89,89), e.to_string()); }
                if ui.add_enabled(config.validate().is_ok(), egui::Button::new("保存设置")).clicked() {
                    let _ = self.tx.send(Command::Post("/api/subscriptions", serde_json::to_value(&config).unwrap()));
                    saved = true;
                }
            });
            if open && !saved {
                self.subscription_edit = Some(config);
            }
        }
    }
}

impl Drop for DownloadApp {
    fn drop(&mut self) {
        self.clipboard_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(worker) = self.clipboard_worker.take() {
            let _ = worker.join();
        }
        let _ = self.tx.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn matches_filter(t: &Value, filter: usize) -> bool {
    match filter {
        1 => {
            num(t, "progress") < 1.0
                && !t["paused"].as_bool().unwrap_or(false)
                && text(t, "error").is_empty()
        }
        2 => {
            t["paused"].as_bool().unwrap_or(false)
                && num(t, "progress") < 1.0
                && text(t, "error").is_empty()
        }
        3 => num(t, "progress") >= 1.0 && text(t, "error").is_empty(),
        4 => !text(t, "error").is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod task_filter_tests {
    use super::*;
    #[test]
    fn completed_stopped_tasks_are_not_counted_as_paused() {
        for (task, expected) in [
            (json!({"progress":1.0,"paused":true,"error":""}), 3),
            (json!({"progress":0.4,"paused":true,"error":""}), 2),
            (json!({"progress":0.4,"paused":false,"error":""}), 1),
            (json!({"progress":1.0,"paused":true,"error":"failed"}), 4),
        ] {
            assert_eq!(
                (1..=4)
                    .filter(|f| matches_filter(&task, *f))
                    .collect::<Vec<_>>(),
                vec![expected]
            );
        }
    }
}

fn main() -> eframe::Result {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--check-clipboard") {
        let result = arboard::Clipboard::new().and_then(|mut c| c.get_text());
        let report = json!({"sequence":open_request::clipboard_sequence(),"readable":result.is_ok(),"magnet_detected":result.as_ref().ok().and_then(|s|open_request::copied_magnet(s)).is_some()});
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                let _ = std::fs::write(
                    parent.join("clipboard-check.json"),
                    serde_json::to_vec_pretty(&report).unwrap(),
                );
            }
        }
        return Ok(());
    }
    let external = if args.first().is_some_and(|s| s == "--open") {
        args.get(1)
    } else {
        args.first().filter(|s| !s.starts_with("--"))
    };
    let mut open_error = None;
    if let Some(source) = external {
        match backend::Backend::root()
            .map_err(anyhow::Error::msg)
            .and_then(|root| open_request::enqueue(&root, source))
        {
            Ok(()) => {}
            Err(error) => open_error = Some(error.to_string()),
        }
    }
    if std::env::args().any(|a| a == "--check-backend") {
        let owned_backend = backend::Backend::start();
        let report = match &owned_backend {
            Ok(b) => json!({"ok":true,"root":b.root,"engine":"librqbit 9.0.1","native_rust":true}),
            Err(e) => json!({"ok":false,"error":e}),
        };
        let parent = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let _ = std::fs::write(
            parent.join("self-check.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        );
        drop(owned_backend);
        if report["ok"] != true {
            std::process::exit(1);
        }
        return Ok(());
    }
    let root =
        backend::Backend::root().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/flow-icon.png"))
                    .expect("embedded app icon"),
            )
            .with_inner_size([1320.0, 820.0])
            .with_min_inner_size([1050.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Flow · 下载工作台",
        options,
        Box::new(move |cc| {
            let mut app = DownloadApp::new(cc, root);
            if let Some(message) = open_error {
                app.message = message;
            }
            Ok(Box::new(app))
        }),
    )
}
