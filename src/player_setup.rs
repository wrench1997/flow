//! Install the pinned player runtime without PowerShell or a separate setup script.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

const URL: &str = "https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20260925/mpv-x86_64-20260925-git-2a4eb8067c.7z";
const HASH: &str = "aef0320478257259c7365087b6d3c87c91c8731b1f3a4e52f1d86d36bac9c998";
pub enum Event {
    Progress(String),
    Finished(Result<(), String>),
}

pub fn start(root: std::path::PathBuf) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = install(&root, |message| {
            let _ = tx.send(Event::Progress(message));
        });
        let _ = tx.send(Event::Finished(result.map_err(|e| format!("{e:#}"))));
    });
    rx
}

fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    hash: &str,
    path: &Path,
    progress: &impl Fn(String),
) -> Result<()> {
    let mut response = client.get(url).send()?.error_for_status()?;
    let total = response.content_length();
    let mut file = std::fs::File::create(path)?;
    let mut digest = Sha256::new();
    let mut received = 0u64;
    let mut buffer = [0u8; 65536];
    let mut last = std::time::Instant::now();
    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        received += n as u64;
        ensure!(received <= 256 * 1024 * 1024, "播放器安装包超过大小限制");
        file.write_all(&buffer[..n])?;
        digest.update(&buffer[..n]);
        if last.elapsed() >= Duration::from_millis(250) {
            progress(format!(
                "正在下载播放内核：{:.1} MiB{}",
                received as f64 / 1048576.0,
                total
                    .map(|n| format!(" / {:.1} MiB", n as f64 / 1048576.0))
                    .unwrap_or_default()
            ));
            last = std::time::Instant::now();
        }
    }
    file.sync_all()?;
    ensure!(
        format!("{:x}", digest.finalize()) == hash,
        "播放器安装包校验失败，请重试"
    );
    Ok(())
}

fn install(root: &Path, progress: impl Fn(String)) -> Result<()> {
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime).context("无法创建播放器目录，请检查目录权限")?;
    // Serialize setup across processes; never replace a runtime used by another installer.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(runtime.join("mpv-install.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("另一窗口正在安装播放器，请稍后重试")?;
    let destination = runtime.join("mpv");
    if destination.join("mpv.exe").is_file() {
        return Ok(());
    }
    let staging = runtime.join(format!(".mpv-install-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&staging)?;
    // Only this newly-created, uniquely named directory is removed on failure.
    let result = (|| -> Result<()> {
        progress("正在连接播放器下载源…".into());
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .user_agent("Flow-player-setup")
            .build()?;
        let archive = staging.join("mpv.7z");
        download(&client, URL, HASH, &archive, &progress)?;
        progress("校验通过，正在配置播放器…".into());
        let unpacked = staging.join("unpacked");
        std::fs::create_dir(&unpacked)?;
        // Use the Windows-supplied archive utility, not a PowerShell script.
        let tar = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .context("找不到 Windows 系统目录")?
            .join("System32/tar.exe");
        let mut command = Command::new(tar);
        command
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .context("无法启动系统解压工具（需要 Windows 10/11）")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "播放器解压失败，请重试");
                break;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("播放器解压超时，请重试");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        ensure!(unpacked.join("mpv.exe").is_file(), "安装包中未找到播放内核");
        let backup = runtime.join(format!("mpv-backup-{}", uuid::Uuid::new_v4()));
        let had_existing = destination.exists();
        if had_existing {
            std::fs::rename(&destination, &backup)
                .context("无法替换旧播放器目录，请关闭视频窗口后重试")?;
        }
        if let Err(error) = std::fs::rename(&unpacked, &destination) {
            if had_existing {
                let _ = std::fs::rename(&backup, &destination);
            }
            return Err(error.into());
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_corrupt_download_before_installation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        assert!(
            download(&client, &url, HASH, &dir.path().join("bad.7z"), &|_| {})
                .unwrap_err()
                .to_string()
                .contains("校验失败")
        );
        assert!(!dir.path().join("mpv.exe").exists());
        server.join().unwrap();
    }
    #[test]
    #[ignore = "downloads and installs the pinned player runtime"]
    fn real_install_into_isolated_directory() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), |message| eprintln!("{message}")).unwrap();
        assert!(dir.path().join("runtime/mpv/mpv.exe").is_file());
        install(dir.path(), |_| {
            panic!("existing install must not download again")
        })
        .unwrap();
    }
}
