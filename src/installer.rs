use anyhow::{Context, Result, ensure};
use eframe::egui;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
};
const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Flow";
fn hidden(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
}
fn reg(args: &[&str]) -> Result<()> {
    let exe = PathBuf::from(std::env::var_os("SystemRoot").context("Windows 目录不可用")?)
        .join("System32/reg.exe");
    let mut cmd = Command::new(exe);
    cmd.args(args);
    hidden(&mut cmd);
    let result = cmd.output()?;
    ensure!(
        result.status.success(),
        "注册安装信息失败：{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}
fn shortcuts(root: &Path, desktop: bool, remove: bool) -> Result<()> {
    run_shortcuts(root, desktop, remove, None)
}
fn run_shortcuts(
    root: &Path,
    desktop: bool,
    remove: bool,
    test_directory: Option<&Path>,
) -> Result<()> {
    let script = r#"$ErrorActionPreference='Stop'; $root=$env:FLOW_INSTALL_ROOT; $shell=New-Object -ComObject WScript.Shell; $paths=@((Join-Path ([Environment]::GetFolderPath('Programs')) 'Flow.lnk')); if($env:FLOW_INSTALL_DESKTOP -eq '1'){$paths+=Join-Path ([Environment]::GetFolderPath('Desktop')) 'Flow.lnk'}; foreach($path in $paths){if($env:FLOW_INSTALL_REMOVE -eq '1'){if(Test-Path -LiteralPath $path){$link=$shell.CreateShortcut($path);if($link.TargetPath -eq (Join-Path $root 'Flow.exe')){Remove-Item -LiteralPath $path}}}else{$link=$shell.CreateShortcut($path);$link.TargetPath=Join-Path $root 'Flow.exe';$link.WorkingDirectory=$root;$link.IconLocation=(Join-Path $root 'Flow.exe')+',0';$link.Save()}}"#;
    // Canonical Windows paths use \\?\, which PowerShell 5 providers and Shell links do not accept.
    let root = dunce::simplified(root);
    let script = format!(
        "[Console]::OutputEncoding=New-Object System.Text.UTF8Encoding; try {{ {script} }} catch {{ [Console]::Error.WriteLine($_.Exception.Message); exit 1 }}"
    );
    let script = script.replace("foreach($path in $paths)", "if($env:FLOW_SHORTCUT_TEST_DIR){$paths=@([IO.Path]::Combine($env:FLOW_SHORTCUT_TEST_DIR,'Flow.lnk'))}; foreach($path in $paths)");
    let exe = PathBuf::from(std::env::var_os("SystemRoot").context("Windows 目录不可用")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut cmd = Command::new(exe);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("FLOW_INSTALL_ROOT", root)
        .env("FLOW_INSTALL_DESKTOP", if desktop { "1" } else { "0" })
        .env("FLOW_INSTALL_REMOVE", if remove { "1" } else { "0" });
    // Never inherit a test override from the launching process.
    cmd.env_remove("FLOW_SHORTCUT_TEST_DIR");
    if let Some(directory) = test_directory {
        cmd.env("FLOW_SHORTCUT_TEST_DIR", dunce::simplified(directory));
    }
    hidden(&mut cmd);
    let result = cmd.output()?;
    ensure!(
        result.status.success(),
        "快捷方式操作失败：{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}
fn place_payload(source: &Path, root: &Path) -> Result<()> {
    let target = root.join("Flow.exe");
    let staged = root.join("Flow.installing.exe");
    std::fs::copy(source, &staged)?;
    let mut backup = None;
    if target.exists() {
        let dir = root.join("data/install-backups");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("Flow-{}.exe", uuid::Uuid::new_v4()));
        std::fs::rename(&target, &path)?;
        backup = Some(path);
    }
    if let Err(error) = std::fs::rename(staged, &target) {
        if let Some(backup) = backup {
            let _ = std::fs::rename(backup, &target);
        }
        return Err(error.into());
    }

    Ok(())
}
fn install(source: &Path, root: &Path, desktop: bool) -> Result<()> {
    ensure!(root.is_absolute(), "请选择绝对安装路径");
    std::fs::create_dir_all(root)?;
    let root = dunce::canonicalize(root)?;
    ensure!(root.parent().is_some(), "不能安装到磁盘根目录");
    std::fs::create_dir_all(root.join("data"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("data/engine.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("此目录的 Flow 正在运行，请先从托盘退出")?;
    place_payload(source, &root)?;
    let target = root.join("Flow.exe");
    std::fs::write(
        root.join("flow-install.json"),
        serde_json::to_vec(
            &serde_json::json!({"product":"Flow","version":env!("CARGO_PKG_VERSION")}),
        )?,
    )?;
    let uninstall = format!("\"{}\" --uninstall", target.display());
    let root_text = root.display().to_string();
    let exe_text = target.display().to_string();
    for (name, value) in [
        ("DisplayName", "Flow"),
        ("DisplayVersion", env!("CARGO_PKG_VERSION")),
        ("Publisher", "wrench1997"),
        ("InstallLocation", root_text.as_str()),
        ("DisplayIcon", exe_text.as_str()),
        ("UninstallString", uninstall.as_str()),
    ] {
        reg(&[
            "add",
            UNINSTALL_KEY,
            "/v",
            name,
            "/t",
            "REG_SZ",
            "/d",
            value,
            "/f",
        ])?;
    }
    shortcuts(&root, desktop, false)?;
    Ok(())
}
pub fn requested(args: &[String]) -> bool {
    args.first().is_some_and(|s| {
        matches!(
            s.as_str(),
            "--install" | "--uninstall" | "--finish-uninstall"
        )
    }) || std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("Flow-Setup-"))
        })
        .unwrap_or(false)
}
struct Installer {
    directory: String,
    desktop: bool,
    uninstall: bool,
    status: String,
    rx: Option<mpsc::Receiver<Result<(), String>>>,
    done: bool,
}
impl eframe::App for Installer {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if let Some(rx) = &self.rx {
            if let Ok(result) = rx.try_recv() {
                self.rx = None;
                match result {
                    Ok(()) => {
                        self.done = true;
                        self.status = if self.uninstall {
                            "卸载已安排；关闭后移除程序，保留下载和个人数据。"
                        } else {
                            "安装完成"
                        }
                        .into();
                    }
                    Err(e) => self.status = e,
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(if self.uninstall {
                "卸载 Flow"
            } else {
                concat!("安装 Flow ", env!("CARGO_PKG_VERSION"))
            });
            ui.add_space(12.0);
            ui.label(if self.uninstall {
                "仅移除程序和快捷方式，保留 data、runtime 及下载文件。"
            } else {
                "为当前 Windows 用户安装，无需管理员权限。"
            });
            ui.add_enabled(
                !self.uninstall && self.rx.is_none() && !self.done,
                egui::TextEdit::singleline(&mut self.directory).desired_width(500.0),
            );
            if !self.uninstall && !self.done {
                if ui
                    .add_enabled(self.rx.is_none(), egui::Button::new("选择目录…"))
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.directory = path.display().to_string();
                    }
                }
                ui.checkbox(&mut self.desktop, "创建桌面快捷方式");
            }
            if ui
                .add_enabled(
                    self.rx.is_none() && !self.done,
                    egui::Button::new(if self.uninstall {
                        "确认卸载"
                    } else {
                        "安装"
                    }),
                )
                .clicked()
            {
                let root = PathBuf::from(&self.directory);
                let desktop = self.desktop;
                let uninstall = self.uninstall;
                let (tx, rx) = mpsc::channel();
                self.rx = Some(rx);
                self.status = "正在处理…".into();
                std::thread::spawn(move || {
                    let result = if uninstall {
                        start_uninstall(&root)
                    } else {
                        std::env::current_exe()
                            .map_err(anyhow::Error::from)
                            .and_then(|exe| install(&exe, &root, desktop))
                    };
                    let _ = tx.send(result.map_err(|e| format!("{e:#}")));
                });
            }
            ui.label(&self.status);
            if self.done {
                if !self.uninstall && ui.button("启动 Flow").clicked() {
                    let mut cmd = Command::new(PathBuf::from(&self.directory).join("Flow.exe"));
                    hidden(&mut cmd);
                    match cmd.spawn() {
                        Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(e) => self.status = e.to_string(),
                    }
                }
                if ui.button("关闭").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        });
    }
}
fn start_uninstall(root: &Path) -> Result<()> {
    ensure!(
        root.join("flow-install.json").is_file(),
        "没有找到 Flow 安装记录"
    );
    // Do not remove an installed copy while a download instance owns its data.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("data/engine.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Flow 仍在运行，请先从托盘退出")?;
    shortcuts(root, true, true)?;
    reg(&["delete", UNINSTALL_KEY, "/f"])?;
    let dir = std::env::temp_dir().join(format!("flow-uninstall-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir)?;
    let helper = dir.join("helper.exe");
    std::fs::copy(std::env::current_exe()?, &helper)?;
    let mut cmd = Command::new(helper);
    cmd.arg("--finish-uninstall")
        .arg(root)
        .arg(std::process::id().to_string());
    hidden(&mut cmd);
    cmd.spawn()?;
    Ok(())
}
pub fn run(args: Vec<String>) -> eframe::Result {
    if args.first().is_some_and(|s| s == "--finish-uninstall") {
        let result = (|| -> Result<()> {
            let root = PathBuf::from(args.get(1).context("缺少目录")?);
            let marker: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("flow-install.json"))?)?;
            ensure!(
                root.is_absolute() && marker["product"] == "Flow",
                "卸载目录无效"
            );
            crate::updater::wait_for_process(args.get(2).context("缺少进程")?.parse()?)?;
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .open(root.join("data/engine.lock"))?;
            fs2::FileExt::try_lock_exclusive(&lock).context("Flow 正在运行")?;
            std::fs::remove_file(root.join("Flow.exe"))?;
            std::fs::remove_file(root.join("flow-install.json"))?;
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let uninstall = args.first().is_some_and(|s| s == "--uninstall");
    let directory = if uninstall {
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    } else {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| "C:\\Flow".into()))
            .join("Programs/Flow")
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 300.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/flow-icon.png"))
                    .unwrap(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "Flow Setup",
        options,
        Box::new(move |cc| {
            let mut fonts = egui::FontDefinitions::default();
            if let Ok(bytes) = std::fs::read("C:/Windows/Fonts/msyh.ttc") {
                fonts
                    .font_data
                    .insert("chinese".into(), egui::FontData::from_owned(bytes).into());
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push("chinese".into());
            }
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(Installer {
                directory: directory.display().to_string(),
                desktop: true,
                uninstall,
                status: String::new(),
                rx: None,
                done: false,
            }))
        }),
    )
}
#[cfg(test)]
mod tests {
    #[test]
    #[cfg(windows)]
    fn canonical_paths_create_and_remove_real_shortcuts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("中文 path [test] ' &");
        let links = temp.path().join("links");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&links).unwrap();
        std::fs::write(root.join("Flow.exe"), b"fixture").unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        super::run_shortcuts(&canonical, true, false, Some(&links)).unwrap();
        assert!(links.join("Flow.lnk").is_file());
        // Repeat after a partial installation, then remove only a matching target.
        super::run_shortcuts(&canonical, true, false, Some(&links)).unwrap();
        super::run_shortcuts(temp.path(), true, true, Some(&links)).unwrap();
        assert!(links.join("Flow.lnk").is_file());
        super::run_shortcuts(&canonical, true, true, Some(&links)).unwrap();
        assert!(!links.join("Flow.lnk").exists());
    }

    #[test]
    fn installation_preserves_user_data_and_backs_up_executable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data/tasks.json"), b"user tasks").unwrap();
        std::fs::write(dir.path().join("Flow.exe"), b"old").unwrap();
        let source = dir.path().join("setup.exe");
        std::fs::write(&source, b"new").unwrap();
        super::place_payload(&source, dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("Flow.exe")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.path().join("data/tasks.json")).unwrap(),
            b"user tasks"
        );
        let backup = std::fs::read_dir(dir.path().join("data/install-backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read(backup).unwrap(), b"old");
    }
    #[test]
    fn setup_detection_does_not_match_normal_arguments() {
        assert!(!super::requested(&["--open".into(), "test.torrent".into()]));
        assert!(super::requested(&["--install".into()]));
    }
}
