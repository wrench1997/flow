//! Native task management and a loopback API shared with the egui front end.
use crate::trackers::{self, Metric};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig, api::TorrentIdOrHash,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Entry {
    pub only_files: Option<Vec<usize>>,
    pub deletion_files: Option<Vec<PathBuf>>,
    pub id: String,
    pub source: String,
    pub save_path: String,
    pub paused: bool,
    pub trackers: Vec<String>,
    pub error: String,
    pub rust_output: Option<String>,
    pub metrics: BTreeMap<String, Metric>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub initial_peers: Vec<std::net::SocketAddr>,
    pub peer_cache_updated: u64,
}

pub struct Engine {
    http_jobs: Mutex<BTreeMap<String, Arc<crate::http_download::Job>>>,
    settings: Mutex<crate::settings::Settings>,
    subscriptions: crate::subscriptions::Catalog,
    probe_slots: Arc<tokio::sync::Semaphore>,
    pub session: Arc<Session>,
    pub data: PathBuf,
    entries: Mutex<Vec<Entry>>,
    handles: Mutex<BTreeMap<String, Arc<ManagedTorrent>>>,
    events: Mutex<VecDeque<Value>>,
    discovery: Mutex<BTreeMap<String, Value>>,
    operations: tokio::sync::Mutex<()>,
    loading: Mutex<BTreeMap<String, Value>>,
    completed_cache: Mutex<BTreeMap<String, Value>>,
    peer_quality: Mutex<BTreeMap<String, BTreeMap<String, crate::peer_quality::Window>>>,
    peer_records: Mutex<BTreeMap<String, Value>>,
    active_bans: std::collections::BTreeSet<String>,
    recovery: Mutex<BTreeMap<String, crate::peer_quality::Recovery>>,
    pub offline: bool,
}

