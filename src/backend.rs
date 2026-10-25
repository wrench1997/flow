//! Native, in-process backend lifecycle. No interpreter or subprocess is used.
use std::{path::PathBuf, sync::mpsc, thread, time::Duration};
use tokio::sync::oneshot;

#[derive(Clone, Default)]
pub struct Shutdown {
    current: std::sync::Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
    cancellation: tokio_util::sync::CancellationToken,
}
impl Shutdown {
    pub fn request(&self) {
        self.cancellation.cancel();
        self.stop_current();
    }
    fn stop_current(&self) {
        if let Some(sender) = self.current.lock().unwrap().take() {
            let _ = sender.send(());
        }
    }
    pub fn requested(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    fn register(&self, sender: oneshot::Sender<()>) {
        let mut current = self.current.lock().unwrap();
        if self.requested() {
            let _ = sender.send(());
        } else {
            *current = Some(sender);
        }
    }

    // Cancel an in-flight API request as well as the engine. In particular, a
    // 60-second remove request must not keep the UI worker alive during exit.
    pub fn wait_for<F: std::future::Future>(
        &self,
        runtime: &tokio::runtime::Runtime,
        request: F,
    ) -> Option<F::Output> {
        runtime.block_on(async {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => None,
                result = request => Some(result),
            }
        })
    }
}

pub struct Backend {
    pub base: String,
    pub token: String,
    pub root: PathBuf,
    stop: Shutdown,
    worker: Option<thread::JoinHandle<()>>,
    ready: Option<mpsc::Receiver<Result<String, String>>>,
}

