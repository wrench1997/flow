use anyhow::{Context, Result, ensure};
use eframe::egui;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};
const MANIFEST_URL: &str =
    "https://github.com/wrench1997/flow/releases/latest/download/update.json";
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    auto_check: bool,
    mirror: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            auto_check: true,
            mirror: String::new(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}
#[derive(Deserialize)]
struct Envelope {
    payload: String,
    signature: String,
}
fn unhex(s: &str) -> Result<Vec<u8>> {
    ensure!(s.len() % 2 == 0, "签名格式错误");
    s.as_bytes()
        .chunks(2)
        .map(|p| Ok(u8::from_str_radix(std::str::from_utf8(p)?, 16)?))
        .collect()
}
fn version(s: &str) -> Result<[u64; 3]> {
    let parts = s
        .strip_prefix('v')
        .unwrap_or(s)
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    parts
        .try_into()
        .map_err(|_| anyhow::anyhow!("版本格式错误"))
}
fn verify_with_key(bytes: &[u8], key: &[u8]) -> Result<Manifest> {
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    let key: [u8; 32] = key.try_into().context("更新公钥无效")?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&key)?;
    let signature = ed25519_dalek::Signature::from_slice(&unhex(&envelope.signature)?)?;
    key.verify_strict(envelope.payload.as_bytes(), &signature)
        .context("更新签名无效，已拒绝更新")?;
    let manifest: Manifest = serde_json::from_str(&envelope.payload)?;
    version(&manifest.version)?;
    ensure!(
        manifest.url
            == format!(
                "https://github.com/wrench1997/flow/releases/download/v{}/Flow.exe",
                manifest.version
            ),
        "更新地址无效"
    );
    ensure!(
        manifest.sha256.len() == 64 && manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "更新校验值无效"
    );
    ensure!(
        manifest.size > 0 && manifest.size <= 200 * 1024 * 1024,
        "更新文件大小无效"
    );
    Ok(manifest)
}
fn verify(bytes: &[u8]) -> Result<Manifest> {
    verify_with_key(
        bytes,
        &unhex(include_str!("../assets/update-public-key.hex").trim())?,
    )
}
fn mirror_url(mirror: &str, original: &str) -> Result<String> {
    if mirror.trim().is_empty() {
        return Ok(original.into());
    }
    let parsed = url::Url::parse(mirror.trim())?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "镜像前缀必须是无账号、无查询参数的 HTTPS 地址"
    );
    Ok(format!(
        "{}/{}",
        mirror.trim().trim_end_matches('/'),
        original
    ))
}
fn client(timeout: u64) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(12))
        .timeout(Duration::from_secs(timeout))
        .user_agent("Flow-updater")
        .build()?)
}
fn check(mirror: &str) -> Result<(Manifest, Vec<u8>)> {
    let mut endpoints = vec![mirror_url(mirror, MANIFEST_URL)?];
    if endpoints[0] != MANIFEST_URL {
        endpoints.push(MANIFEST_URL.into());
    }
    let mut errors = Vec::new();
    for url in endpoints {
        let result = (|| -> Result<_> {
            let response = client(25)?.get(&url).send()?.error_for_status()?;
            let mut bytes = Vec::new();
            response.take(65537).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 65536, "更新清单过大");
            Ok((verify(&bytes)?, bytes))
        })();
        match result {
            Ok(value) => return Ok(value),
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    anyhow::bail!("检查失败：{}", errors.join("；"))
}
fn download(
    manifest: &Manifest,
    mirror: &str,
    path: &Path,
    progress: impl Fn(String),
) -> Result<()> {
    let mut endpoints = vec![mirror_url(mirror, &manifest.url)?];
    if endpoints[0] != manifest.url {
        endpoints.push(manifest.url.clone());
    }
    let mut errors = Vec::new();
    for url in endpoints {
        let result = (|| -> Result<()> {
            let mut response = client(600)?.get(url).send()?.error_for_status()?;
            let mut file = std::fs::File::create(path)?;
            let mut digest = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0; 65536];
            let mut last = std::time::Instant::now();
            loop {
                let n = response.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                ensure!(size <= manifest.size, "下载文件超过签名清单的大小");
                file.write_all(&buffer[..n])?;
                digest.update(&buffer[..n]);
                if last.elapsed() > Duration::from_millis(250) {
                    progress(format!(
                        "正在下载 {:.1} / {:.1} MiB",
                        size as f64 / 1048576.0,
                        manifest.size as f64 / 1048576.0
                    ));
                    last = std::time::Instant::now();
                }
            }
            file.sync_all()?;
            ensure!(
                size == manifest.size
                    && format!("{:x}", digest.finalize()) == manifest.sha256.to_lowercase(),
                "更新文件校验失败"
            );
            Ok(())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    anyhow::bail!("下载失败，可重试：{}", errors.join("；"))
}
enum Event {
    Checked(Result<(Manifest, Vec<u8>), String>),
    Progress(String),
    Downloaded(Result<PathBuf, String>),
}
pub struct Updater {
    pub open: bool,
    root: PathBuf,
    config: Config,
    status: String,
    rx: Option<mpsc::Receiver<Event>>,
    candidate: Option<(Manifest, Vec<u8>)>,
    ready: Option<PathBuf>,
    auto_started: bool,
}
impl Updater {
    pub fn new(root: PathBuf) -> Self {
        let config = std::fs::read(root.join("data/update-settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let status =
            std::fs::read_to_string(root.join("data/update-result.txt")).unwrap_or_default();
        Self {
            open: false,
            root,
            config,
            status,
            rx: None,
            candidate: None,
            ready: None,
            auto_started: false,
        }
    }
    fn start_check(&mut self) {
        if self.rx.is_some() {
            return;
        }
        self.candidate = None;
        self.ready = None;
        self.status = "正在检查更新…".into();
        let mirror = self.config.mirror.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(Event::Checked(check(&mirror).map_err(|e| format!("{e:#}"))));
        });
    }
    fn save(&self) -> Result<()> {
        mirror_url(&self.config.mirror, MANIFEST_URL)?;
        std::fs::create_dir_all(self.root.join("data"))?;
        std::fs::write(
            self.root.join("data/update-settings.json"),
            serde_json::to_vec_pretty(&self.config)?,
        )?;
        Ok(())
    }
    fn start_download(&mut self) {
        let Some((manifest, bytes)) = self.candidate.clone() else {
            return;
        };
        let root = self.root.clone();
        let mirror = self.config.mirror.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.status = "正在准备下载…".into();
        std::thread::spawn(move || {
            let result = (|| -> Result<PathBuf> {
                let dir = root
                    .join("data/updates")
                    .join(uuid::Uuid::new_v4().to_string());
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("update.json"), bytes)?;
                download(&manifest, &mirror, &dir.join("new.exe"), |s| {
                    let _ = tx.send(Event::Progress(s));
                })?;
                Ok(dir)
            })();
            let _ = tx.send(Event::Downloaded(result.map_err(|e| format!("{e:#}"))));
        });
    }
    pub fn ui(&mut self, ctx: &egui::Context) -> bool {
        if !self.auto_started {
            self.auto_started = true;
            if self.config.auto_check {
                self.start_check();
            }
        }
        let mut finished = false;
        if let Some(rx) = &self.rx {
            ctx.request_repaint_after(Duration::from_millis(250));
            while let Ok(event) = rx.try_recv() {
                match event {
                    Event::Progress(s) => self.status = s,
                    Event::Checked(result) => {
                        finished = true;
                        match result {
                            Ok((manifest, bytes)) => {
                                if version(&manifest.version).ok()
                                    > version(env!("CARGO_PKG_VERSION")).ok()
                                {
                                    self.status = format!("发现新版本 {}", manifest.version);
                                    self.candidate = Some((manifest, bytes));
                                    self.open = true;
                                } else {
                                    self.status = "当前已是最新版本".into();
                                }
                            }
                            Err(error) => self.status = error,
                        }
                    }
                    Event::Downloaded(result) => {
                        finished = true;
                        match result {
                            Ok(dir) => {
                                self.ready = Some(dir);
                                self.status = "签名和文件校验通过，可以安装更新".into();
                            }
                            Err(error) => self.status = error,
                        }
                    }
                }
            }
        }
        if finished {
            self.rx = None;
        }
        if !self.open {
            return false;
        }
        let mut open = true;
        let mut exit = false;
        egui::Window::new("软件更新").open(&mut open).default_width(560.0).show(ctx,|ui|{
            ui.heading(format!("Flow {}",env!("CARGO_PKG_VERSION")));
            ui.checkbox(&mut self.config.auto_check,"启动时自动检查更新（安装前需确认）");
            ui.label("GitHub 镜像代理前缀（留空直连官方）");
            ui.add(egui::TextEdit::singleline(&mut self.config.mirror).desired_width(500.0).hint_text("https://你的代理域名/"));
            ui.weak("代理需支持 前缀/https://github.com/… 格式及 Releases 文件；配置代理时优先使用，失败回退官方。始终验证内置公钥签名和 SHA-256。");
            ui.horizontal(|ui|{
                if ui.button("保存更新设置").clicked(){self.status=match self.save(){Ok(())=>"更新设置已保存".into(),Err(e)=>e.to_string()};}
                if ui.add_enabled(self.rx.is_none(),egui::Button::new("检查更新")).clicked(){self.start_check();}
                if ui.add_enabled(self.rx.is_none() && self.candidate.is_some() && self.ready.is_none(),egui::Button::new("下载新版")).clicked(){self.start_download();}
            });
            ui.label(&self.status);if self.rx.is_some(){ui.spinner();}
            if let Some(dir)=self.ready.clone(){
                ui.weak("安装会停止当前下载并退出，替换程序后自动重开。保留 data、runtime 和下载文件；旧 EXE 保存在更新备份目录。");
                if ui.button("退出并安装更新").clicked(){match spawn_helper(&dir){Ok(())=>exit=true,Err(e)=>self.status=e.to_string()}}
            }
            ui.hyperlink_to("打开 GitHub 发布页","https://github.com/wrench1997/flow/releases/latest");
        });
        self.open = open;
        exit
    }
}
fn hidden(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
}
fn spawn_helper(dir: &Path) -> Result<()> {
    let helper = dir.join("helper.exe");
    std::fs::copy(std::env::current_exe()?, &helper)?;
    let mut cmd = std::process::Command::new(helper);
    cmd.arg("--apply-update")
        .arg(std::process::id().to_string());
    hidden(&mut cmd);
    cmd.spawn()?;
    Ok(())
}
fn verify_file(path: &Path, manifest: &Manifest) -> Result<()> {
    ensure!(
        std::fs::metadata(path)?.len() == manifest.size,
        "更新文件大小不符"
    );
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == manifest.sha256.to_lowercase(),
        "更新文件校验失败"
    );
    Ok(())
}
pub fn apply_update(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("更新目录无效")?;
    ensure!(
        dir.parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "updates")
            && dir
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|n| n == "data"),
        "更新助手目录无效"
    );
    let root = dir.ancestors().nth(3).context("程序目录无效")?;
    let manifest = verify(&std::fs::read(dir.join("update.json"))?)?;
    ensure!(
        version(&manifest.version)? > version(env!("CARGO_PKG_VERSION"))?,
        "拒绝降级或重复更新"
    );
    verify_file(&dir.join("new.exe"), &manifest)?;
    let pid = args.get(1).context("缺少父进程")?.parse::<u32>()?;
    wait_for_process(pid)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("data/engine.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("仍有 Flow 实例运行，更新取消")?;
    let target = root.join("Flow.exe");
    let backup = dir.join("previous.exe");
    std::fs::rename(&target, &backup).context("无法备份旧版程序")?;
    if let Err(error) = std::fs::rename(dir.join("new.exe"), &target) {
        let _ = std::fs::rename(&backup, &target);
        return Err(error.into());
    }
    drop(lock);
    let mut command = std::process::Command::new(&target);
    command.current_dir(root);
    hidden(&mut command);
    if let Err(error) = command.spawn() {
        let _ = std::fs::rename(&target, dir.join("failed.exe"));
        let _ = std::fs::rename(&backup, &target);
        let _ = command.spawn();
        return Err(error.into());
    }
    std::fs::write(
        root.join("data/update-result.txt"),
        format!(
            "已更新至 {}，旧版备份：{}",
            manifest.version,
            backup.display()
        ),
    )?;
    Ok(())
}
pub fn wait_for_process(pid: u32) -> Result<()> {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn WaitForSingleObject(handle: isize, ms: u32) -> u32;
            fn CloseHandle(handle: isize) -> i32;
            fn GetLastError() -> u32;
        }
        let handle = OpenProcess(0x00100000, 0, pid);
        if handle != 0 {
            let status = WaitForSingleObject(handle, 120000);
            CloseHandle(handle);
            ensure!(status == 0, "等待旧版退出超时，请手动退出后重试");
        } else {
            ensure!(GetLastError() == 87, "无法确认旧版进程已退出");
        }
    }
    Ok(())
}
pub fn report_helper_error(message: &str) {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.ancestors().nth(4) {
            let _ = std::fs::write(
                root.join("data/update-result.txt"),
                format!("更新失败：{message}"),
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    #[test]
    fn signed_manifest_rejects_tampering_and_wrong_key() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let payload = serde_json::to_string(&Manifest {
            version: "0.4.5".into(),
            url: "https://github.com/wrench1997/flow/releases/download/v0.4.5/Flow.exe".into(),
            sha256: "a".repeat(64),
            size: 123,
        })
        .unwrap();
        let sig = key
            .sign(payload.as_bytes())
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let bytes =
            serde_json::to_vec(&serde_json::json!({"payload":payload,"signature":sig})).unwrap();
        assert!(verify_with_key(&bytes, &key.verifying_key().to_bytes()).is_ok());
        assert!(
            verify_with_key(
                &bytes,
                &ed25519_dalek::SigningKey::from_bytes(&[8; 32])
                    .verifying_key()
                    .to_bytes()
            )
            .is_err()
        );
        let changed = String::from_utf8(bytes).unwrap().replace("0.4.5", "0.4.6");
        assert!(verify_with_key(changed.as_bytes(), &key.verifying_key().to_bytes()).is_err());
        assert!(version("0.4.10").unwrap() > version("0.4.9").unwrap());
        assert!(mirror_url("http://proxy.test", MANIFEST_URL).is_err());
    }
}