fn atomic(path: &Path, data: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = File::create(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// libtorrent stores a multi-file torrent below its name; rqbit's explicit
/// output_folder instead names that subdirectory itself. Preserve that mapping.
fn metadata_size(bytes: &[u8]) -> Result<(usize, u64)> {
    use serde_bencode::value::Value as B;
    let B::Dict(root) = serde_bencode::from_bytes::<B>(bytes)? else {
        bail!("无效种子");
    };
    let Some(B::Dict(info)) = root.get(b"info".as_slice()) else {
        bail!("缺少 info");
    };
    let length = |value: Option<&B>| match value {
        Some(B::Int(n)) if *n >= 0 => *n as u64,
        _ => 0,
    };
    if let Some(B::List(files)) = info.get(b"files".as_slice()) {
        let total = files
            .iter()
            .map(|file| match file {
                B::Dict(file) => length(file.get(b"length".as_slice())),
                _ => 0,
            })
            .fold(0u64, u64::saturating_add);
        Ok((files.len(), total))
    } else {
        Ok((1, length(info.get(b"length".as_slice()))))
    }
}

fn output_path(bytes: &[u8], base: &Path) -> Result<PathBuf> {
    use serde_bencode::value::Value as B;
    let B::Dict(root) = serde_bencode::from_bytes::<B>(bytes)? else {
        bail!("种子格式无效");
    };
    let Some(B::Dict(info)) = root.get(b"info".as_slice()) else {
        bail!("种子没有 info 字段");
    };
    if !info.contains_key(b"files".as_slice()) {
        return Ok(base.to_path_buf());
    }
    let Some(B::Bytes(name)) = info
        .get(b"name.utf-8".as_slice())
        .or_else(|| info.get(b"name".as_slice()))
    else {
        bail!("种子没有目录名");
    };
    let name = String::from_utf8(name.clone()).context("种子目录名不是 UTF-8，请先转换种子编码")?;
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', ':']) {
        bail!("种子目录名不安全");
    }
    Ok(base.join(name))
}

// Preserve the exact info dictionary: re-encoding it could change the info hash.
// rqbit restores trackers from the persisted torrent bytes on restart.
fn metadata_with_trackers(bytes: &[u8], extra: &[String]) -> Result<Vec<u8>> {
    if extra.is_empty() {
        return Ok(bytes.to_vec());
    }
    use serde_bencode::value::Value as B;
    let parsed = librqbit::torrent_from_bytes(bytes)?;
    let mut urls: Vec<String> = parsed
        .iter_announce()
        .map(|s| String::from_utf8_lossy(s.as_ref()).into_owned())
        .collect();
    urls.extend_from_slice(extra);
    urls.sort();
    urls.dedup();
    let B::Dict(root) = serde_bencode::from_bytes::<B>(bytes)? else {
        bail!("种子格式无效");
    };
    let mut root: BTreeMap<Vec<u8>, B> = root.into_iter().collect();
    root.insert(
        b"announce-list".to_vec(),
        B::List(
            urls.into_iter()
                .map(|u| B::List(vec![B::Bytes(u.into_bytes())]))
                .collect(),
        ),
    );
    let mut result = vec![b'd'];
    for (key, value) in root {
        result.extend(serde_bencode::to_bytes(&B::Bytes(key.clone()))?);
        if key == b"info" {
            result.extend_from_slice(parsed.info.raw_bytes.as_ref());
        } else {
            result.extend(serde_bencode::to_bytes(&value)?);
        }
    }
    result.push(b'e');
    anyhow::ensure!(
        librqbit::torrent_from_bytes(&result)?.info_hash == parsed.info_hash,
        "Tracker 更新改变了资源哈希"
    );
    Ok(result)
}

impl Engine {
    pub async fn new(root: &Path, offline: bool) -> Result<Arc<Self>> {
        let data = root.join("data");
        std::fs::create_dir_all(&data)?;
        startup_stage(root, "读取配置和任务目录");
        let settings_file = data.join("settings.json");
        let settings: crate::settings::Settings = if settings_file.exists() {
            serde_json::from_slice(&std::fs::read(&settings_file)?)?
        } else {
            crate::settings::Settings::defaults(root)
        };
        settings.validate()?;
        atomic(&settings_file, &serde_json::to_vec_pretty(&settings)?)?;
        let catalog = data.join("tasks-rust.json");
        let old = data.join("tasks.json");
        let entries: Vec<Entry> = if catalog.exists() {
            serde_json::from_slice(&std::fs::read(&catalog)?)?
        } else if old.exists() {
            serde_json::from_slice(&std::fs::read(&old)?)?
        } else {
            Vec::new()
        };
        // Flow owns task restoration. Preserve librqbit fast-resume bitmaps,
        // but do not let Session::new restore the same tasks before API startup.
        if catalog.exists() {
            let session_file = data.join("rqbit/session.json");
            if session_file.exists() {
                startup_stage(root, "准备续传状态（备份引擎任务索引）");
                let previous = std::fs::read(&session_file)?;
                // A backup is created before rebuilding the derived index.
                let backup = data.join(format!(
                    "rqbit/session-startup-{}.json",
                    uuid::Uuid::new_v4().simple()
                ));
                atomic(&backup, &previous)?;
                atomic(&session_file, br#"{"torrents":{}}"#)?;
            }
        }
        let mut opts = SessionOptions {
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(data.join("rqbit")),
            }),
            disable_local_service_discovery: true,
            peer_limit: Some(settings.peer_limit as usize),
            ipv4_only: true,
            listen: Some(librqbit::ListenerOptions {
                ipv4_only: true,
                ..Default::default()
            }),
            connect: Some(librqbit::ConnectionOptions::default()),
            ratelimits: librqbit::limits::LimitsConfig {
                upload_bps: std::num::NonZeroU32::new(settings.upload_kib * 1024),
                download_bps: std::num::NonZeroU32::new(settings.download_kib * 1024),
            },
            ..Default::default()
        };
        if offline {
            opts.dht = None;
            // Local fixtures may use a loopback tracker; disable public discovery and DHT.
        } else if let Some(dht) = opts.dht.as_mut() {
            let persistence = dht.persistence.get_or_insert_with(Default::default);
            persistence.config_filename = Some(data.join("dht.json"));
            persistence.dump_interval = Some(Duration::from_secs(30));
        }
        if let Some(listener) = opts.listen.as_mut() {
            listener.ipv4_only = true;
        }
        let records_path = data.join("peer-records.json");
        let peer_records: BTreeMap<String, Value> = if records_path.exists() {
            serde_json::from_slice(&std::fs::read(&records_path)?).context("节点记录文件损坏")?
        } else {
            BTreeMap::new()
        };
        let active_bans: std::collections::BTreeSet<String> = peer_records
            .iter()
            .filter(|(ip, record)| {
                record["blocked"] == true && ip.parse::<std::net::IpAddr>().is_ok()
            })
            .map(|(ip, _)| ip.clone())
            .collect();
        let blocklist = data.join("peer-blocklist.txt");
        std::fs::write(
            &blocklist,
            format!(
                "# Flow local IP blacklist\n{}",
                active_bans
                    .iter()
                    .map(|ip| format!("Flow:{ip}-{ip}\n"))
                    .collect::<String>()
            ),
        )?;
        opts.blocklist_url = Some(
            url::Url::from_file_path(&blocklist)
                .map_err(|_| anyhow::anyhow!("黑名单路径无效"))?
                .to_string(),
        );
        startup_stage(root, "初始化网络和下载会话");
        let session = Session::new_with_opts(root.join("downloads"), opts).await?;
        let engine = Arc::new(Self {
            http_jobs: Mutex::new(BTreeMap::new()),
            settings: Mutex::new(settings),
            subscriptions: crate::subscriptions::Catalog::open(&data)?,
            probe_slots: Arc::new(tokio::sync::Semaphore::new(12)),
            session,
            data,
            entries: Mutex::new(entries),
            handles: Mutex::new(BTreeMap::new()),
            events: Mutex::new(VecDeque::new()),
            discovery: Mutex::new(BTreeMap::new()),
            operations: tokio::sync::Mutex::new(()),
            loading: Mutex::new(BTreeMap::new()),
            completed_cache: Mutex::new(BTreeMap::new()),
            peer_quality: Mutex::new(BTreeMap::new()),
            peer_records: Mutex::new(peer_records),
            active_bans,
            recovery: Mutex::new(BTreeMap::new()),
            offline,
        });
        engine.persist()?;
        startup_stage(root, "下载会话已就绪");
        Ok(engine)
    }

    pub fn event(&self, id: &str, level: &str, kind: &str, message: &str) {
        let timestamp = trackers::now();
        let e = json!({"time":format!("{:02}:{:02}:{:02}",(timestamp/3600+8)%24,(timestamp/60)%60,timestamp%60),"task":id,"level":level,"kind":kind,"message":message});
        let mut events = self.events.lock().unwrap();
        if events.len() >= 300 {
            events.pop_front();
        }
        events.push_back(e.clone());
        let file = self.data.join("events-rust.jsonl");
        if std::fs::metadata(&file).is_ok_and(|m| m.len() > 2_000_000) {
            let _ = std::fs::rename(&file, self.data.join("events-rust.previous.jsonl"));
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{e}");
        }
    }
    fn persist(&self) -> Result<()> {
        atomic(
            &self.data.join("tasks-rust.json"),
            &serde_json::to_vec_pretty(&*self.entries.lock().unwrap())?,
        )
    }
    fn entry(&self, id: &str) -> Result<Entry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .context("任务不存在")
    }
    fn handle(&self, id: &str) -> Result<Arc<ManagedTorrent>> {
        self.handles
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("任务仍在加载或加载失败")
    }
    fn error(&self, id: &str, error: &str) {
        if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
            e.error = error.into();
        }
        self.event(id, "error", "engine", error);
        let _ = self.persist();
    }

    pub async fn restore(self: &Arc<Self>) {
        let mut entries = self.entries.lock().unwrap().clone();
        // Opening thousands of files is synchronous inside librqbit. Restore
        // cached, small-file-count tasks first; unresolved magnets go last.
        let mut costs = BTreeMap::new();
        for entry in &entries {
            if entry.paused
                && entry.error.is_empty()
                && entry.deletion_files.is_none()
                && entry.only_files.is_none()
            {
                if let Ok(cached) = self.cached_complete(entry) {
                    self.completed_cache
                        .lock()
                        .unwrap()
                        .insert(entry.id.clone(), cached);
                }
            }
            let summary = std::fs::read(self.data.join(format!("{}.torrent", entry.id)))
                .ok()
                .and_then(|bytes| metadata_size(&bytes).ok());
            costs.insert(entry.id.clone(), summary.map(|v| v.0).unwrap_or(usize::MAX));
            self.loading.lock().unwrap().insert(
                entry.id.clone(),
                json!({
                    "stage":"排队恢复（小任务优先）", "started":trackers::now(),
                    "files":summary.map(|v| v.0), "total":summary.map(|v| v.1)
                }),
            );
        }
        entries.sort_by_key(|entry| {
            if crate::http_download::is_http(&entry.source) {
                0
            } else {
                costs[&entry.id]
            }
        });
        let mut resolutions = tokio::task::JoinSet::new();
        for entry in entries {
            if self.completed_cache.lock().unwrap().contains_key(&entry.id) {
                self.loading.lock().unwrap().remove(&entry.id);
                continue;
            }
            if entry.source.starts_with("magnet:?")
                && !self.data.join(format!("{}.torrent", entry.id)).exists()
            {
                if resolutions.len() >= 2 {
                    let _ = resolutions.join_next().await;
                }
                let engine = self.clone();
                resolutions.spawn(async move {
                    if let Err(error) = engine.load(&entry.id).await {
                        engine.error(&entry.id, &format!("{error:#}"));
                    }
                    engine.loading.lock().unwrap().remove(&entry.id);
                });
                continue;
            }
            if let Err(e) = self.load(&entry.id).await {
                self.error(&entry.id, &format!("{e:#}"));
            }
            self.loading.lock().unwrap().remove(&entry.id);
        }
        while resolutions.join_next().await.is_some() {}
    }

    async fn load(self: &Arc<Self>, id: &str) -> Result<()> {
        self.completed_cache.lock().unwrap().remove(id);
        let mut operation = Some(self.operations.lock().await);
        let entry = self.entry(id)?;
        if self.handles.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        if entry.deletion_files.is_some() {
            bail!("此任务有尚未完成的删除操作，请重新删除任务");
        }
        if crate::http_download::is_http(&entry.source) {
            let job = crate::http_download::Job::open(
                &self.data,
                id,
                &entry.source,
                Path::new(&entry.save_path),
            )?;
            self.http_jobs
                .lock()
                .unwrap()
                .insert(id.into(), job.clone());
            if !entry.paused {
                job.start(self.session.clone()).await?;
            }
            if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                e.error.clear();
            }
            self.persist()?;
            return Ok(());
        }
        let metadata_file = self.data.join(format!("{id}.torrent"));
        let mut seen_peers = entry.initial_peers.clone();
        if entry.peer_cache_updated > 0
            && trackers::now().saturating_sub(entry.peer_cache_updated) > 7 * 86400
        {
            seen_peers.clear();
        }
        seen_peers.truncate(64);
        let bytes = if metadata_file.exists() {
            std::fs::read(&metadata_file)?
        } else if entry.source.starts_with("magnet:?") {
            // Resolving metadata must not block pause/delete/settings for other tasks.
            drop(operation.take());
            self.event(id, "info", "metadata", "正在解析磁力链接元数据");
            let mut magnet = url::Url::parse(&entry.source)?;
            for tr in &entry.trackers {
                magnet.query_pairs_mut().append_pair("tr", tr);
            }
            let mut resolved = None;
            for attempt in 1..=2 {
                // Tracker-less magnets can use subscribed public sources before
                // metadata resolution. Do not augment explicit/private tracker sets.
                if !self.offline && !magnet.query_pairs().any(|(k, _)| k == "tr") {
                    self.event(
                        id,
                        "info",
                        "metadata_sources",
                        "补充订阅中的公开 Tracker 辅助磁力解析",
                    );
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(5))
                        .build()?;
                    if let Ok((sources, _)) = tokio::time::timeout(
                        Duration::from_secs(15),
                        self.subscriptions.candidates(&client),
                    )
                    .await
                    {
                        for tracker in sources.keys().take(40) {
                            magnet.query_pairs_mut().append_pair("tr", tracker);
                        }
                    }
                }
                self.loading.lock().unwrap().insert(id.into(),json!({"stage":format!("获取元数据，第 {attempt}/2 轮；DHT {}，候选 Tracker {}，缓存节点 {}",if self.session.get_dht().is_some(){"已启用"}else{"关闭"},magnet.query_pairs().filter(|(k,_)| k=="tr").count(),seen_peers.len()),"started":trackers::now()}));
                let resolution = tokio::time::timeout(
                    Duration::from_secs(90),
                    self.session.add_torrent(
                        AddTorrent::from_url(magnet.to_string()),
                        Some(AddTorrentOptions {
                            list_only: true,
                            initial_peers: Some(seen_peers.clone()),
                            ..Default::default()
                        }),
                    ),
                );
                let response = tokio::select! {
                    result = resolution => result,
                    _ = async { loop { tokio::time::sleep(Duration::from_millis(200)).await; if self.entry(id).map_or(true, |e| !entry.paused && e.paused) { break; } } } => {self.loading.lock().unwrap().remove(id);return Ok(());},
                };
                match response {
                    Ok(Ok(value)) => {
                        resolved = Some(value);
                        break;
                    }
                    error => {
                        let reason = match error {
                            Ok(Err(e)) => format!("{e:#}"),
                            Err(_) => "90 秒内未收到完整元数据".into(),
                            _ => unreachable!(),
                        };
                        self.event(
                            id,
                            "warning",
                            "metadata_retry",
                            &format!("第 {attempt} 轮失败：{reason}"),
                        );
                    }
                }
            }
            match resolved.context("元数据获取失败：两轮找源仍未收到完整元数据。请查看诊断日志，重试或导入 .torrent；Tracker 有做种统计不代表节点可连接。")? {
                AddTorrentResponse::ListOnly(r) => {
                    seen_peers = r.seen_peers;
                    seen_peers.truncate(64);
                    r.torrent_bytes.to_vec()
                }
                _ => bail!("这个磁力任务已存在"),
            }
        } else {
            std::fs::read(&entry.source)?
        };
        if operation.is_none() {
            operation = Some(self.operations.lock().await);
        }
        let _operation = operation;
        let entry = self.entry(id)?;
        let bytes = metadata_with_trackers(&bytes, &entry.trackers)?;
        let (file_count, total) = metadata_size(&bytes)?;
        self.loading.lock().unwrap().insert(id.into(), json!({"stage":"正在打开本地文件", "started":trackers::now(), "files":file_count, "total":total}));
        self.event(
            id,
            "info",
            "restore_files",
            &format!("正在打开 {file_count} 个本地文件；完成后加载续传数据"),
        );
        let output = entry
            .rust_output
            .as_ref()
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| output_path(&bytes, Path::new(&entry.save_path)))?;
        atomic(&metadata_file, &bytes)?;
        let opts = AddTorrentOptions {
            only_files: entry.only_files.clone(),
            paused: true,
            overwrite: true,
            output_folder: Some(output.display().to_string()),
            trackers: Some(entry.trackers.clone()),
            initial_peers: Some(seen_peers.clone()),
            ..Default::default()
        };
        let response = self
            .session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
            .await?;
        let handle = response.into_handle().context("引擎未返回下载任务")?;
        // Pause requests made while file opening was in progress must win.
        let entry = self.entry(id)?;
        if self
            .handles
            .lock()
            .unwrap()
            .iter()
            .any(|(other, h)| other != id && h.info_hash() == handle.info_hash())
        {
            bail!("相同资源已经存在");
        }
        if handle.is_paused() != entry.paused {
            if entry.paused {
                self.session.pause(&handle).await?;
            } else {
                self.session.unpause(&handle).await?;
            }
        }
        let mut trackers = handle
            .shared()
            .trackers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        trackers.extend(entry.trackers);
        trackers.sort();
        trackers.dedup();
        if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
            e.rust_output = Some(output.display().to_string());
            e.error.clear();
            e.trackers = trackers;
            if !handle
                .with_metadata(|m| m.info.info().private)
                .unwrap_or(true)
            {
                e.initial_peers = seen_peers.clone();
                if !e.initial_peers.is_empty() {
                    e.peer_cache_updated = trackers::now();
                }
            } else {
                e.initial_peers.clear();
                e.peer_cache_updated = 0;
            }
        }
        self.handles
            .lock()
            .unwrap()
            .insert(id.into(), handle.clone());
        // Initialization captures its original start_paused value in librqbit.
        // A resume during checking can otherwise finish in Paused with a false
        // pause flag. Reconcile after transitions without holding the API lock
        // while waiting for hashing/network work.
        let weak = Arc::downgrade(self);
        let task_id = id.to_owned();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(engine) = weak.upgrade() else {
                    break;
                };
                let _operation = engine.operations.lock().await;
                let Ok(current) = engine.handle(&task_id) else {
                    break;
                };
                if !Arc::ptr_eq(&current, &handle) {
                    break;
                }
                let Ok(entry) = engine.entry(&task_id) else {
                    break;
                };
                let stats = handle.stats();
                let state = stats.state;
                if stats.finished && !engine.settings.lock().unwrap().seed_after_download {
                    let result = async {
                        if matches!(state, librqbit::TorrentStatsState::Live) {
                            let _ = engine.remember_working_peers();
                            engine.session.pause(&handle).await?;
                        }
                        if !entry.paused {
                            if let Some(e) = engine
                                .entries
                                .lock()
                                .unwrap()
                                .iter_mut()
                                .find(|e| e.id == task_id)
                            {
                                e.paused = true;
                            }
                            engine.persist()?;
                            engine.event(&task_id, "info", "complete", "下载完成，已自动停止做种");
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        engine.error(&task_id, &format!("停止做种失败：{error:#}"));
                        break;
                    }
                    continue;
                }
                let result =
                    if !entry.paused && matches!(state, librqbit::TorrentStatsState::Paused) {
                        engine.session.unpause(&handle).await
                    } else if entry.paused && matches!(state, librqbit::TorrentStatsState::Live) {
                        engine.session.pause(&handle).await
                    } else {
                        continue;
                    };
                if let Err(error) = result {
                    engine.error(&task_id, &format!("同步暂停状态失败：{error:#}"));
                    break;
                }
            }
        });
        self.persist()?;
        self.event(
            id,
            "info",
            "loaded",
            "Rust 引擎已载入任务；首次迁移会校验原目录已有文件",
        );
        if !self.offline {
            let _ = self.start_discovery(id, true);
        }
        Ok(())
    }

    pub async fn add(
        self: &Arc<Self>,
        source: String,
        save_path: String,
        paused: bool,
        only_files: Option<Vec<usize>>,
    ) -> Result<String> {
        let source = source.trim().to_string();
        let save = PathBuf::from(save_path.trim());
        if source.is_empty() || !save.is_absolute() {
            bail!("请填写磁力链接或种子文件，以及绝对保存路径");
        }
        if crate::http_download::is_http(&source) {
            crate::http_download::validate_url(&source)?;
        } else if !source.starts_with("magnet:?") {
            let p = Path::new(&source);
            if !p
                .extension()
                .and_then(|v| v.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("torrent"))
                || std::fs::metadata(p)?.len() > 20_000_000
            {
                bail!("请选择小于 20 MB 的 .torrent 文件");
            }
            output_path(&std::fs::read(p)?, &save)?;
        } else {
            url::Url::parse(&source)?;
        }
        if let Some(indices) = &only_files {
            anyhow::ensure!(
                !crate::http_download::is_http(&source) && !source.starts_with("magnet:?"),
                "请先解析磁力元数据，再在文件页选择下载文件"
            );
            let files = crate::torrent_preview::read(Path::new(&source))?;
            anyhow::ensure!(
                !indices.is_empty() && indices.iter().all(|i| *i < files.len()),
                "至少选择一个有效文件"
            );
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut entries = self.entries.lock().unwrap();
            if entries.iter().any(|e| e.source == source) {
                bail!("任务已经存在");
            }
            entries.push(Entry {
                id: id.clone(),
                source,
                save_path: save.display().to_string(),
                paused,
                only_files,
                ..Default::default()
            });
        }
        self.persist()?;
        let engine = self.clone();
        let task = id.clone();
        tokio::spawn(async move {
            if let Err(e) = engine.load(&task).await {
                engine.error(&task, &format!("{e:#}"));
            }
        });
        Ok(id)
    }

    pub async fn action(
        self: &Arc<Self>,
        id: &str,
        action: &str,
        trackers_text: &str,
    ) -> Result<()> {
        let entry = self.entry(id)?;
        if self.completed_cache.lock().unwrap().contains_key(id) {
            if action == "pause"
                || (action == "resume" && !self.settings.lock().unwrap().seed_after_download)
            {
                return Ok(());
            }
            if matches!(action, "recheck" | "apply_trackers") {
                self.load(id).await?;
            }
        }
        if action == "remove" {
            return self
                .remove(
                    id,
                    if trackers_text.is_empty() {
                        "keep"
                    } else {
                        trackers_text
                    },
                )
                .await;
        }
        if crate::http_download::is_http(&entry.source) {
            anyhow::ensure!(
                matches!(action, "pause" | "resume"),
                "HTTP 任务不支持此 BT 操作"
            );
            if !self.http_jobs.lock().unwrap().contains_key(id) {
                self.load(id).await?;
            }
            let _operation = self.operations.lock().await;
            let job = self
                .http_jobs
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .context("HTTP 任务未载入")?;
            if action == "pause" {
                job.stop().await?;
            } else {
                job.start(self.session.clone()).await?;
            }
            if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                e.paused = action == "pause";
                e.error.clear();
            }
            self.persist()?;
            return Ok(());
        }
        if matches!(action, "discover" | "announce") {
            return self.start_discovery(id, action == "discover");
        }
        if action == "trackers" {
            let candidates = trackers_text
                .lines()
                .filter(|s| !s.trim().is_empty())
                .map(|s| trackers::normalize(s).context("无效 Tracker 地址"))
                .collect::<Result<Vec<_>>>()?;
            if candidates.len() > 200 {
                bail!("最多添加 200 个 Tracker");
            }
            if self
                .handle(id)?
                .with_metadata(|m| m.info.info().private)
                .unwrap_or(false)
            {
                bail!("私有种子不能补充公开 Tracker");
            }
            {
                let mut entries = self.entries.lock().unwrap();
                let e = entries.iter_mut().find(|e| e.id == id).unwrap();
                e.trackers.extend(candidates);
                e.trackers.sort();
                e.trackers.dedup();
                e.trackers.truncate(200);
            }
            self.persist()?;
            self.event(
                id,
                "info",
                action,
                "候选已保存；点击应用候选后由引擎使用（将重新校验文件）",
            );
            return Ok(());
        }
        if action == "pause" && self.handle(id).is_err() {
            // Metadata resolution may still finish, but load() re-reads this intent
            // under the operation lock before creating a downloading handle.
            let _operation = self.operations.lock().await;
            if let Ok(h) = self.handle(id) {
                if !h.is_paused() && !matches!(h.stats().state, librqbit::TorrentStatsState::Paused)
                {
                    self.session.pause(&h).await?;
                }
            }
            if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                e.paused = true;
            }
            self.persist()?;
            self.event(id, "info", "pause", "任务已暂停");
            return Ok(());
        }
        if action == "resume" && self.handle(id).is_err() {
            if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                e.paused = false;
                e.error.clear();
            }
            self.persist()?;
            let engine = self.clone();
            let task = id.to_string();
            tokio::spawn(async move {
                if let Err(e) = engine.load(&task).await {
                    engine.error(&task, &format!("{e:#}"));
                }
            });
            return Ok(());
        }
        let _operation = self.operations.lock().await;
        let handle = self.handle(id).ok();
        match action {
            "pause" | "resume" => {
                let h = handle.context("任务正在解析，请等待后重试")?;
                if action == "resume"
                    && h.stats().finished
                    && !self.settings.lock().unwrap().seed_after_download
                {
                    if matches!(h.stats().state, librqbit::TorrentStatsState::Live) {
                        self.session.pause(&h).await?;
                    }
                    if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                        e.paused = true;
                    }
                    self.persist()?;
                    return Ok(());
                }
                if action == "pause" {
                    if !h.is_paused()
                        && !matches!(h.stats().state, librqbit::TorrentStatsState::Paused)
                    {
                        self.session.pause(&h).await?;
                    }
                } else {
                    if h.is_paused()
                        || matches!(
                            h.stats().state,
                            librqbit::TorrentStatsState::Paused
                                | librqbit::TorrentStatsState::Error
                        )
                    {
                        self.session.unpause(&h).await?;
                    }
                }
                if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                    e.paused = action == "pause";
                }
            }
            "remove" => {
                if let Some(h) = handle {
                    self.session
                        .delete(TorrentIdOrHash::Id(h.id()), false)
                        .await?;
                }
                self.handles.lock().unwrap().remove(id);
                self.entries.lock().unwrap().retain(|e| e.id != id);
            }
            "recheck" | "apply_trackers" => {
                let h = handle.context("任务尚未载入")?;
                self.session
                    .delete(TorrentIdOrHash::Id(h.id()), false)
                    .await?;
                self.handles.lock().unwrap().remove(id);
                drop(_operation);
                self.load(id).await?;
                self.event(
                    id,
                    "info",
                    action,
                    "重新载入任务并校验已有数据，不删除下载文件",
                );
                return Ok(());
            }
            _ => bail!("未知操作"),
        }
        self.persist()?;
        self.event(id, "info", action, &format!("操作完成：{action}"));
        Ok(())
    }

    async fn select_files(self: &Arc<Self>, id: &str, indices: Vec<usize>) -> Result<()> {
        if self.completed_cache.lock().unwrap().contains_key(id) {
            self.load(id).await?;
        }
        let _operation = self.operations.lock().await;
        let h = self.handle(id)?;
        let count = h.with_metadata(|m| m.file_infos.len())?;
        let selected: std::collections::HashSet<usize> = indices.into_iter().collect();
        anyhow::ensure!(
            !selected.is_empty(),
            "至少选择一个文件；需要停止下载请暂停任务"
        );
        anyhow::ensure!(selected.iter().all(|i| *i < count), "文件编号无效");
        self.session.update_only_files(&h, &selected).await?;
        let mut indices: Vec<_> = selected.into_iter().collect();
        indices.sort();
        if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
            e.only_files = Some(indices);
        }
        self.persist()?;
        self.event(
            id,
            "info",
            "files",
            "已更新下载文件选择；相邻文件可能包含共享分块数据",
        );
        Ok(())
    }

    async fn remove(self: &Arc<Self>, id: &str, mode: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(mode, "keep" | "incomplete" | "all"),
            "无效删除模式"
        );
        let _operation = self.operations.lock().await;
        let entry = self.entry(id)?;
        if crate::http_download::is_http(&entry.source) {
            if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                e.paused = true;
            }
            self.persist()?;
            let job = self.http_jobs.lock().unwrap().get(id).cloned();
            if let Some(job) = job {
                job.remove(mode).await?;
            } else {
                crate::http_download::Job::remove_stopped(
                    &self.data,
                    id,
                    &entry.source,
                    Path::new(&entry.save_path),
                    mode,
                )?;
            }
            self.http_jobs.lock().unwrap().remove(id);
        } else {
            let mut handle = self.handle(id).ok();
            let root = entry
                .rust_output
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&entry.save_path));
            let files = if mode == "keep" {
                Vec::new()
            } else if let Some(files) = entry.deletion_files.clone() {
                files
            } else if let Some(h) = &handle {
                anyhow::ensure!(
                    !matches!(
                        h.stats().state,
                        librqbit::TorrentStatsState::Initializing { .. }
                    ),
                    "文件校验中，请等待结束后删除数据"
                );
                if !h.is_paused() {
                    self.session.pause(h).await?;
                }
                if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                    e.paused = true;
                }
                let stats = h.stats();
                h.with_metadata(|m| {
                    m.file_infos
                        .iter()
                        .enumerate()
                        .filter(|(i, f)| {
                            mode == "all"
                                || stats.file_progress.get(*i).copied().unwrap_or(0) < f.len
                        })
                        .map(|(_, f)| root.join(&f.relative_filename))
                        .collect::<Vec<_>>()
                })?
            } else if let Some(cached) = self.completed_cache.lock().unwrap().get(id) {
                if mode == "all" {
                    cached["files"]
                        .as_array()
                        .context("缺少文件记录")?
                        .iter()
                        .map(|f| root.join(f["path"].as_str().unwrap_or("")))
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                // Without loaded metadata no download paths can be safely attributed to this task.
                anyhow::ensure!(
                    entry.rust_output.is_none(),
                    "任务未载入，无法安全确认数据文件；请先载入或选择保留文件"
                );
                Vec::new()
            };
            crate::file_ops::validate_files(&root, &files)?;
            // Never remove files another loaded task is using.
            for (other, h) in self
                .handles
                .lock()
                .unwrap()
                .iter()
                .filter(|(other, _)| other.as_str() != id)
            {
                let overlap = h.with_metadata(|m| {
                    m.file_infos
                        .iter()
                        .any(|f| files.contains(&h.output_folder().join(&f.relative_filename)))
                })?;
                anyhow::ensure!(!overlap, "文件被另一任务 {other} 使用，拒绝删除");
            }
            if let Some(e) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                e.deletion_files = Some(files.clone());
                e.paused = true;
            }
            self.persist()?;
            if let Some(h) = handle.take() {
                self.session
                    .delete(TorrentIdOrHash::Id(h.id()), false)
                    .await?;
            }
            self.handles.lock().unwrap().remove(id);
            crate::file_ops::remove_files(&root, &files)?;
        }
        for extension in ["torrent", "resume"] {
            let file = self.data.join(format!("{id}.{extension}"));
            if file.exists() {
                std::fs::remove_file(file)?;
            }
        }
        self.entries.lock().unwrap().retain(|e| e.id != id);
        self.discovery.lock().unwrap().remove(id);
        self.persist()?;
        self.event(
            id,
            "info",
            "remove",
            &format!("任务已删除，文件处理方式：{mode}"),
        );
        Ok(())
    }

    fn start_discovery(self: &Arc<Self>, id: &str, fetch: bool) -> Result<()> {
        if self.offline {
            bail!("离线测试模式不查询外网 Tracker");
        }
        let h = self.handle(id)?;
        if h.with_metadata(|m| m.info.info().private)? {
            bail!("私有种子仅使用原有 Tracker");
        }
        {
            let mut all = self.discovery.lock().unwrap();
            let prev = all.get(id);
            if prev.is_some_and(|v| {
                v["running"] == true
                    || trackers::now().saturating_sub(v["started"].as_u64().unwrap_or(0)) < 60
            }) {
                bail!("当前轮次尚未结束，或距离上次查询不足 60 秒");
            }
            all.insert(id.into(),json!({"running":true,"started":trackers::now(),"message":"正在按当前资源哈希查询候选 Tracker…"}));
        }
        let engine = self.clone();
        let task = id.to_string();
        let hash: [u8; 20] = h.info_hash().0;
        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .user_agent("Flow/0.2")
                .build()
                .unwrap();
            let (found, notes) = if fetch {
                engine.subscriptions.candidates(&client).await
            } else {
                (BTreeMap::new(), vec![])
            };
            let Ok(mut entry) = engine.entry(&task) else {
                return;
            };
            // Preserve original trackers; use remaining slots for ranked candidates.
            let mut extra: Vec<_> = found
                .keys()
                .filter(|u| !entry.trackers.contains(u))
                .cloned()
                .collect();
            extra.sort_by(|a, b| {
                entry
                    .metrics
                    .get(b)
                    .and_then(Metric::score)
                    .unwrap_or(-1.0)
                    .total_cmp(&entry.metrics.get(a).and_then(Metric::score).unwrap_or(-1.0))
            });
            let room = 200usize.saturating_sub(entry.trackers.len());
            entry.trackers.extend(extra.into_iter().take(room));
            if let Some(e) = engine
                .entries
                .lock()
                .unwrap()
                .iter_mut()
                .find(|e| e.id == task)
            {
                e.trackers = entry.trackers.clone();
            }
            let _ = engine.persist();
            let semaphore = engine.probe_slots.clone();
            let mut probes = tokio::task::JoinSet::new();
            for url in &entry.trackers {
                let mut old = entry.metrics.get(url).cloned().unwrap_or_default();
                if old.retry_at > trackers::now() {
                    continue;
                }
                if let Some(source) = found.get(url) {
                    old.source = source.clone();
                } else if old.source.is_empty() {
                    old.source = "原有配置".into();
                }
                let client = client.clone();
                let url = url.clone();
                let sem = semaphore.clone();
                probes.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let metric = trackers::probe(&client, &url, hash, old).await;
                    (url, metric)
                });
            }
            let mut success = 0;
            while let Some(Ok((url, metric))) = probes.join_next().await {
                if metric.status == "响应正常" {
                    success += 1;
                }
                if let Some(e) = engine
                    .entries
                    .lock()
                    .unwrap()
                    .iter_mut()
                    .find(|e| e.id == task)
                {
                    e.metrics.insert(url, metric);
                }
            }
            let message = format!(
                "查询完成：{success}/{} 个 Tracker 返回本资源统计。应用候选会重新校验已有文件。{}",
                entry.trackers.len(),
                notes.join("；")
            );
            engine.discovery.lock().unwrap().insert(
                task.clone(),
                json!({"running":false,"started":trackers::now(),"message":message}),
            );
            engine.event(&task, "info", "tracker_discovery", &message);
            let _ = engine.persist();
        });
        Ok(())
    }

    // Completed, stopped tasks do not need thousands of writable file handles.
    // Trust only a complete persisted piece bitmap, as fast resume does; this
    // is saved completion state, not a fresh disk verification.
    fn cached_complete(&self, entry: &Entry) -> Result<Value> {
        let bytes = std::fs::read(self.data.join(format!("{}.torrent", entry.id)))?;
        let parsed = librqbit::torrent_from_bytes(&bytes)?;
        use serde_bencode::value::Value as B;
        let B::Dict(root) = serde_bencode::from_bytes::<B>(&bytes)? else {
            bail!("metadata");
        };
        let Some(B::Dict(info)) = root.get(b"info".as_slice()) else {
            bail!("info");
        };
        let Some(B::Bytes(pieces)) = info.get(b"pieces".as_slice()) else {
            bail!("pieces");
        };
        let count = pieces.len() / 20;
        let bits = std::fs::read(
            self.data
                .join("rqbit")
                .join(format!("{:?}.bitv", parsed.info_hash)),
        )?;
        anyhow::ensure!(
            count > 0
                && bits.len() == count.div_ceil(8)
                && (0..count).all(|i| bits[i / 8] & (0x80 >> (i % 8)) != 0),
            "尚未完成"
        );
        let (_, total) = metadata_size(&bytes)?;
        let output = entry
            .rust_output
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(output_path(&bytes, Path::new(&entry.save_path))?);
        let mut files = Vec::new();
        if let Some(B::List(rows)) = info.get(b"files".as_slice()) {
            for (index, row) in rows.iter().enumerate() {
                let B::Dict(row) = row else {
                    bail!("file");
                };
                let Some(B::List(parts)) = row
                    .get(b"path.utf-8".as_slice())
                    .or_else(|| row.get(b"path".as_slice()))
                else {
                    bail!("path");
                };
                let mut path = PathBuf::new();
                for part in parts {
                    let B::Bytes(part) = part else {
                        bail!("path");
                    };
                    let part = std::str::from_utf8(part)?;
                    anyhow::ensure!(
                        !part.is_empty()
                            && part != "."
                            && part != ".."
                            && !part.contains(['/', '\\', ':']),
                        "unsafe path"
                    );
                    path.push(part);
                }
                let Some(B::Int(size)) = row.get(b"length".as_slice()) else {
                    bail!("length");
                };
                files.push(json!({"index":index,"path":path.display().to_string(),"size":size,"done":size,"selected":true,"source":output.join(path).display().to_string()}));
            }
        } else {
            let Some(B::Bytes(name)) = info
                .get(b"name.utf-8".as_slice())
                .or_else(|| info.get(b"name".as_slice()))
            else {
                bail!("name");
            };
            let name = std::str::from_utf8(name)?;
            anyhow::ensure!(
                !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':']),
                "unsafe name"
            );
            files.push(json!({"index":0,"path":name,"size":total,"done":total,"selected":true,"source":output.join(name).display().to_string()}));
        }
        Ok(json!({"total":total,"files":files}))
    }

    async fn recover_disconnected(&self) {
        if self.offline {
            return;
        }
        let handles = self.handles.lock().unwrap().clone();
        self.recovery
            .lock()
            .unwrap()
            .retain(|id, _| handles.contains_key(id));
        for (id, handle) in handles {
            let Ok(_operation) = self.operations.try_lock() else {
                continue;
            };
            let Ok(entry) = self.entry(&id) else {
                continue;
            };
            let stats = handle.stats();
            let stalled = !entry.paused
                && !handle.is_paused()
                && !stats.finished
                && stats.error.is_none()
                && stats.live.as_ref().is_some_and(|live| {
                    live.download_speed.as_bytes() == 0 && live.snapshot.peer_stats.live == 0
                });
            let due = self
                .recovery
                .lock()
                .unwrap()
                .entry(id.clone())
                .or_default()
                .due(trackers::now(), stalled, stats.progress_bytes);
            if !due {
                continue;
            }
            // Preserve verified pieces and the same handle; this refreshes its discovery stream.
            let result = async {
                self.session.pause(&handle).await?;
                self.session.unpause(&handle).await
            }
            .await;
            match result {
                Ok(()) => self.event(&id, "info", "peer_recovery", "连续两分钟无连接且无进展，已重新启动节点发现；保留已有数据，十分钟内不重复触发"),
                Err(error) => self.event(&id, "warn", "peer_recovery", &format!("节点发现恢复失败：{error}；可手动继续任务")),
            }
        }
    }

    fn remember_working_peers(&self) -> Result<()> {
        let handles = self.handles.lock().unwrap().clone();
        let mut changed = false;
        for (id, handle) in handles {
            if handle
                .with_metadata(|m| m.info.info().private)
                .unwrap_or(true)
            {
                continue;
            }
            let Some(live) = handle.live() else {
                continue;
            };
            let mut good = live
                .per_peer_stats_snapshot(
                    serde_json::from_value(json!({"state":"all"})).expect("valid peer filter"),
                )
                .peers
                .into_iter()
                .filter_map(|(addr, p)| {
                    addr.parse::<std::net::SocketAddr>()
                        .ok()
                        .map(|addr| (addr, p))
                })
                .filter(|(addr, p)| {
                    addr.port() != 0
                        && !addr.ip().is_unspecified()
                        && !addr.ip().is_multicast()
                        && p.counters.fetched_bytes > 0
                        && self
                            .peer_records
                            .lock()
                            .unwrap()
                            .get(&addr.ip().to_string())
                            .is_none_or(|r| r["blocked"] != true)
                })
                .collect::<Vec<_>>();
            {
                let quality = self.peer_quality.lock().unwrap();
                good.sort_by_key(|(addr, p)| {
                    let q = quality
                        .get(&id)
                        .and_then(|peers| peers.get(&addr.to_string()))
                        .map(|w| w.quality(trackers::now()));
                    (
                        q.map(|q| q.rank).unwrap_or(2),
                        std::cmp::Reverse(q.map(|q| q.rate).unwrap_or(0)),
                        std::cmp::Reverse(p.counters.fetched_bytes),
                    )
                });
            }
            let peers = good
                .into_iter()
                .take(64)
                .map(|(addr, _)| addr)
                .collect::<Vec<_>>();
            if peers.is_empty() {
                continue;
            }
            if let Some(entry) = self.entries.lock().unwrap().iter_mut().find(|e| e.id == id) {
                entry.initial_peers = peers;
                entry.peer_cache_updated = trackers::now();
                changed = true;
            }
        }
        if changed {
            self.persist()?;
        }
        atomic(
            &self.data.join("peer-records.json"),
            &serde_json::to_vec_pretty(&*self.peer_records.lock().unwrap())?,
        )?;
        Ok(())
    }

    pub fn snapshot(&self, selected: &str) -> Value {
        let entries = self.entries.lock().unwrap().clone();
        let handles = self.handles.lock().unwrap().clone();
        self.peer_quality
            .lock()
            .unwrap()
            .retain(|id, _| handles.contains_key(id));
        let mut tasks = Vec::new();
        let mut detail = json!({"files":[],"peers":[],"trackers":[],"discovery":self.discovery.lock().unwrap().get(selected).cloned().unwrap_or(json!({}))});
        for e in entries {
            let mut item = json!({"id":e.id,"name":e.source,"save_path":e.save_path,"paused":e.paused,"error":e.error,"progress":0,"download_rate":0,"upload_rate":0,"done":0,"total":0,"peers":0,"seeds":null,"availability":-1,"state":"loading","diagnosis":if e.error.is_empty(){"正在解析元数据或载入任务"}else{&e.error}});
            if e.error.is_empty() {
                if let Some(load) = self.loading.lock().unwrap().get(&e.id) {
                    item["total"] = load["total"].clone();
                    item["diagnosis"] = json!(format!(
                        "{} · {} 个文件 · 已等待 {} 秒；下载进度将在续传状态就绪后显示",
                        load["stage"].as_str().unwrap_or("恢复中"),
                        load["files"]
                            .as_u64()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "未知数量".into()),
                        trackers::now().saturating_sub(load["started"].as_u64().unwrap_or(0))
                    ));
                }
            }
            item["kind"] = json!(if crate::http_download::is_http(&e.source) {
                "http"
            } else {
                "bt"
            });
            item["source"] = json!(e.source);
            if let Some(job) = self.http_jobs.lock().unwrap().get(&e.id).cloned() {
                let s = job.state.lock().unwrap().clone();
                item["name"] = json!(s.name);
                if crate::media::playable(&s.name) {
                    item["media_files"] = json!([{"index":0,"path":s.name,"size":s.total,
                        "source":if s.complete {job.destination.display().to_string()} else {e.source.clone()}}]);
                }
                item["done"] = json!(s.done);
                item["total"] = json!(s.total.unwrap_or(0));
                item["progress"] = json!(if s.complete {
                    1.0
                } else {
                    s.total
                        .filter(|n| *n > 0)
                        .map(|n| s.done as f64 / n as f64)
                        .unwrap_or(0.0)
                });
                item["download_rate"] = json!(s.rate);
                item["error"] = json!(s.error);
                item["state"] = json!(if s.complete {
                    "complete"
                } else if e.paused {
                    "paused"
                } else if s.running {
                    "http"
                } else {
                    "error"
                });
                item["diagnosis"] = json!(if !s.error.is_empty() {
                    s.error
                } else if s.complete {
                    "HTTP 下载完成".into()
                } else if e.paused {
                    "已暂停，临时文件保留".into()
                } else {
                    "HTTP 传输中；支持续传的服务器将复用临时文件".into()
                });
                if selected == e.id {
                    detail["files"] =
                        json!([{"path":s.name,"size":s.total,"done":s.done,"selected":true}]);
                }
                tasks.push(item);
                continue;
            }
            if e.source.starts_with("magnet:") {
                item["magnet"] = json!(e.source);
            }
            item["name"] = json!(
                url::Url::parse(&e.source)
                    .ok()
                    .and_then(|u| u
                        .query_pairs()
                        .find(|(k, _)| k == "dn")
                        .map(|(_, v)| v.into_owned()))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| if e.source.starts_with("magnet:") {
                        "磁力任务 · 等待元数据".into()
                    } else {
                        Path::new(&e.source)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    })
            );
            if let Some(cached) = self.completed_cache.lock().unwrap().get(&e.id) {
                if selected == e.id {
                    detail["files"] = cached["files"].clone();
                }
                item["media_files"] = json!(
                    cached["files"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|f| crate::media::playable(f["path"].as_str().unwrap_or("")))
                        .cloned()
                        .collect::<Vec<_>>()
                );
                item["total"] = cached["total"].clone();
                item["done"] = cached["total"].clone();
                item["progress"] = json!(1.0);
                item["state"] = json!("complete");
                item["diagnosis"] =
                    json!("已完成，已停止做种（已保存的完成状态；需要重新验证文件时点击校验文件）");
            }
            if let Some(h) = handles.get(&e.id) {
                let stats = h.stats();
                let mut magnet = url::Url::parse("magnet:?").unwrap();
                magnet
                    .query_pairs_mut()
                    .append_pair("xt", &format!("urn:btih:{}", h.info_hash().as_string()));
                if let Some(name) = h.name() {
                    magnet.query_pairs_mut().append_pair("dn", &name);
                }
                for tracker in &e.trackers {
                    magnet.query_pairs_mut().append_pair("tr", tracker);
                }
                item["magnet"] = json!(magnet.to_string());
                let initializing = matches!(
                    stats.state,
                    librqbit::TorrentStatsState::Initializing { .. }
                );
                let (down, up, peers) = stats
                    .live
                    .as_ref()
                    .map(|s| {
                        (
                            s.download_speed.as_bytes(),
                            s.upload_speed.as_bytes(),
                            serde_json::to_value(&s.snapshot.peer_stats).unwrap_or(json!({})),
                        )
                    })
                    .unwrap_or((0, 0, json!({})));
                let live_peers = peers["live"].as_u64().unwrap_or(0);
                let peer_details = h.live().map(|live| {
                    live.per_peer_stats_snapshot(
                        serde_json::from_value(json!({"state":"all"})).expect("valid peer filter"),
                    )
                });
                let attempts: u64 = peer_details
                    .as_ref()
                    .map(|p| {
                        p.peers
                            .values()
                            .map(|p| u64::from(p.counters.connection_attempts))
                            .sum()
                    })
                    .unwrap_or(0);
                let connection_errors: u64 = peer_details
                    .as_ref()
                    .map(|p| p.peers.values().map(|p| u64::from(p.counters.errors)).sum())
                    .unwrap_or(0);
                item["connection_attempts"] = json!(attempts);
                item["connection_errors"] = json!(connection_errors);
                item["cached_peers"] = json!(e.initial_peers.len());
                // The engine may be paused even when the saved intent is running.
                let actually_paused =
                    h.is_paused() || matches!(stats.state, librqbit::TorrentStatsState::Paused);
                item["paused"] = json!(actually_paused);
                let mut peer_rows = Vec::new();
                let mut stable_peers = 0usize;
                {
                    let mut quality = self.peer_quality.lock().unwrap();
                    let windows = quality.entry(e.id.clone()).or_default();
                    if actually_paused || initializing || stats.finished {
                        windows.clear();
                    }
                    if let Some(details) = &peer_details {
                        windows.retain(|addr, _| details.peers.contains_key(addr));
                        for (addr, peer) in &details.peers {
                            let now = trackers::now();
                            let q = windows
                                .entry(addr.clone())
                                .or_insert_with(|| {
                                    crate::peer_quality::Window::new(
                                        now,
                                        peer.counters.fetched_bytes,
                                    )
                                })
                                .observe(now, peer.counters.fetched_bytes);
                            if q.rank == 0 {
                                stable_peers += 1;
                            }
                            peer_rows.push(json!({"address":addr,"client":peer.client_name.clone().unwrap_or_default(),"downloaded":peer.counters.fetched_bytes,"uploaded":peer.counters.uploaded_bytes,"errors":peer.counters.errors,"state":peer.state,"transfer_rank":q.rank,"recent_rate":q.rate,"idle_seconds":q.idle,"transfer_status":if peer.state == "dead" && peer.counters.fetched_bytes > 0 { "已断开 · 曾有效传输" } else { q.label }}));
                        }
                    }
                }
                {
                    let mut records = self.peer_records.lock().unwrap();
                    for row in &peer_rows {
                        if let Ok(addr) = row["address"]
                            .as_str()
                            .unwrap_or("")
                            .parse::<std::net::SocketAddr>()
                        {
                            let record = records.entry(addr.ip().to_string()).or_insert_with(
                                || json!({"first_seen":trackers::now(),"blocked":false}),
                            );
                            for key in [
                                "address",
                                "client",
                                "downloaded",
                                "uploaded",
                                "errors",
                                "recent_rate",
                                "transfer_status",
                                "idle_seconds",
                            ] {
                                record[key] = row[key].clone();
                            }
                            record["task"] = json!(e.id);
                            record["last_seen"] = json!(trackers::now());
                            record["score"] = match row["transfer_rank"].as_u64() {
                                Some(0) => json!(90),
                                Some(1) => json!(60),
                                Some(3) => json!(10),
                                _ => Value::Null,
                            };
                        }
                    }
                    while records.len() > 2000 {
                        let oldest = records
                            .iter()
                            .filter(|(_, r)| r["blocked"] != true)
                            .min_by_key(|(_, r)| r["last_seen"].as_u64().unwrap_or(0))
                            .map(|(ip, _)| ip.clone());
                        if let Some(ip) = oldest {
                            records.remove(&ip);
                        } else {
                            break;
                        }
                    }
                }
                peer_rows.sort_by_key(|p| {
                    (
                        p["transfer_rank"].as_u64().unwrap_or(3),
                        std::cmp::Reverse(p["recent_rate"].as_u64().unwrap_or(0)),
                        p["address"].as_str().unwrap_or("").to_owned(),
                    )
                });
                item["stable_peers"] = json!(stable_peers);

                let ratio = if stats.total_bytes == 0 {
                    0.0
                } else {
                    stats.progress_bytes as f64 / stats.total_bytes as f64
                };
                let diagnosis = if let Some(error) = stats.error.as_ref() {
                    error.clone()
                } else if initializing {
                    format!("校验已有文件 {:.1}%（不是下载完成度）", ratio * 100.0)
                } else if actually_paused && stats.finished {
                    "下载完成，已停止做种".into()
                } else if actually_paused {
                    "任务已暂停".into()
                } else if stats.finished {
                    "下载完成，正在做种".into()
                } else if down > 0 {
                    format!(
                        "正在接收数据；当前连接 {live_peers} 个，已观察到持续收数节点 {stable_peers} 个"
                    )
                } else if live_peers == 0 && attempts > 0 {
                    format!(
                        "已找到候选节点，累计尝试连接 {attempts} 次，节点错误 {connection_errors} 次，目前无连接；引擎继续重试。Tracker 报告数不代表可连接节点"
                    )
                } else if live_peers == 0 {
                    format!(
                        "元数据已就绪，尚未观察到节点连接尝试；DHT {}，缓存节点 {}",
                        if self.session.get_dht().is_some() {
                            "已启用"
                        } else {
                            "关闭"
                        },
                        e.initial_peers.len()
                    )
                } else {
                    "节点已连接但暂无数据：可能被对端限速或没有当前所需分片；引擎尚未提供可区分两者的指标".into()
                };
                if let Some(metadata) = h.metadata.load_full() {
                    item["media_files"] = json!(metadata.file_infos.iter().enumerate()
                        .filter(|(_,f)|crate::media::playable(&f.relative_filename.to_string_lossy()))
                        .map(|(index,f)|json!({"index":index,"path":f.relative_filename.display().to_string(),"size":f.len}))
                        .collect::<Vec<_>>());
                }
                item["name"] = json!(h.name().unwrap_or_else(|| e.source.clone()));
                item["state"] = json!(if initializing {
                    "checking".to_string()
                } else {
                    stats.state.to_string()
                });
                item["error"] = json!(stats.error.unwrap_or_default());
                item["diagnosis"] = json!(diagnosis);
                item["progress"] = json!(if initializing { 0.0 } else { ratio });
                item["done"] = json!(if initializing {
                    0
                } else {
                    stats.progress_bytes
                });
                item["total"] = json!(stats.total_bytes);
                item["download_rate"] = json!(down);
                item["upload_rate"] = json!(up);
                item["peers"] = json!(live_peers);
                if selected == e.id {
                    if let Some(m) = h.metadata.load_full() {
                        let selected_files = h.only_files();
                        detail["files"]=json!(m.file_infos.iter().enumerate().map(|(i,f)|json!({"index":i,"path":f.relative_filename.display().to_string(),"size":f.len,"done":stats.file_progress.get(i).copied().unwrap_or(0),"selected":selected_files.as_ref().is_none_or(|s|s.contains(&i))})).collect::<Vec<_>>());
                    }
                    // Session lookup takes its global DB lock, held while another
                    // torrent opens all files. We already own this task's handle.
                    detail["peers"] = json!(peer_rows);
                }
            }
            if selected == e.id {
                let mut rows = e
                    .trackers
                    .iter()
                    .map(|url| {
                        let metric = e.metrics.get(url).cloned().unwrap_or_default();
                        let mut row = serde_json::to_value(&metric).unwrap();
                        row["url"] = json!(url);

                        row
                    })
                    .collect::<Vec<_>>();
                rows.sort_by_key(|r| r["url"].as_str().unwrap_or("").to_owned());
                detail["trackers"] = json!(rows);
            }
            tasks.push(item);
        }
        let records: Vec<Value> = self
            .peer_records
            .lock()
            .unwrap()
            .iter()
            .map(|(ip, record)| {
                let mut row = record.clone();
                row["ip"] = json!(ip);
                row["active_block"] = json!(self.active_bans.contains(ip));
                row
            })
            .collect();
        json!({"peer_records":records,"tasks":tasks,"detail":detail,"settings":*self.settings.lock().unwrap(),"subscriptions":self.subscriptions.snapshot(),"events":*self.events.lock().unwrap(),"engine":"librqbit 9.0.1 · Rust","listen_port":self.session.announce_port().unwrap_or(0),"updated_at":trackers::now(),"discovery":*self.discovery.lock().unwrap()})
    }
}