impl Backend {
    pub fn root() -> Result<PathBuf, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let parent = exe.parent().ok_or("无法定位程序目录")?;
        let is_build = parent
            .file_name()
            .and_then(|v| v.to_str())
            .is_some_and(|v| matches!(v, "debug" | "release" | "deps"));
        let root = if is_build {
            exe.ancestors()
                .find(|p| p.file_name().is_some_and(|v| v == "target"))
                .and_then(|p| p.parent())
                .unwrap_or(parent)
        } else {
            parent
        }
        .to_path_buf();
        Ok(root)
    }

    pub fn start() -> Result<Self, String> {
        Self::start_in(Self::root()?, false)
    }

    pub fn start_in(root: PathBuf, offline: bool) -> Result<Self, String> {
        let mut backend = Self::launch_in(root, offline)?;
        let result = backend
            .ready
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(30));
        match result {
            Ok(Ok(base)) => {
                backend.base = base;
                backend.ready = None;
                Ok(backend)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err("Rust 下载引擎启动超时".into()),
        }
    }
    pub fn poll_ready(&mut self) -> Option<Result<(), String>> {
        let ready = self.ready.as_ref()?;
        match ready.try_recv() {
            Ok(Ok(base)) => {
                self.base = base;
                self.ready = None;
                Some(Ok(()))
            }
            Ok(Err(error)) => {
                self.ready = None;
                Some(Err(error))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ready = None;
                Some(Err("引擎初始化线程意外退出，请查看启动日志".into()))
            }
        }
    }
    pub fn launch_in(root: PathBuf, offline: bool) -> Result<Self, String> {
        Self::launch_with_shutdown(root, offline, Shutdown::default())
    }
    pub fn launch_with_shutdown(
        root: PathBuf,
        offline: bool,
        shutdown: Shutdown,
    ) -> Result<Self, String> {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (stop, stopped) = oneshot::channel();
        shutdown.register(stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let backend_root = root.clone();
        let backend_token = token.clone();
        let worker = thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build();
            match rt {
                Ok(rt) => {
                    rt.block_on(async {
                        if let Err(e) = crate::engine::serve(
                            backend_root.clone(),
                            backend_token,
                            offline,
                            stopped,
                            ready_tx.clone(),
                        )
                        .await
                        {
                            crate::engine::startup_stage(
                                &backend_root,
                                &format!("启动失败：{e:#}"),
                            );
                            let _ = ready_tx.send(Err(format!("{e:#}")));
                        }
                    });
                    rt.shutdown_timeout(Duration::from_secs(10));
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                }
            }
        });
        Ok(Self {
            base: String::new(),
            token,
            root,
            stop: shutdown,
            worker: Some(worker),
            ready: Some(ready_rx),
        })
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.stop_current();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_is_immediate_idempotent_and_retry_safe() {
        let shutdown = Shutdown::default();
        let (tx, mut rx) = oneshot::channel();
        shutdown.register(tx);
        shutdown.stop_current();
        assert!(rx.try_recv().is_ok());
        assert!(!shutdown.requested());
        shutdown.request();
        shutdown.request();
        let (tx, mut rx) = oneshot::channel();
        shutdown.register(tx);
        assert!(rx.try_recv().is_ok());
    }
    #[test]
    fn shutdown_cancels_inflight_api_headers_and_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for send_headers in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let shutdown = Shutdown::default();
            let (accepted, received) = mpsc::channel();
            let cancel = shutdown.clone();
            let worker = thread::spawn(move || {
                received.recv_timeout(Duration::from_secs(5)).unwrap();
                cancel.request();
            });
            let started = std::time::Instant::now();
            let result = shutdown.wait_for(&runtime, async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0u8; 4096];
                    socket.read(&mut request).await.unwrap();
                    if send_headers {
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                            .await
                            .unwrap();
                    }
                    accepted.send(()).unwrap();
                    std::future::pending::<()>().await;
                });
                reqwest::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .unwrap()
                    .get(url)
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
            });
            assert!(result.is_none(), "exit waited for the HTTP timeout");
            assert!(started.elapsed() < Duration::from_secs(3));
            worker.join().unwrap();
        }
    }
    #[test]
    fn out_of_band_shutdown_stops_api_and_releases_directory_lock() {
        let root = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::default();
        let mut backend =
            Backend::launch_with_shutdown(root.path().into(), true, shutdown.clone()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while backend.poll_ready().is_none() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!backend.base.is_empty());
        let started = std::time::Instant::now();
        shutdown.request();
        drop(backend);
        assert!(started.elapsed() < Duration::from_secs(3));
        let restarted = Backend::start_in(root.path().into(), true).unwrap();
        drop(restarted);
    }

    #[test]
    fn shutdown_releases_active_http_and_bt_files_before_restart() {
        use std::io::{Read, Write};
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let downloads = root.path().join("downloads");
        std::fs::create_dir_all(&data).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let (stop, stopped) = mpsc::channel();
        let fixture = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 131072\r\nETag: \"fixture\"\r\n\r\n",
                )
                .unwrap();
            socket.write_all(&vec![42; 65536]).unwrap();
            let _ = stopped.recv_timeout(Duration::from_secs(15));
        });
        let mut torrent =
            b"d4:infod6:lengthi16384e4:name7:pending12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend_from_slice(&[0; 20]);
        torrent.extend_from_slice(b"ee");
        std::fs::write(data.join("paused.torrent"), torrent).unwrap();
        let entries = vec![
            crate::engine::Entry {
                id: "http".into(),
                source,
                save_path: downloads.display().to_string(),
                ..Default::default()
            },
            crate::engine::Entry {
                id: "paused".into(),
                source: data.join("paused.torrent").display().to_string(),
                save_path: downloads.display().to_string(),
                paused: true,
                ..Default::default()
            },
        ];
        std::fs::write(
            data.join("tasks-rust.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let backend = Backend::start_in(root.path().into(), true).unwrap();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let state: serde_json::Value = client
                .get(format!("{}/api/state", backend.base))
                .bearer_auth(&backend.token)
                .send()
                .unwrap()
                .json()
                .unwrap();
            let tasks = state["tasks"].as_array().unwrap();
            if tasks
                .iter()
                .any(|t| t["id"] == "http" && t["done"].as_u64().unwrap() > 0)
                && tasks
                    .iter()
                    .any(|t| t["id"] == "paused" && t["state"] == "paused")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "downloads did not start: {state}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let files = [downloads.join(".flow-http.part"), downloads.join("pending")];
        #[cfg(windows)]
        for file in &files {
            use std::os::windows::fs::OpenOptionsExt;
            assert!(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode(0)
                    .open(file)
                    .is_err()
            );
        }
        let started = std::time::Instant::now();
        drop(backend);
        assert!(started.elapsed() < Duration::from_secs(5));
        for file in files {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(0);
            }
            options
                .open(file)
                .expect("download file still in use after shutdown");
        }
        let saved: crate::http_download::State =
            serde_json::from_slice(&std::fs::read(data.join("http.http.json")).unwrap()).unwrap();
        assert!(saved.done > 0 && saved.done < 131072);
        assert!(saved.error.is_empty());
        drop(Backend::start_in(root.path().into(), true).unwrap());
        let _ = stop.send(());
        fixture.join().unwrap();
    }

    #[test]
    fn corrupt_settings_report_error_and_retry_preserves_data() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("data")).unwrap();
        let settings = temp.path().join("data/settings.json");
        std::fs::write(&settings, b"broken configuration").unwrap();
        assert!(Backend::start_in(temp.path().into(), true).is_err());
        assert_eq!(std::fs::read(&settings).unwrap(), b"broken configuration");
        std::fs::write(
            &settings,
            serde_json::to_vec(&crate::settings::Settings::defaults(temp.path())).unwrap(),
        )
        .unwrap();
        let backend = Backend::start_in(temp.path().into(), true).unwrap();
        drop(backend);
    }
    #[test]
    fn stale_derived_index_does_not_block_startup_and_is_backed_up() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(data.join("rqbit")).unwrap();
        std::fs::write(data.join("tasks-rust.json"), b"[]").unwrap();
        std::fs::write(data.join("rqbit/session.json"), b"stale engine index").unwrap();
        let bitmap = data.join("rqbit/fixture.bitv");
        std::fs::write(&bitmap, b"preserve resume state").unwrap();
        let backend = Backend::start_in(temp.path().into(), true).unwrap();
        assert!(
            std::fs::read_dir(data.join("rqbit"))
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("session-startup-")
                    && std::fs::read(e.path()).unwrap() == b"stale engine index")
        );
        assert_eq!(std::fs::read(bitmap).unwrap(), b"preserve resume state");
        drop(backend);
    }
    #[test]
    fn cancel_initialization_releases_lock_for_next_launch() {
        let temp = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        drop(Backend::launch_in(temp.path().into(), true).unwrap());
        assert!(started.elapsed() < Duration::from_secs(10));
        let backend = Backend::start_in(temp.path().into(), true).unwrap();
        drop(backend);
    }
    #[test]
    fn starts_authenticates_rejects_duplicate_and_stops() {
        let temp = tempfile::tempdir().unwrap();
        let backend = Backend::start_in(temp.path().into(), true).unwrap();
        let url = format!("{}/api/state", backend.base);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        assert_eq!(client.get(&url).send().unwrap().status(), 401);
        let health = format!("{}/api/health", backend.base);
        assert_eq!(client.get(&health).send().unwrap().status(), 401);
        assert!(
            client
                .get(&health)
                .bearer_auth(&backend.token)
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        let snapshot: serde_json::Value = client
            .get(&url)
            .bearer_auth(&backend.token)
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(snapshot["engine"], "librqbit 9.0.1 · Rust");
        assert!(Backend::start_in(temp.path().into(), true).is_err());
        let settings_url = format!("{}/api/settings", backend.base);
        let mut settings = crate::settings::Settings::defaults(temp.path());
        settings.download_dir = temp.path().join("custom").display().to_string();
        settings.upload_kib = 64;
        settings.download_kib = 128;
        assert!(
            client
                .post(&settings_url)
                .bearer_auth(&backend.token)
                .json(&settings)
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        assert!(std::path::Path::new(&settings.download_dir).is_dir());
        let updated: serde_json::Value = client
            .get(&url)
            .bearer_auth(&backend.token)
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(updated["settings"]["upload_kib"], 64);
        settings.peer_limit = 0;
        assert_eq!(
            client
                .post(&settings_url)
                .bearer_auth(&backend.token)
                .json(&settings)
                .send()
                .unwrap()
                .status(),
            400
        );
        drop(backend);
        assert!(client.get(&url).send().is_err());
        let restarted = Backend::start_in(temp.path().into(), true).unwrap();
        let saved: serde_json::Value = client
            .get(format!("{}/api/state", restarted.base))
            .bearer_auth(&restarted.token)
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(saved["settings"]["upload_kib"], 64);
        assert_eq!(saved["settings"]["peer_limit"], 150);
    }
}
