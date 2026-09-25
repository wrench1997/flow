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
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, ManagedTorrent, Session,
    SessionOptions, SessionPersistenceConfig, api::TorrentIdOrHash,
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
}

pub struct Engine {
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
        let session = Session::new_with_opts(root.join("downloads"), opts).await?;
        let engine = Arc::new(Self {
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
            offline,
        });
        engine.persist()?;
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
        let entries = self.entries.lock().unwrap().clone();
        for entry in entries {
            if let Err(e) = self.load(&entry.id).await {
                self.error(&entry.id, &format!("{e:#}"));
            }
        }
    }

    async fn load(self: &Arc<Self>, id: &str) -> Result<()> {
        let _operation = self.operations.lock().await;
        let entry = self.entry(id)?;
        let metadata_file = self.data.join(format!("{id}.torrent"));
        let mut seen_peers = entry.initial_peers.clone();
        let bytes = if metadata_file.exists() {
            std::fs::read(&metadata_file)?
        } else if entry.source.starts_with("magnet:?") {
            self.event(id, "info", "metadata", "正在解析磁力链接元数据");
            let mut magnet = url::Url::parse(&entry.source)?;
            for tr in &entry.trackers {
                magnet.query_pairs_mut().append_pair("tr", tr);
            }
            let response = tokio::time::timeout(
                Duration::from_secs(90),
                self.session.add_torrent(
                    AddTorrent::from_url(magnet.to_string()),
                    Some(AddTorrentOptions {
                        list_only: true,
                        ..Default::default()
                    }),
                ),
            )
            .await
            .context("磁力解析超时，请重试或导入种子文件")??;
            match response {
                AddTorrentResponse::ListOnly(r) => {
                    seen_peers = r.seen_peers;
                    r.torrent_bytes.to_vec()
                }
                _ => bail!("这个磁力任务已存在"),
            }
        } else {
            std::fs::read(&entry.source)?
        };
        let bytes = metadata_with_trackers(&bytes, &entry.trackers)?;
        let output = entry
            .rust_output
            .as_ref()
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| output_path(&bytes, Path::new(&entry.save_path)))?;
        atomic(&metadata_file, &bytes)?;
        let opts = AddTorrentOptions {
            paused: entry.paused,
            overwrite: true,
            output_folder: Some(output.display().to_string()),
            trackers: Some(entry.trackers.clone()),
            initial_peers: Some(seen_peers),
            ..Default::default()
        };
        let response = self
            .session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
            .await?;
        let handle = response.into_handle().context("引擎未返回下载任务")?;
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
        }
        self.handles.lock().unwrap().insert(id.into(), handle);
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

    pub async fn add(self: &Arc<Self>, source: String, save_path: String) -> Result<String> {
        let source = source.trim().to_string();
        let save = PathBuf::from(save_path.trim());
        if source.is_empty() || !save.is_absolute() {
            bail!("请填写磁力链接或种子文件，以及绝对保存路径");
        }
        if !source.starts_with("magnet:?") {
            let p = Path::new(&source);
            if p.extension().and_then(|v| v.to_str()) != Some("torrent")
                || std::fs::metadata(p)?.len() > 20_000_000
            {
                bail!("请选择小于 20 MB 的 .torrent 文件");
            }
            output_path(&std::fs::read(p)?, &save)?;
        } else {
            url::Url::parse(&source)?;
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
        self.entry(id)?;
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
        if action == "resume" && self.handle(id).is_err() {
            self.load(id).await?;
        }
        let _operation = self.operations.lock().await;
        let handle = self.handle(id).ok();
        match action {
            "pause" | "resume" => {
                let h = handle.context("任务正在解析，请等待后重试")?;
                if action == "pause" {
                    if !h.is_paused() {
                        self.session.pause(&h).await?;
                    }
                } else {
                    if h.is_paused() {
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

    pub fn snapshot(&self, selected: &str) -> Value {
        let entries = self.entries.lock().unwrap().clone();
        let handles = self.handles.lock().unwrap().clone();
        let mut tasks = Vec::new();
        let mut detail = json!({"files":[],"peers":[],"trackers":[],"discovery":self.discovery.lock().unwrap().get(selected).cloned().unwrap_or(json!({}))});
        for e in entries {
            let mut item = json!({"id":e.id,"name":e.source,"save_path":e.save_path,"paused":e.paused,"error":e.error,"progress":0,"download_rate":0,"upload_rate":0,"done":0,"total":0,"peers":0,"seeds":null,"availability":-1,"state":"loading","diagnosis":if e.error.is_empty(){"正在解析元数据或载入任务"}else{&e.error}});
            item["source"] = json!(e.source);
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
                let ratio = if stats.total_bytes == 0 {
                    0.0
                } else {
                    stats.progress_bytes as f64 / stats.total_bytes as f64
                };
                let diagnosis = if let Some(error) = stats.error.as_ref() {
                    error.clone()
                } else if initializing {
                    format!("校验已有文件 {:.1}%（不是下载完成度）", ratio * 100.0)
                } else if e.paused {
                    "任务已暂停".into()
                } else if stats.finished {
                    "下载完成，正在做种".into()
                } else if down > 0 {
                    "正在接收数据".into()
                } else if live_peers == 0 {
                    "暂无对等连接，检查 Tracker 和资源活跃度".into()
                } else {
                    "已连接，等待可用数据".into()
                };
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
                        detail["files"]=json!(m.file_infos.iter().enumerate().map(|(i,f)|json!({"path":f.relative_filename.display().to_string(),"size":f.len,"done":stats.file_progress.get(i).copied().unwrap_or(0)})).collect::<Vec<_>>());
                    }
                    if let Ok(p) = Api::new(self.session.clone(), None)
                        .api_peer_stats(TorrentIdOrHash::Id(h.id()), Default::default())
                    {
                        detail["peers"]=json!(p.peers.into_iter().map(|(addr,p)|json!({"address":addr,"client":p.client_name.unwrap_or_default(),"downloaded":p.counters.fetched_bytes,"errors":p.counters.errors,"state":p.state})).collect::<Vec<_>>());
                    }
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
                        row["score"] = json!(metric.score());
                        row
                    })
                    .collect::<Vec<_>>();
                rows.sort_by(|a, b| {
                    b["score"]
                        .as_f64()
                        .unwrap_or(-1.0)
                        .total_cmp(&a["score"].as_f64().unwrap_or(-1.0))
                });
                detail["trackers"] = json!(rows);
            }
            tasks.push(item);
        }
        json!({"tasks":tasks,"detail":detail,"settings":*self.settings.lock().unwrap(),"subscriptions":self.subscriptions.snapshot(),"events":*self.events.lock().unwrap(),"engine":"librqbit 9.0.1 · Rust","listen_port":self.session.announce_port().unwrap_or(0),"updated_at":trackers::now(),"discovery":*self.discovery.lock().unwrap()})
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
            )
            .await
            .map(|id| json!({"id":id})),
    )
}
async fn action(State(s): State<ApiState>, Json(v): Json<Value>) -> Response {
    reply(
        s.engine
            .action(
                v["id"].as_str().unwrap_or(""),
                v["action"].as_str().unwrap_or(""),
                v["trackers"].as_str().unwrap_or(""),
            )
            .await
            .map(|_| json!({"ok":true})),
    )
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

pub async fn serve(
    root: PathBuf,
    token: String,
    offline: bool,
    stop: oneshot::Receiver<()>,
    ready: mpsc::SyncSender<std::result::Result<String, String>>,
) -> Result<()> {
    std::fs::create_dir_all(root.join("data"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("data/engine.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("此项目已有下载引擎运行，请先关闭原窗口")?;
    let engine = Engine::new(&root, offline).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let state_data = ApiState {
        engine: engine.clone(),
        token,
    };
    let app = Router::new()
        .route("/api/state", get(state))
        .route("/api/tasks", post(add))
        .route("/api/action", post(action))
        .route("/api/subscriptions", post(subscriptions))
        .route("/api/settings", post(settings))
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
    let snapshot_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = atomic(
                &snapshots.data.join("status.json"),
                &serde_json::to_vec_pretty(&snapshots.snapshot("")).unwrap(),
            );
        }
    });
    let _ = ready.send(Ok(format!("http://{addr}")));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = stop.await;
        })
        .await?;
    restore_task.abort();
    snapshot_task.abort();
    maintenance_task.abort();
    engine.persist()?;
    engine.session.stop().await;
    drop(engine);
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn wait(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("engine condition timed out");
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
        let seed = Engine::new(&seed_root, true).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tracker_url = format!("http://{}/announce", listener.local_addr().unwrap());
        let port = seed.session.announce_port().unwrap();
        use serde_bencode::value::Value as B;
        let payload_response = serde_bencode::to_bytes(&B::Dict(
            [
                (b"interval".to_vec(), B::Int(60)),
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
        client.persist().unwrap();
        client.session.stop().await;
        drop(handle);
        drop(client);
        let client = Engine::new(&download_root, true).await.unwrap();
        client.restore().await;
        assert!(
            client.snapshot("local")["tasks"][0]["paused"]
                .as_bool()
                .unwrap()
        );
        client.action("local", "resume", "").await.unwrap();
        client.action("local", "resume", "").await.unwrap();
        client.action("local", "pause", "").await.unwrap();
        client.action("local", "pause", "").await.unwrap();
        assert!(client.handle("local").unwrap().is_paused());
        client.action("local", "resume", "").await.unwrap();
        let handle = client.handle("local").unwrap();
        wait(|| handle.stats().finished).await;
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
        wait(|| handle.stats().finished).await;
        client.action("local", "remove", "").await.unwrap();
        assert!(existing.join("payload.bin").exists());
        assert_eq!(client.snapshot("")["tasks"].as_array().unwrap().len(), 0);
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