#[derive(Clone)]
struct ApiState {
    engine: Arc<Engine>,
    token: String,
}
async fn auth(State(s): State<ApiState>, request: Request, next: Next) -> Response {
    if request
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        != Some(format!("Bearer {}", s.token).as_str())
    {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"未授权"}))).into_response();
    }
    next.run(request).await
}
async fn state(
    State(s): State<ApiState>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    Json(
        s.engine
            .snapshot(q.get("selected").map(String::as_str).unwrap_or("")),
    )
}
fn reply(result: Result<Value>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{e:#}")})),
        )
            .into_response(),
    }
}
async fn add(State(s): State<ApiState>, Json(v): Json<Value>) -> Response {
    reply(
        s.engine
            .add(
                v["source"].as_str().unwrap_or("").into(),
                v["save_path"].as_str().unwrap_or("").into(),
                v["paused"].as_bool().unwrap_or(false),
                match v.get("only_files").filter(|v| !v.is_null()) {
                    Some(value) => match serde_json::from_value::<Vec<usize>>(value.clone()) {
                        Ok(indices) => Some(indices),
                        Err(_) => return reply(Err(anyhow::anyhow!("文件选择格式无效"))),
                    },
                    None => None,
                },
            )
            .await
            .map(|id| json!({"id":id})),
    )
}
async fn peer_policy(State(s): State<ApiState>, Json(v): Json<Value>) -> Response {
    reply((|| -> Result<Value> {
        let ip = v["ip"]
            .as_str()
            .context("缺少 IP")?
            .parse::<std::net::IpAddr>()?
            .to_string();
        let blocked = v["blocked"].as_bool().context("缺少黑名单状态")?;
        let mut records = s.engine.peer_records.lock().unwrap();
        let mut updated = records.clone();
        let row = updated.get_mut(&ip).context("节点记录不存在")?;
        row["blocked"] = json!(blocked);
        row["policy_updated"] = json!(trackers::now());
        row["reason"] = json!(if blocked {
            "用户根据传输记录手动加入黑名单"
        } else {
            "用户手动解除黑名单"
        });
        atomic(
            &s.engine.data.join("peer-records.json"),
            &serde_json::to_vec_pretty(&updated)?,
        )?;
        *records = updated;
        drop(records);
        if blocked {
            for entry in s.engine.entries.lock().unwrap().iter_mut() {
                entry
                    .initial_peers
                    .retain(|addr| addr.ip().to_string() != ip);
            }
            s.engine.persist()?;
        }
        s.engine.event(
            "",
            "info",
            "peer_policy",
            &format!("节点 {ip} 黑名单={blocked}；重启后应用连接规则"),
        );
        Ok(json!({"ok":true,"restart_required":true}))
    })())
}

