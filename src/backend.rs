//! Native, in-process backend lifecycle. No interpreter or subprocess is used.
use std::{path::PathBuf, sync::mpsc, thread, time::Duration};
use tokio::sync::oneshot;

pub struct Backend {
    pub base: String,
    pub token: String,
    pub root: PathBuf,
    stop: Option<oneshot::Sender<()>>,
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
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (stop, stopped) = oneshot::channel();
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
            stop: Some(stop),
            worker: Some(worker),
            ready: Some(ready_rx),
        })
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
