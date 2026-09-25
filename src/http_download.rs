//! Direct HTTP transfers with task-owned partial files and validated Range resume.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub name: String,
    pub done: u64,
    pub total: Option<u64>,
    pub complete: bool,
    pub ready_to_finish: bool,
    pub validator: Option<String>,
    pub error: String,
    #[serde(skip)]
    pub rate: u64,
    #[serde(skip)]
    pub running: bool,
}
pub struct Job {
    pub state: Mutex<State>,
    pub part: PathBuf,
    pub destination: PathBuf,
    record: PathBuf,
    source: String,
    worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}
pub fn is_http(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}
pub fn validate_url(source: &str) -> Result<url::Url> {
    let url = url::Url::parse(source)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "请输入 HTTP/HTTPS 直链"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "不支持 URL 中内嵌账号密码"
    );
    anyhow::ensure!(
        !url.path().contains("magnet:"),
        "这是包含磁力文字的网页地址，不是 HTTP 文件直链"
    );
    Ok(url)
}
fn name_for(url: &url::Url, id: &str) -> String {
    let raw = url
        .path_segments()
        .and_then(|mut p| p.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");
    let name: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(150)
        .collect();
    // A task-specific suffix avoids overwriting existing or concurrent downloads.
    let name = name.trim_matches(['.', ' ']);
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => format!("{stem}-{}.{}", &id[..id.len().min(8)], ext),
        None => format!("{stem}-{}", &id[..id.len().min(8)]),
    }
}
impl Job {
    pub fn open(data: &Path, id: &str, source: &str, directory: &Path) -> Result<Arc<Self>> {
        let url = validate_url(source)?;
        std::fs::create_dir_all(directory)?;
        let record = data.join(format!("{id}.http.json"));
        let mut state: State = if record.exists() {
            serde_json::from_slice(&std::fs::read(&record)?)?
        } else {
            State::default()
        };
        // Recompute paths from the trusted task ID and URL; persisted JSON cannot redirect writes/deletes.
        state.name = name_for(&url, id);
        let destination = directory.join(&state.name);
        let part = directory.join(format!(".flow-{id}.part"));
        crate::file_ops::validate_files(directory, &[destination.clone(), part.clone()])?;
        if state.ready_to_finish
            && !part.exists()
            && destination.is_file()
            && std::fs::metadata(&destination)?.len() == state.done
        {
            state.complete = true;
        }
        if state.complete {
            if !destination.is_file() || std::fs::metadata(&destination)?.len() != state.done {
                state.error = "已完成的 HTTP 文件缺失或大小已改变".into();
            }
        } else {
            anyhow::ensure!(
                !destination.exists(),
                "目标文件已存在；为避免覆盖，请更换保存目录"
            );
            state.done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        }
        let job = Arc::new(Self {
            state: Mutex::new(state),
            part,
            destination,
            record,
            source: source.into(),
            worker: tokio::sync::Mutex::new(None),
        });
        job.save()?;
        Ok(job)
    }
    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&*self.state.lock().unwrap())?;
        let temp = self.record.with_extension("tmp");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(temp, &self.record)?;
        Ok(())
    }
    pub async fn start(self: &Arc<Self>, limits: Arc<librqbit::Session>) -> Result<()> {
        let mut worker = self.worker.lock().await;
        if worker.as_ref().is_some_and(|w| !w.is_finished()) || self.state.lock().unwrap().complete
        {
            return Ok(());
        }
        {
            let mut state = self.state.lock().unwrap();
            state.error.clear();
            state.running = true;
        }
        let job = self.clone();
        *worker = Some(tokio::spawn(async move {
            let result = job.transfer(limits).await;
            {
                let mut state = job.state.lock().unwrap();
                state.running = false;
                state.rate = 0;
                if let Err(e) = result {
                    state.error = format!("{e:#}");
                }
            }
            let _ = job.save();
        }));
        Ok(())
    }
    pub async fn stop(&self) -> Result<()> {
        if let Some(worker) = self.worker.lock().await.take() {
            worker.abort();
            let _ = worker.await;
        }
        {
            let mut state = self.state.lock().unwrap();
            state.running = false;
            state.rate = 0;
        }
        self.save()
    }
    pub async fn remove(&self, mode: &str) -> Result<()> {
        self.stop().await?;
        let root = self.destination.parent().context("无保存目录")?;
        let mut files = Vec::new();
        if mode != "keep" {
            files.push(self.part.clone());
        }
        if mode == "all" && self.state.lock().unwrap().complete {
            files.push(self.destination.clone());
        }
        crate::file_ops::remove_files(root, &files)?;
        if self.record.exists() {
            std::fs::remove_file(&self.record)?;
        }
        Ok(())
    }
    pub fn remove_stopped(
        data: &Path,
        id: &str,
        source: &str,
        directory: &Path,
        mode: &str,
    ) -> Result<()> {
        let name = name_for(&validate_url(source)?, id);
        let part = directory.join(format!(".flow-{id}.part"));
        let record = data.join(format!("{id}.http.json"));
        let saved = std::fs::read(&record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok());
        let mut files = Vec::new();
        if mode != "keep" {
            files.push(part.clone());
        }
        if mode == "all"
            && saved.is_some_and(|s| s.complete || (s.ready_to_finish && !part.exists()))
        {
            files.push(directory.join(name));
        }
        crate::file_ops::remove_files(directory, &files)?;
        if record.exists() {
            std::fs::remove_file(record)?;
        }
        Ok(())
    }
    async fn transfer(&self, limits: Arc<librqbit::Session>) -> Result<()> {
        crate::file_ops::validate_files(
            self.destination.parent().unwrap(),
            &[self.part.clone(), self.destination.clone()],
        )?;
        anyhow::ensure!(!self.destination.exists(), "目标已存在，拒绝覆盖");
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        let old = self.state.lock().unwrap().clone();
        let length = std::fs::metadata(&self.part).map(|m| m.len()).unwrap_or(0);
        let mut offset = if old.validator.is_some() { length } else { 0 };
        let mut request = client
            .get(&self.source)
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if offset > 0 {
            request = request
                .header(reqwest::header::RANGE, format!("bytes={offset}-"))
                .header(reqwest::header::IF_RANGE, old.validator.as_ref().unwrap());
        }
        let mut response = request.send().await?;
        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            offset = 0;
            response = client
                .get(&self.source)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send()
                .await?;
        }
        response = response.error_for_status()?;
        let validator = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.starts_with("W/"))
            .or_else(|| {
                response
                    .headers()
                    .get(reqwest::header::LAST_MODIFIED)
                    .and_then(|v| v.to_str().ok())
            })
            .map(str::to_string);
        let total;
        if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            let (start, end, size) = parse_range(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .context("服务器未提供 Content-Range")?,
            )?;
            anyhow::ensure!(
                start == offset && end + 1 == size,
                "服务器返回的续传范围不匹配"
            );
            if offset > 0 {
                anyhow::ensure!(
                    validator == old.validator,
                    "资源校验标识改变，拒绝拼接不同版本；请重新新建任务"
                );
            }
            total = Some(size);
        } else if response.status() == reqwest::StatusCode::OK {
            offset = 0; // Server ignored Range or resource changed: safely restart the partial file.
            total = response.content_length();
        } else {
            bail!("不支持的 HTTP 状态：{}", response.status());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(offset == 0)
            .append(offset > 0)
            .open(&self.part)?;
        {
            let mut s = self.state.lock().unwrap();
            s.done = offset;
            s.total = total;
            s.validator = validator;
        }
        self.save()?;
        let mut sample = Instant::now();
        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await? {
            // Share the configured global download budget with BT transfers.
            for block in chunk.chunks(1024) {
                if let Some(size) = std::num::NonZeroU32::new(block.len() as u32) {
                    limits.ratelimits.prepare_for_download(size).await?;
                }
                file.write_all(block)?;
                {
                    let mut s = self.state.lock().unwrap();
                    s.done += block.len() as u64;
                    if s.total.is_some_and(|n| s.done > n) {
                        bail!("响应超过声明的文件长度");
                    }
                }
                received += block.len() as u64;
            }
            if sample.elapsed() >= Duration::from_millis(500) {
                self.state.lock().unwrap().rate =
                    (received as f64 / sample.elapsed().as_secs_f64()) as u64;
                sample = Instant::now();
                received = 0;
                self.save()?;
            }
        }
        {
            let s = self.state.lock().unwrap();
            anyhow::ensure!(
                s.total.is_none_or(|n| n == s.done),
                "响应提前结束，请继续任务重试"
            );
        }
        file.sync_all()?;
        drop(file);
        self.state.lock().unwrap().ready_to_finish = true;
        self.save()?;
        finish_file(&self.part, &self.destination)?;
        {
            let mut s = self.state.lock().unwrap();
            s.complete = true;
            s.total = Some(s.done);
        }
        self.save()?;
        Ok(())
    }
}
fn finish_file(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileW(from: *const u16, to: *const u16) -> i32;
        }
        let from: Vec<u16> = std::fs::canonicalize(source)?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let target = std::fs::canonicalize(destination.parent().unwrap())?
            .join(destination.file_name().unwrap());
        let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // MoveFileW fails if destination exists, and supports filesystems without hard links.
        if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
            return Err(std::io::Error::last_os_error()).context("无法完成文件，临时文件已保留");
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::hard_link(source, destination)?;
        std::fs::remove_file(source)?;
    }
    Ok(())
}
fn parse_range(value: &str) -> Result<(u64, u64, u64)> {
    let (range, total) = value
        .strip_prefix("bytes ")
        .context("无效 Content-Range")?
        .split_once('/')
        .context("无效范围")?;
    let (start, end) = range.split_once('-').context("无效范围")?;
    let (start, end, total): (u64, u64, u64) = (start.parse()?, end.parse()?, total.parse()?);
    anyhow::ensure!(start <= end && end < total, "无效续传范围");
    Ok((start, end, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    async fn fixture() -> (String, Vec<u8>, Arc<AtomicUsize>, JoinHandle<()>) {
        let payload: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
        let bytes = payload.clone();
        let ranges = Arc::new(AtomicUsize::new(0));
        let counter = ranges.clone();
        let app = Router::new()
            .route(
                "/file.bin",
                get(move |headers: HeaderMap| {
                    let bytes = bytes.clone();
                    let counter = counter.clone();
                    async move {
                        let start = headers
                            .get("range")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.strip_prefix("bytes="))
                            .and_then(|s| s.strip_suffix('-'))
                            .and_then(|s| s.parse::<usize>().ok());
                        if let Some(start) = start {
                            counter.fetch_add(1, Ordering::SeqCst);
                            if start >= bytes.len() {
                                return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                            }
                            (
                                StatusCode::PARTIAL_CONTENT,
                                [
                                    ("etag", "\"fixture\"".to_string()),
                                    (
                                        "content-range",
                                        format!(
                                            "bytes {start}-{}/{}",
                                            bytes.len() - 1,
                                            bytes.len()
                                        ),
                                    ),
                                ],
                                bytes[start..].to_vec(),
                            )
                                .into_response()
                        } else {
                            ([("etag", "\"fixture\"")], bytes).into_response()
                        }
                    }
                }),
            )
            .route(
                "/ignored.bin",
                get(|| async { ([("etag", "\"new\"")], b"replacement body".to_vec()) }),
            )
            .route(
                "/bad.bin",
                get(|| async {
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [("etag", "\"fixture\""), ("content-range", "bytes 3-6/7")],
                        b"oops".to_vec(),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, payload, ranges, server)
    }
    async fn session(path: &Path) -> Arc<librqbit::Session> {
        librqbit::Session::new_with_opts(
            path.to_path_buf(),
            librqbit::SessionOptions {
                dht: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn pause_restart_range_resume_and_delete_modes() {
        let (base, payload, ranges, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("downloads");
        let limits = session(dir.path()).await;
        limits
            .ratelimits
            .set_download_bps(std::num::NonZeroU32::new(16 * 1024));
        let job = Job::open(dir.path(), "test-http", &format!("{base}/file.bin"), &out).unwrap();
        job.start(limits.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while job.state.lock().unwrap().done == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        job.stop().await.unwrap();
        let partial = std::fs::metadata(&job.part).unwrap().len();
        assert!(partial > 0 && partial < payload.len() as u64);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(std::fs::metadata(&job.part).unwrap().len(), partial);
        drop(job);
        let resumed =
            Job::open(dir.path(), "test-http", &format!("{base}/file.bin"), &out).unwrap();
        resumed.start(limits.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while resumed.state.lock().unwrap().running {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            resumed.state.lock().unwrap().complete,
            "{:?}",
            resumed.state.lock().unwrap().error
        );
        assert_eq!(std::fs::read(&resumed.destination).unwrap(), payload);
        assert!(ranges.load(Ordering::SeqCst) > 0);
        let neighbor = out.join("unrelated.txt");
        std::fs::write(&neighbor, b"keep").unwrap();
        resumed.remove("incomplete").await.unwrap();
        assert!(resumed.destination.exists());
        resumed.remove("all").await.unwrap();
        assert!(!resumed.destination.exists());
        assert!(neighbor.exists());
        let kept = Job::open(dir.path(), "kept", &format!("{base}/file.bin"), &out).unwrap();
        std::fs::write(&kept.part, b"partial").unwrap();
        kept.remove("keep").await.unwrap();
        assert!(kept.part.exists());
        let removed = Job::open(dir.path(), "removed", &format!("{base}/file.bin"), &out).unwrap();
        std::fs::write(&removed.part, b"partial").unwrap();
        removed.remove("incomplete").await.unwrap();
        assert!(!removed.part.exists());
        limits.stop().await;
        server.abort();
    }
    #[tokio::test]
    async fn ignored_range_restarts_and_invalid_range_does_not_append() {
        let (base, _, _, server) = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("downloads");
        let limits = session(dir.path()).await;
        for (id, path, ok) in [("ignored", "ignored.bin", true), ("bad", "bad.bin", false)] {
            let job = Job::open(dir.path(), id, &format!("{base}/{path}"), &out).unwrap();
            std::fs::write(&job.part, b"old partial").unwrap();
            job.state.lock().unwrap().validator = Some("\"fixture\"".into());
            job.save().unwrap();
            let result = job.transfer(limits.clone()).await;
            assert_eq!(result.is_ok(), ok);
            if ok {
                assert_eq!(
                    std::fs::read(&job.destination).unwrap(),
                    b"replacement body"
                );
            } else {
                assert_eq!(std::fs::read(&job.part).unwrap(), b"old partial");
            }
        }
        let conflict =
            Job::open(dir.path(), "conflict", &format!("{base}/file.bin"), &out).unwrap();
        std::fs::write(&conflict.destination, b"existing file").unwrap();
        assert!(conflict.transfer(limits.clone()).await.is_err());
        assert_eq!(
            std::fs::read(&conflict.destination).unwrap(),
            b"existing file"
        );
        conflict.remove("all").await.unwrap();
        assert_eq!(
            std::fs::read(&conflict.destination).unwrap(),
            b"existing file"
        );
        limits.stop().await;
        server.abort();
    }
}