async fn action(State(s): State<ApiState>, Json(v): Json<Value>) -> Response {
    reply(
        s.engine
            .action(
                v["id"].as_str().unwrap_or(""),
                v["action"].as_str().unwrap_or(""),
                if v["action"] == "remove" {
                    v["delete_mode"].as_str().unwrap_or("keep")
                } else {
                    v["trackers"].as_str().unwrap_or("")
                },
            )
            .await
            .map(|_| json!({"ok":true})),
    )
}

// Opening a stream does not silently resume a paused task. The explicit play
// action selects its file and resumes first; GET/HEAD only read the stream.
async fn prepare_play(State(s): State<ApiState>, Json(v): Json<Value>) -> Response {
    reply(
        async {
            let id = v["id"].as_str().context("缺少任务编号")?;
            let index = v["index"].as_u64().context("缺少文件编号")? as usize;
            let h = s.engine.handle(id)?;
            anyhow::ensure!(
                !matches!(
                    h.stats().state,
                    librqbit::TorrentStatsState::Initializing { .. }
                ),
                "文件正在校验，请校验完成后播放"
            );
            h.with_metadata(|m| m.file_infos.get(index).map(|f| f.relative_filename.clone()))?
                .context("文件编号无效")?;
            if let Some(mut selected) = h.only_files() {
                if !selected.contains(&index) {
                    selected.push(index);
                    s.engine.select_files(id, selected).await?;
                }
            }
            s.engine.action(id, "resume", "").await?;
            Ok(json!({"path":format!("/api/media/{id}/{index}")}))
        }
        .await,
    )
}

