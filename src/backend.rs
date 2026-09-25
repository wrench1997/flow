//! Native, in-process backend lifecycle. No interpreter or subprocess is used.
use std::{path::PathBuf, sync::mpsc, thread, time::Duration};
use tokio::sync::oneshot;

pub struct Backend {
    pub base: String,
    pub token: String,
    pub root: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Backend {
    pub fn start() -> Result<Self, String> {
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
        Self::start_in(root, false)
    }

    pub fn start_in(root: PathBuf, offline: bool) -> Result<Self, String> {
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
                            backend_root,
                            backend_token,
                            offline,
                            stopped,
                            ready_tx.clone(),
                        )
                        .await
                        {
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
        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(base)) => Ok(Self {
                base,
                token,
                root,
                stop: Some(stop),
                worker: Some(worker),
            }),
            other => {
                let _ = stop.send(());
                let error = match other {
                    Ok(Err(e)) => e,
                    _ => "Rust 下载引擎启动超时".into(),
                };
                Err(error)
            }
        }
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
