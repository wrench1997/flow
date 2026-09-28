//! Install the pinned player runtime without PowerShell or a separate setup script.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
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

pub fn install(root: &Path, progress: impl Fn(String)) -> Result<()> {
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
        extract_archive(&archive, &unpacked)?;
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

/// Extract the verified 7z payload in-process. Windows 10's tar.exe cannot
/// consistently read 7z archives, and its previous stderr was discarded.
fn extract_archive(archive: &Path, destination: &Path) -> Result<()> {
    let mut extracted_bytes = 0u64;
    sevenz_rust2::decompress_file_with_extract_fn(archive, destination, |entry, reader, path| {
        extracted_bytes = extracted_bytes.saturating_add(entry.size());
        if extracted_bytes > 1024 * 1024 * 1024 {
            return Err(sevenz_rust2::Error::Other("播放器解压内容超过 1 GiB 限制".into()));
        }
        sevenz_rust2::default_entry_extract_fn(entry, reader, path)
    })
    .with_context(|| format!("播放器 7z 解压失败：{}", archive.display()))?;
    ensure!(destination.join("mpv.exe").is_file(), "播放器压缩包缺少 mpv.exe");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn in_process_extractor_rejects_invalid_archive_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.7z");
        std::fs::write(&archive, b"not a seven zip archive").unwrap();
        let error = extract_archive(&archive, &dir.path().join("out")).unwrap_err();
        assert!(format!("{error:#}").contains("7z 解压失败"));
    }
    #[test]
    #[ignore = "requires FLOW_TEST_MPV_ARCHIVE pointing at the pinned 7z file"]
    fn real_archive_extracts_without_windows_tar() {
        let archive = std::env::var_os("FLOW_TEST_MPV_ARCHIVE").expect("set FLOW_TEST_MPV_ARCHIVE");
        let dir = tempfile::tempdir().unwrap();
        extract_archive(Path::new(&archive), dir.path()).unwrap();
        assert!(dir.path().join("mpv.exe").metadata().unwrap().len()>100_000_000);
    }
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