async fn media_stream(
    State(s): State<ApiState>,
    axum::extract::Path((id, index)): axum::extract::Path<(String, usize)>,
    headers: axum::http::HeaderMap,
    method: axum::http::Method,
) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let result = async {
        let h = s.engine.handle(&id)?;
        let mut stream = h.stream(index).await?;
        let len = stream.len();
        let request_range = match headers.get("range") {
            Some(value) => Some(value.to_str().context("无效 Range 请求头")?),
            None => None,
        };
        let (start, count, partial) = match crate::media::range(request_range, len) {
            Ok(r) => r,
            Err(()) => {
                return Ok(Response::builder()
                    .status(416)
                    .header("Content-Range", format!("bytes */{len}"))
                    .body(axum::body::Body::empty())?);
            }
        };
        stream.seek(std::io::SeekFrom::Start(start)).await?;
        let body = if method == axum::http::Method::HEAD {
            axum::body::Body::empty()
        } else {
            axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(stream.take(count)))
        };
        let mut response = Response::builder()
            .status(if partial { 206 } else { 200 })
            .header("Accept-Ranges", "bytes")
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", count)
            .header("Cache-Control", "no-store");
        if partial {
            response = response.header(
                "Content-Range",
                format!("bytes {}-{}/{len}", start, start + count - 1),
            );
        }
        Ok::<_, anyhow::Error>(response.body(body)?)
    }
    .await;
    match result {
        Ok(response) => response,
        Err(e) => reply(Err(e)),
    }
}

