//! Flow owns the controls; mpv handles decoding in an independent video window.
use eframe::egui;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

pub struct Player {
    pub open: bool,
    pub source: String,
    root: PathBuf,
    child: Option<Child>,
    commands: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    events: mpsc::Receiver<Value>,
    props: Value,
    pub message: String,
    title: String,
    seek: f64,
    history: std::collections::BTreeMap<String, f64>,
    current_key: String,
    setup: Option<mpsc::Receiver<crate::player_setup::Event>>,
    setup_status: String,
    pending_play: Option<(String, String, Option<String>)>,
}

impl Player {
    pub fn new(root: PathBuf) -> Self {
        let history = std::fs::read(root.join("data/player-history.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            history,
            current_key: String::new(),
            setup: None,
            setup_status: String::new(),
            pending_play: None,
            open: false,
            source: String::new(),
            root,
            child: None,
            commands: None,
            events: mpsc::channel().1,
            props: json!({}),
            message: String::new(),
            title: String::new(),
            seek: 0.0,
        }
    }
    fn ensure_started(&mut self) -> anyhow::Result<()> {
        if let Some(child) = &mut self.child {
            if child.try_wait()?.is_none() {
                return Ok(());
            }
        }
        let exe = self.root.join("runtime/mpv/mpv.exe");
        anyhow::ensure!(exe.is_file(), "缺少播放内核，请点击安装播放器");
        let pipe = format!(r"\\.\pipe\flow-player-{}", uuid::Uuid::new_v4().simple());
        let mut command = Command::new(exe);
        command
            .args([
                "--no-config",
                "--idle=yes",
                "--force-window=yes",
                "--osc=yes",
                "--terminal=no",
                "--title=Flow · 视频",
                "--keep-open=yes",
                "--cache=yes",
                "--cache-secs=20",
                "--demuxer-max-bytes=67108864",
                "--network-timeout=20",
                "--ytdl=no",
            ])
            .arg(format!("--input-ipc-server={pipe}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        #[cfg(test)]
        command.args(["--vo=null", "--ao=null", "--force-window=no"]);
        self.child = Some(command.spawn()?);
        let (tx, mut commands) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let (events, rx) = mpsc::channel();
        self.commands = Some(tx);
        self.events = rx;
        self.props = json!({});
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    #[cfg(windows)] {
                        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                        let mut attempts = 0;
                        let client = loop {
                            match tokio::net::windows::named_pipe::ClientOptions::new().open(&pipe) {
                                Ok(client) => break client,
                                Err(e) => { attempts += 1; if attempts >= 100 { return Err(e.into()); }
                                    tokio::time::sleep(Duration::from_millis(50)).await; }
                            }
                        };
                        let (reader, mut writer) = tokio::io::split(client);
                        let mut lines = tokio::io::BufReader::new(reader).lines();
                        for (id, property) in ["time-pos", "duration", "pause", "volume", "speed", "paused-for-cache", "demuxer-cache-duration", "track-list", "eof-reached"].iter().enumerate() {
                            let text = format!("{}\n",json!({"command":["observe_property",id,property]}));
                            writer.write_all(text.as_bytes()).await?;
                        }
                        loop {
                            tokio::select! {
                                command = commands.recv() => {
                                    let Some(command) = command else { break; };
                                    writer.write_all(format!("{}\n",json!({"command":command})).as_bytes()).await?;
                                }
                                line = lines.next_line() => {
                                    let Some(line) = line? else { break; };
                                    if let Ok(value) = serde_json::from_str(&line) { if events.send(value).is_err() { break; } }
                                }
                            }
                        }
                    }
                    Ok(())
                })
            })();
            if let Err(error) = result {
                let _ = events.send(json!({"flow-error":error.to_string()}));
            }
        });
        Ok(())
    }
    pub fn command(&self, command: Value) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(command);
        }
    }
    fn remember(&mut self) {
        if self.current_key.is_empty() {
            return;
        }
        let position = self.props["time-pos"].as_f64().unwrap_or(0.0);
        if self.props["eof-reached"] == true {
            self.history.remove(&self.current_key);
        } else if position > 1.0 {
            self.history.insert(self.current_key.clone(), position);
        }
        // Keep this local-only history bounded.
        while self.history.len() > 200 {
            if let Some(key) = self.history.keys().next().cloned() {
                self.history.remove(&key);
            }
        }
        let _ = std::fs::create_dir_all(self.root.join("data"));
        if let Ok(bytes) = serde_json::to_vec(&self.history) {
            let _ = std::fs::write(self.root.join("data/player-history.json"), bytes);
        }
    }
    pub fn play(&mut self, source: String, title: String, token: Option<&str>) {
        if !self.root.join("runtime/mpv/mpv.exe").is_file() {
            self.pending_play = Some((source, title, token.map(str::to_owned)));
            self.open = true;
            self.begin_setup();
            return;
        }
        self.remember();
        self.open = false;
        self.message.clear();
        if let Err(e) = self.ensure_started() {
            self.open = true;
            self.message = e.to_string();
            return;
        }
        self.current_key = if token.is_some() {
            url::Url::parse(&source)
                .map(|u| format!("torrent:{}", u.path()))
                .unwrap_or(source.clone())
        } else {
            source.clone()
        };
        self.title = title;
        self.seek = 0.0;
        self.props = json!({});
        let mut options = json!({});
        if let Some(position) = self.history.get(&self.current_key) {
            options["start"] = json!(position.to_string());
        }
        if let Some(token) = token {
            options["http-header-fields"] = json!(format!("Authorization: Bearer {token}"));
        }
        self.command(json!(["loadfile", source, "replace", -1, options]));
        self.command(json!(["set_property", "pause", false]));
    }
    fn open_source(&mut self) {
        let source = self.source.trim().to_owned();
        if std::path::Path::new(&source).is_file() {
            match std::fs::canonicalize(&source) {
                Ok(path) => self.play(path.display().to_string(), source, None),
                Err(e) => self.message = e.to_string(),
            }
        } else if url::Url::parse(&source)
            .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        {
            self.play(source.clone(), source, None);
        } else {
            self.message =
                "请选择本地媒体或输入 HTTP(S) 媒体直链。磁力链接请先添加下载任务，再在文件页播放。"
                    .into();
        }
    }
    fn begin_setup(&mut self) {
        if self.setup.is_none() {
            self.message.clear();
            self.setup_status = "正在准备播放内核…".into();
            self.setup = Some(crate::player_setup::start(self.root.clone()));
        }
    }
    pub fn ui(&mut self, ctx: &egui::Context) {
        if self.setup.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
            let mut finished = None;
            if let Some(rx) = &self.setup {
                loop {
                    match rx.try_recv() {
                        Ok(crate::player_setup::Event::Progress(status)) => {
                            self.setup_status = status
                        }
                        Ok(crate::player_setup::Event::Finished(result)) => {
                            finished = Some(result);
                            break;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            finished = Some(Err("播放器安装中断，请重试".into()));
                            break;
                        }
                    }
                }
            }
            if let Some(result) = finished {
                self.setup = None;
                self.setup_status.clear();
                match result {
                    Ok(()) => {
                        self.message.clear();
                        if let Some((source, title, token)) = self.pending_play.take() {
                            self.play(source, title, token.as_deref());
                        }
                    }
                    Err(error) => {
                        self.message = error;
                        self.open = true;
                    }
                }
            }
        }
        while let Ok(event) = self.events.try_recv() {
            if event["event"] == "property-change" {
                if let Some(name) = event["name"].as_str() {
                    self.props[name] = event["data"].clone();
                }
            }
            if event["event"] == "end-file" && event["reason"] == "error" {
                self.open = true;
                self.message = format!(
                    "播放失败：{}",
                    event["file_error"]
                        .as_str()
                        .unwrap_or("无法读取媒体，检查来源或缓冲状态")
                );
            }
            if let Some(error) = event["flow-error"].as_str() {
                self.open = true;
                self.message = error.into();
            }
            if let Some(error) = event["error"].as_str() {
                if error != "success" {
                    self.message = format!("播放控制失败：{error}");
                }
            }
        }
        if self
            .child
            .as_mut()
            .is_some_and(|c| c.try_wait().ok().flatten().is_some())
        {
            self.remember();
            self.child = None;
            self.commands = None;
            self.props = json!({});
            self.message = "视频窗口已关闭".into();
        }
        if !self.open {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let mut open = self.open;
        egui::Window::new("Flow 播放器").open(&mut open).default_width(640.0).show(ctx,|ui| {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("FLOW / PLAY").color(egui::Color32::from_rgb(28,180,164)));
                if ui.button("打开文件…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() { self.source=path.display().to_string(); self.open_source(); }
                }
            });
            ui.horizontal(|ui| { ui.add(egui::TextEdit::singleline(&mut self.source).hint_text("本地文件路径 / HTTP(S) 媒体直链").desired_width(480.0));
                if ui.button("播放").clicked() { self.open_source(); }
            });
            ui.separator();
            if self.setup.is_some() {
                ui.horizontal(|ui| { ui.spinner(); ui.label(&self.setup_status); });
                ui.weak("首次播放会自动下载并校验 mpv，完成后继续播放；不需要运行脚本。");
                if self.pending_play.is_some() && ui.button("安装完成后不自动播放").clicked() { self.pending_play = None; }
            } else if !self.root.join("runtime/mpv/mpv.exe").is_file() {
                ui.label("播放器尚未安装。首次播放也会自动配置。");
                if ui.button("安装 / 重试安装播放器").clicked() { self.begin_setup(); }
            }
            ui.label(egui::RichText::new(&self.title).strong());
            let buffering=self.props["paused-for-cache"]==true;
            ui.label(if buffering {"正在缓冲 · 等待播放所需数据"} else if self.props["eof-reached"]==true {"播放结束"} else if self.props["pause"]==true {"已暂停播放"} else if self.child.is_some() {"播放内核已连接"} else {"打开媒体开始播放"});
            let duration=self.props["duration"].as_f64().unwrap_or(0.0);
            let position=self.props["time-pos"].as_f64().unwrap_or(0.0);
            ui.label(format!("{} / {}    已缓冲 {:.1} 秒", clock(position),clock(duration),self.props["demuxer-cache-duration"].as_f64().unwrap_or(0.0)));
            // Retain the drag target while new time-pos events arrive.
            let seek_id=ui.id().with("seek");
            if !ui.ctx().dragged_id().is_some() { self.seek=position; }
            ui.push_id(seek_id,|ui| {
                let slider=ui.add_enabled(duration>0.0,egui::Slider::new(&mut self.seek,0.0..=duration.max(1.0)).show_value(false));
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) { self.command(json!(["seek",self.seek,"absolute+exact"])); }
            });
            ui.horizontal(|ui| {
                if ui.button("−10 秒").clicked() {self.command(json!(["seek",-10,"relative"]));}
                if ui.button(if self.props["pause"]==true {"▶ 继续"} else {"Ⅱ 暂停"}).clicked() {self.command(json!(["cycle","pause"]));}
                if ui.button("停止").clicked() {self.pending_play=None;self.command(json!(["stop"]));self.props=json!({});}
                if ui.button("+10 秒").clicked() {self.command(json!(["seek",10,"relative"]));}
                if ui.button("全屏").clicked() {self.command(json!(["cycle","fullscreen"]));}
            });
            ui.horizontal(|ui| {
                let mut volume=self.props["volume"].as_f64().unwrap_or(100.0);
                if ui.add(egui::Slider::new(&mut volume,0.0..=100.0).text("音量")).changed() {self.command(json!(["set_property","volume",volume]));}
                let mut speed=self.props["speed"].as_f64().unwrap_or(1.0);
                egui::ComboBox::from_id_salt("play-speed").selected_text(format!("{speed}×")).show_ui(ui,|ui| {
                    for value in [0.5,0.75,1.0,1.25,1.5,2.0] { if ui.selectable_value(&mut speed,value,format!("{value}×")).changed() {self.command(json!(["set_property","speed",speed]));} }
                });
            });
            ui.horizontal(|ui| {
                if ui.button("加载字幕…").clicked() {if let Some(path)=rfd::FileDialog::new().add_filter("字幕", &["srt","ass","ssa","vtt"]).pick_file() {self.command(json!(["sub-add",path.display().to_string(),"select"]));}}
                if ui.button("关闭字幕").clicked() {self.command(json!(["set_property","sid","no"]));}
                if let Some(tracks)=self.props["track-list"].as_array() {
                    for (kind,label,property) in [("audio","音轨","aid"),("sub","字幕","sid")] {
                        egui::ComboBox::from_id_salt(property).selected_text(label).show_ui(ui,|ui| {
                            for track in tracks.iter().filter(|t|t["type"]==kind) {
                                let label=format!("{} · {} {}",track["id"],track["lang"].as_str().unwrap_or(""),track["title"].as_str().unwrap_or(""));
                                if ui.selectable_label(track["selected"]==true,label).clicked() {self.command(json!(["set_property",property,track["id"]]));}
                            }
                        });
                    }
                }
            });
            ui.weak("视频在独立窗口显示。画面窗口：移动鼠标显示进度条，可拖动跳转、点击暂停和调音量；空格暂停，F 全屏。关闭此控制面板不停止播放。停止播放不会暂停下载任务。");
            if !self.message.is_empty() {ui.colored_label(egui::Color32::from_rgb(200,85,80), &self.message);}
        });
        self.open = self.open && open;
    }
}

fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}
impl Drop for Player {
    fn drop(&mut self) {
        self.remember();
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the optional mpv runtime"]
    fn real_mpv_decodes_seeks_and_pauses_generated_audio() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fixture.wav");
        let size = 8000u32 * 2 * 20;
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend((size + 36).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(8000u32.to_le_bytes());
        wav.extend(16000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(size.to_le_bytes());
        wav.resize(44 + size as usize, 0);
        std::fs::write(&path, wav).unwrap();
        let mut player = Player::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        player.play(path.display().to_string(), "fixture".into(), None);
        assert!(player.message.is_empty(), "{}", player.message);
        fn wait_property(player: &mut Player, name: &str, predicate: impl Fn(&Value) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "missing property {name}: {:?}",
                    player.props
                );
                if let Ok(event) = player.events.recv_timeout(Duration::from_millis(200)) {
                    assert!(event["flow-error"].is_null(), "{event}");
                    if event["event"] == "property-change" {
                        let key = event["name"].as_str().unwrap();
                        player.props[key] = event["data"].clone();
                        if key == name && predicate(&event["data"]) {
                            break;
                        }
                    }
                }
            }
        }
        wait_property(&mut player, "duration", |v| {
            v.as_f64().is_some_and(|n| n >= 19.9)
        });
        player.command(json!(["set_property", "pause", true]));
        wait_property(&mut player, "pause", |v| v == true);
        player.command(json!(["seek", 8, "absolute+exact"]));
        wait_property(&mut player, "time-pos", |v| {
            v.as_f64().is_some_and(|n| (n - 8.0).abs() < 0.5)
        });
        player.command(json!(["set_property", "speed", 1.5]));
        wait_property(&mut player, "speed", |v| v.as_f64() == Some(1.5));
        player.current_key.clear(); // A fixture must not enter the user's history.
    }
}