async fn subscriptions(
    State(s): State<ApiState>,
    Json(config): Json<crate::subscriptions::Config>,
) -> Response {
    reply(
        s.engine
            .subscriptions
            .save(config)
            .await
            .map(|_| json!({"ok":true})),
    )
}

async fn select_files(State(s): State<ApiState>, Json(v): Json<Value>) -> Response {
    let result = async {
        let indices: Vec<usize> =
            serde_json::from_value(v["files"].clone()).context("文件选择必须为编号数组")?;
        s.engine
            .select_files(v["id"].as_str().unwrap_or(""), indices)
            .await?;
        Ok(json!({"ok":true}))
    }
    .await;
    reply(result)
}

async fn settings(
    State(s): State<ApiState>,
    Json(config): Json<crate::settings::Settings>,
) -> Response {
    let _operation = s.engine.operations.lock().await;
    let result = (|| -> Result<Value> {
        config.validate()?;
        std::fs::create_dir_all(&config.download_dir).context("无法创建下载目录")?;
        atomic(
            &s.engine.data.join("settings.json"),
            &serde_json::to_vec_pretty(&config)?,
        )?;
        s.engine
            .session
            .ratelimits
            .set_upload_bps(std::num::NonZeroU32::new(config.upload_kib * 1024));
        s.engine
            .session
            .ratelimits
            .set_download_bps(std::num::NonZeroU32::new(config.download_kib * 1024));
        *s.engine.settings.lock().unwrap() = config;
        Ok(json!({"ok":true}))
    })();
    reply(result)
}

pub fn startup_stage(root: &Path, message: &str) {
    let _ = std::fs::create_dir_all(root.join("data"));
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("data/startup.log"))
    {
        let _ = writeln!(file, "{} {}", trackers::now(), message);
    }
    let _ = atomic(&root.join("data/startup-stage.txt"), message.as_bytes());
}

pub async fn serve(
    root: PathBuf,
    token: String,
    offline: bool,
    mut stop: oneshot::Receiver<()>,
    ready: mpsc::SyncSender<std::result::Result<String, String>>,
) -> Result<()> {
    std::fs::create_dir_all(root.join("data"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("data/engine.lock"))?;
    startup_stage(&root, "获取任务目录锁");
    fs2::FileExt::try_lock_exclusive(&lock).context("此项目已有下载引擎运行，请先关闭原窗口")?;
    let engine = tokio::select! {
        result=Engine::new(&root,offline)=>result?,
        _=&mut stop=>{startup_stage(&root,"初始化已取消");return Ok(());}
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let state_data = ApiState {
        engine: engine.clone(),
        token,
    };
    let app = Router::new()
        .route("/api/health", get(|| async { Json(json!({"ok":true})) }))
        .route("/api/state", get(state))
        .route("/api/tasks", post(add))
        .route("/api/action", post(action))
        .route("/api/subscriptions", post(subscriptions))
        .route("/api/peer-policy", post(peer_policy))
        .route("/api/settings", post(settings))
        .route("/api/files", post(select_files))
        .route("/api/play", post(prepare_play))
        .route("/api/media/{id}/{index}", get(media_stream))
        .layer(middleware::from_fn_with_state(state_data.clone(), auth))
        .with_state(state_data);
    let restore = engine.clone();
    let restore_task = tokio::spawn(async move {
        restore.restore().await;
    });
    let snapshots = engine.clone();
    let maintenance = engine.clone();
    let maintenance_task = tokio::spawn(async move {
        loop {
            let minutes = maintenance
                .subscriptions
                .config
                .lock()
                .unwrap()
                .health_minutes;
            tokio::time::sleep(Duration::from_secs(60)).await;
            if maintenance.offline {
                continue;
            }
            let entries = maintenance.entries.lock().unwrap().clone();
            for e in entries.into_iter().filter(|e| !e.paused) {
                let last = maintenance
                    .discovery
                    .lock()
                    .unwrap()
                    .get(&e.id)
                    .and_then(|v| v["started"].as_u64())
                    .unwrap_or(0);
                if trackers::now().saturating_sub(last) < minutes * 60 {
                    continue;
                }
                // Discovery only: never restart a healthy transfer to change trackers.
                let _ = maintenance.start_discovery(&e.id, true);
            }
        }
    });
    let recovery_engine = engine.clone();
    let recovery_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            recovery_engine.recover_disconnected().await;
        }
    });
    let snapshot_task = tokio::spawn(async move {
        let mut sample = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            sample += 1;
            if sample % 15 == 0 {
                let _ = snapshots.remember_working_peers();
            }
            let _ = atomic(
                &snapshots.data.join("status.json"),
                &serde_json::to_vec_pretty(&snapshots.snapshot("")).unwrap(),
            );
        }
    });
    startup_stage(&root, "本机接口已就绪；任务在后台恢复");
    let _ = ready.send(Ok(format!("http://{addr}")));
    use std::future::IntoFuture;
    let (shutdown, stopped) = oneshot::channel::<()>();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result=&mut server=>result?,
        _=&mut stop=>{
            startup_stage(&root,"正在停止接口并保存任务");
            let _=shutdown.send(());
            // A player waiting for missing pieces must not hold shutdown forever.
            let _=tokio::time::timeout(Duration::from_secs(3),&mut server).await;
        }
    }
    restore_task.abort();
    recovery_task.abort();
    snapshot_task.abort();
    maintenance_task.abort();
    engine.persist()?;
    let jobs: Vec<_> = engine.http_jobs.lock().unwrap().values().cloned().collect();
    for job in jobs {
        let _ = job.stop().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), engine.session.stop()).await;
    startup_stage(&root, "引擎已退出");
    drop(engine);
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn persisted_blacklist_is_loaded_by_native_engine() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("data")).unwrap();
        std::fs::write(root.path().join("data/peer-records.json"), br#"{"192.0.2.7":{"blocked":true},"2001:db8::7":{"blocked":true},"192.0.2.8":{"blocked":false}}"#).unwrap();
        let engine = super::Engine::new(root.path(), true).await.unwrap();
        assert!(engine.session.blocklist.has("192.0.2.7".parse().unwrap()));
        assert!(engine.session.blocklist.has("2001:db8::7".parse().unwrap()));
        assert!(!engine.session.blocklist.has("192.0.2.8".parse().unwrap()));
        engine.session.stop().await;
    }
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_restore_uses_bitmap_without_opening_download_files() {
        let temp = tempfile::tempdir().unwrap();
        let engine = Engine::new(temp.path(), true).await.unwrap();
        let mut torrent = b"d4:infod6:lengthi1e4:name1:x12:piece lengthi16384e6:pieces20:".to_vec();
        torrent.extend_from_slice(&[0u8; 20]);
        torrent.extend_from_slice(b"ee");
        let parsed = librqbit::torrent_from_bytes(&torrent).unwrap();
        std::fs::write(engine.data.join("fixture.torrent"), &torrent).unwrap();
        let bitmap = engine
            .data
            .join("rqbit")
            .join(format!("{:?}.bitv", parsed.info_hash));
        std::fs::create_dir_all(bitmap.parent().unwrap()).unwrap();
        let entry = Entry {
            id: "fixture".into(),
            source: "fixture.torrent".into(),
            paused: true,
            save_path: temp.path().join("downloads").display().to_string(),
            ..Default::default()
        };
        std::fs::write(&bitmap, [0u8]).unwrap();
        assert!(engine.cached_complete(&entry).is_err());
        std::fs::write(&bitmap, [0x80u8]).unwrap();
        engine.entries.lock().unwrap().push(entry);
        engine.restore().await;
        assert!(engine.handle("fixture").is_err());
        let state = engine.snapshot("fixture");
        assert_eq!(state["tasks"][0]["progress"], 1.0);
        assert_eq!(state["detail"]["files"][0]["path"], "x");
        assert!(!temp.path().join("downloads/x").exists());
        engine.action("fixture", "pause", "").await.unwrap();
        engine.action("fixture", "resume", "").await.unwrap();
        assert!(engine.handle("fixture").is_err());
        engine.session.stop().await;
    }
    #[test]
    fn restore_cost_counts_files_not_piece_metadata_size() {
        assert_eq!(metadata_size(b"d4:infod6:lengthi123eee").unwrap(), (1, 123));
        assert_eq!(
            metadata_size(b"d4:infod5:filesld6:lengthi10eed6:lengthi20eeeee").unwrap(),
            (2, 30)
        );
        assert!(metadata_size(b"invalid").is_err());
    }
    async fn wait(stage: &str, mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("engine condition timed out: {stage}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actual_transfer_existing_files_pause_restart_and_remove() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let seed_root = root.join("seed");
        let fixture = seed_root.join("fixture");
        std::fs::create_dir_all(&fixture).unwrap();
        let payload = (0..8 * 1024 * 1024usize)
            .map(|i| ((i * 37 + i / 4096) % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(fixture.join("payload.bin"), &payload).unwrap();
        std::fs::write(fixture.join("unselected.bin"), vec![42u8; 32768]).unwrap();
        let seed = Engine::new(&seed_root, true).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tracker_url = format!("http://{}/announce", listener.local_addr().unwrap());
        let port = seed.session.announce_port().unwrap();
        use serde_bencode::value::Value as B;
        let payload_response = serde_bencode::to_bytes(&B::Dict(
            [
                (b"interval".to_vec(), B::Int(1)),
                (
                    b"peers".to_vec(),
                    B::Bytes(vec![127, 0, 0, 1, (port >> 8) as u8, port as u8]),
                ),
            ]
            .into_iter()
            .collect(),
        ))
        .unwrap();
        let router = Router::new().route(
            "/announce",
            get(move || {
                let b = payload_response.clone();
                async move { b }
            }),
        );
        let tracker_server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (torrent, seed_handle) = seed
            .session
            .create_and_serve_torrent(
                &fixture,
                librqbit::CreateTorrentOptions {
                    piece_length: Some(16384),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        seed_handle.wait_until_initialized().await.unwrap();
        assert!(seed_handle.stats().finished);
        // A bare magnet must resolve using cached peers with DHT/public trackers disabled.
        let magnet_root = root.join("magnet-client");
        let magnet_client = Engine::new(&magnet_root, true).await.unwrap();
        magnet_client.entries.lock().unwrap().push(Entry {
            id: "magnet-fixture".into(),
            source: format!(
                "magnet:?xt=urn:btih:{}",
                seed_handle.info_hash().as_string()
            ),
            save_path: magnet_root.join("downloads").display().to_string(),
            paused: true,
            initial_peers: vec![(std::net::Ipv4Addr::LOCALHOST, port).into()],
            ..Default::default()
        });
        tokio::time::timeout(
            Duration::from_secs(30),
            magnet_client.load("magnet-fixture"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(magnet_client.data.join("magnet-fixture.torrent").is_file());
        assert!(
            !magnet_client
                .entry("magnet-fixture")
                .unwrap()
                .initial_peers
                .is_empty()
        );
        magnet_client.session.stop().await;
        let download_root = root.join("client");
        std::fs::create_dir_all(&download_root).unwrap();
        let source = download_root.join("fixture.torrent");
        std::fs::write(&source, torrent.as_bytes().unwrap()).unwrap();
        let dest = download_root.join("downloads");
        let existing = dest.join("fixture");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("payload.bin"), &payload[..1024 * 1024]).unwrap();
        let client = Engine::new(&download_root, true).await.unwrap();
        client.entries.lock().unwrap().push(Entry {
            id: "local".into(),
            source: source.display().to_string(),
            save_path: dest.display().to_string(),
            paused: true,
            trackers: vec![tracker_url],
            initial_peers: vec![
                (
                    std::net::Ipv4Addr::LOCALHOST,
                    seed.session.announce_port().unwrap(),
                )
                    .into(),
            ],
            ..Default::default()
        });
        client.load("local").await.unwrap();
        let handle = client.handle("local").unwrap();
        handle.wait_until_initialized().await.unwrap();
        assert!(handle.stats().progress_bytes >= 1024 * 1024);
        assert!(!handle.stats().finished);
        assert_eq!(handle.output_folder(), existing);
        let selected_index = handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .position(|f| f.relative_filename == Path::new("payload.bin"))
                    .unwrap()
            })
            .unwrap();
        assert!(client.select_files("local", Vec::new()).await.is_err());
        assert!(client.select_files("local", vec![999]).await.is_err());
        client
            .select_files("local", vec![selected_index])
            .await
            .unwrap();
        client.persist().unwrap();
        client.session.stop().await;
        drop(handle);
        drop(client);
        let client = Engine::new(&download_root, true).await.unwrap();
        client.restore().await;
        assert_eq!(
            client.handle("local").unwrap().only_files(),
            Some(vec![selected_index])
        );
        assert!(
            client.snapshot("local")["tasks"][0]["paused"]
                .as_bool()
                .unwrap()
        );
        // Engine state wins over a stale catalog intent, and repeated pause is safe.
        client
            .entries
            .lock()
            .unwrap()
            .iter_mut()
            .find(|e| e.id == "local")
            .unwrap()
            .paused = false;
        assert_eq!(client.snapshot("local")["tasks"][0]["paused"], true);
        client.action("local", "pause", "").await.unwrap();
        assert!(client.entry("local").unwrap().paused);
        client.action("local", "resume", "").await.unwrap();
        client.action("local", "resume", "").await.unwrap();
        wait("first data after restore", || {
            client.handle("local").unwrap().stats().progress_bytes > 1024 * 1024
        })
        .await;
        client.remember_working_peers().unwrap();
        assert!(client.entry("local").unwrap().peer_cache_updated > 0);
        client.action("local", "pause", "").await.unwrap();
        client.action("local", "pause", "").await.unwrap();
        assert!(client.handle("local").unwrap().is_paused());
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.action("local", "resume", "").await.unwrap();
        let handle = client.handle("local").unwrap();
        // A demuxer can request the tail before the whole file finishes.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("range", "bytes=-4096".parse().unwrap());
        let response = media_stream(
            State(ApiState {
                engine: client.clone(),
                token: "fixture".into(),
            }),
            axum::extract::Path(("local".into(), selected_index)),
            headers,
            axum::http::Method::GET,
        )
        .await;
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["content-length"], "4096");
        let tail = tokio::time::timeout(
            Duration::from_secs(30),
            axum::body::to_bytes(response.into_body(), 4096),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&tail[..], &payload[payload.len() - 4096..]);
        wait("download completion", || handle.stats().finished).await;
        wait("automatic stop after completion", || {
            handle.is_paused() && client.entry("local").unwrap().paused
        })
        .await;
        client.action("local", "resume", "").await.unwrap();
        assert!(handle.is_paused());
        client.settings.lock().unwrap().seed_after_download = true;
        client.action("local", "resume", "").await.unwrap();
        assert!(!handle.is_paused());
        client.settings.lock().unwrap().seed_after_download = false;
        wait("stop existing seeder after settings change", || {
            handle.is_paused()
        })
        .await;

        assert_eq!(
            std::fs::read(existing.join("payload.bin")).unwrap(),
            payload
        );
        serde_json::to_string(&client.snapshot("local")).unwrap();
        let snapshot = client.snapshot("local");
        let magnet = url::Url::parse(snapshot["tasks"][0]["magnet"].as_str().unwrap()).unwrap();
        assert!(magnet.query_pairs().any(|(key, value)| key == "xt"
            && value == format!("urn:btih:{}", handle.info_hash().as_string())));
        client.action("local", "recheck", "").await.unwrap();
        let handle = client.handle("local").unwrap();
        wait("recheck completion", || handle.stats().finished).await;
        let neighbor = existing.join("unrelated.txt");
        std::fs::write(&neighbor, b"keep me").unwrap();
        client
            .action("local", "remove", "incomplete")
            .await
            .unwrap();
        assert!(existing.join("payload.bin").exists());
        assert!(!existing.join("unselected.bin").exists());
        assert!(neighbor.exists());
        assert_eq!(client.snapshot("")["tasks"].as_array().unwrap().len(), 0);
        let id = client
            .add(
                source.display().to_string(),
                dest.display().to_string(),
                true,
                Some(vec![0]),
            )
            .await
            .unwrap();
        wait("new handle", || client.handle(&id).is_ok()).await;
        assert_eq!(client.handle(&id).unwrap().only_files(), Some(vec![0]));
        client
            .handle(&id)
            .unwrap()
            .wait_until_initialized()
            .await
            .unwrap();
        client.action(&id, "remove", "all").await.unwrap();
        assert!(!existing.join("payload.bin").exists());
        assert!(neighbor.exists());
        client.session.stop().await;
        seed.session.stop().await;
        tracker_server.abort();
    }

    #[test]
    fn unsafe_multifile_name_is_rejected() {
        let bytes = b"d4:infod5:filesle4:name2:..ee";
        assert!(output_path(bytes, Path::new("D:/downloads")).is_err());
    }
}
