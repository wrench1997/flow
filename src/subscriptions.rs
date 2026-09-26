//! Persistent subscription catalog. A failed refresh never replaces a good cache.
use crate::trackers::{normalize, now};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub enabled: bool,
    pub urls: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub refresh_hours: u64,
    pub health_minutes: u64,
    pub sources: Vec<Source>,
}
impl Default for Config {
    fn default() -> Self {
        Self { refresh_hours: 24, health_minutes: 30, sources: vec![
            Source { name: "XIU2".into(), enabled: true, urls: vec![
                "https://cf.trackerslist.com/best.txt".into(),
                "https://raw.githubusercontent.com/XIU2/TrackersListCollection/master/best.txt".into()] },
            Source { name: "ngosang".into(), enabled: true, urls: vec![
                "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt".into(),
                "https://ngosang.github.io/trackerslist/trackers_best.txt".into()] },
            Source { name: "newTrackon".into(), enabled: true, urls: vec![
                "https://newtrackon.com/api/stable".into()] },
            Source { name: "animeTrackerList".into(), enabled: true, urls: vec![
                "https://cdn.jsdelivr.net/gh/DeSireFire/animeTrackerList@master/ATline_best.txt".into(),
                "https://raw.githubusercontent.com/DeSireFire/animeTrackerList/master/ATline_best.txt".into()] },
            Source { name: "OpenTracker".into(), enabled: true, urls: vec![
                "https://raw.githubusercontent.com/1265578519/OpenTracker/master/tracker.txt".into(),
                "https://cdn.jsdelivr.net/gh/1265578519/OpenTracker@master/tracker.txt".into()] },
        ] }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=168).contains(&self.refresh_hours),
            "更新间隔应为 1–168 小时"
        );
        anyhow::ensure!(
            (5..=1440).contains(&self.health_minutes),
            "健康检查间隔应为 5–1440 分钟"
        );
        anyhow::ensure!(self.sources.len() <= 16, "最多 16 个订阅源");
        let mut names = std::collections::HashSet::new();
        for s in &self.sources {
            anyhow::ensure!(
                !s.name.trim().is_empty() && names.insert(s.name.clone()),
                "订阅名称不能为空或重复"
            );
            anyhow::ensure!(
                !s.urls.is_empty() && s.urls.len() <= 5,
                "每个订阅应包含 1–5 个镜像地址"
            );
            for raw in &s.urls {
                let u = url::Url::parse(raw)?;
                anyhow::ensure!(
                    matches!(u.scheme(), "https" | "http")
                        && u.host_str().is_some()
                        && u.username().is_empty()
                        && u.password().is_none(),
                    "订阅只支持无内嵌账号的 HTTP/HTTPS 地址"
                );
            }
        }
        Ok(())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Cache {
    urls: Vec<String>,
    updated: u64,
    retry_at: u64,
    failures: u32,
    message: String,
}
pub struct Catalog {
    path: PathBuf,
    pub config: Mutex<Config>,
    cache: Mutex<BTreeMap<String, Cache>>,
    gate: tokio::sync::Mutex<()>,
}
fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}
impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        let file = path.join("tracker-sources.json");
        let mut config: Config = if file.exists() {
            serde_json::from_slice(&std::fs::read(&file)?)
                .context("tracker-sources.json 格式错误")?
        } else {
            Config::default()
        };
        config.validate()?;
        // One-time upgrade: preserve custom sources and disabled entries. Deletions after
        // this migration are intentional and must not be undone on the next launch.
        let migration = path.join("tracker-defaults-v040.json");
        if !migration.exists() {
            for source in Config::default().sources.into_iter().skip(2) {
                if config.sources.len() < 16
                    && !config.sources.iter().any(|existing| {
                        existing.name.eq_ignore_ascii_case(&source.name)
                            || existing.urls.iter().any(|url| source.urls.contains(url))
                    })
                {
                    config.sources.push(source);
                }
            }
        }
        write(&file, &config)?;
        if !migration.exists() {
            write(&migration, &true)?;
        }
        let cache = std::fs::read(path.join("tracker-cache.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self {
            path: path.into(),
            config: Mutex::new(config),
            cache: Mutex::new(cache),
            gate: tokio::sync::Mutex::new(()),
        })
    }
    pub async fn save(&self, config: Config) -> Result<()> {
        config.validate()?;
        let _gate = self.gate.lock().await;
        write(&self.path.join("tracker-sources.json"), &config)?;
        *self.config.lock().unwrap() = config;
        Ok(())
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"config":*self.config.lock().unwrap(), "cache":*self.cache.lock().unwrap()})
    }
    pub async fn candidates(
        &self,
        client: &reqwest::Client,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let _gate = self.gate.lock().await;
        let config = self.config.lock().unwrap().clone();
        let mut all = BTreeMap::new();
        let mut notes = Vec::new();
        let mut jobs = tokio::task::JoinSet::new();
        for source in config.sources.into_iter().filter(|s| s.enabled) {
            // A source's mirrors form part of its cache identity.
            let key = format!("{}|{}", source.name, source.urls.join("|"));
            let cache = self
                .cache
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let client = client.clone();
            let refresh_hours = config.refresh_hours;
            jobs.spawn(async move {
                let cache = refresh_source(&client, &source, cache, refresh_hours).await;
                (key, source.name, cache)
            });
        }
        while let Some(result) = jobs.join_next().await {
            match result {
                Ok((key, name, cache)) => {
                    notes.push(cache.message.clone());
                    for tracker in &cache.urls {
                        // Stable attribution even when requests finish in a different order.
                        let owner = all.entry(tracker.clone()).or_insert_with(|| name.clone());
                        if name < *owner {
                            *owner = name.clone();
                        }
                    }
                    self.cache.lock().unwrap().insert(key, cache);
                }
                Err(error) => notes.push(format!("订阅刷新失败：{error}")),
            }
        }
        notes.sort();
        if let Err(e) = write(
            &self.path.join("tracker-cache.json"),
            &*self.cache.lock().unwrap(),
        ) {
            notes.push(format!("缓存保存失败：{e}"));
        }
        (all, notes)
    }
}
async fn refresh_source(
    client: &reqwest::Client,
    source: &Source,
    mut cache: Cache,
    refresh_hours: u64,
) -> Cache {
    if now() < cache.retry_at
        || (cache.updated != 0 && now().saturating_sub(cache.updated) < refresh_hours * 3600)
    {
        return cache;
    }
    // All sources run independently; a complete mirror chain has a bounded budget.
    let result = tokio::time::timeout(std::time::Duration::from_secs(12), async {
        let mut failures = Vec::new();
        for endpoint in &source.urls {
            match fetch(client, endpoint).await {
                Ok(urls) => return Ok((urls, endpoint.clone())),
                Err(error) => failures.push(format!("{endpoint}: {error}")),
            }
        }
        Err(failures.join("；"))
    })
    .await;
    match result {
        Ok(Ok((urls, endpoint))) => {
            cache.urls = urls;
            cache.updated = now();
            cache.failures = 0;
            cache.retry_at = 0;
            cache.message = format!("{}：已更新（{}）", source.name, endpoint);
        }
        failure => {
            let reason = match failure {
                Ok(Err(reason)) => reason,
                _ => "订阅刷新超过 12 秒".into(),
            };
            cache.failures = cache.failures.saturating_add(1);
            cache.retry_at = now() + (300u64 * 2u64.pow(cache.failures.min(7))).min(21600);
            cache.message = format!(
                "{}：刷新失败，{}；稍后重试。{}",
                source.name,
                if cache.urls.is_empty() {
                    "暂无缓存"
                } else {
                    "沿用上次成功缓存"
                },
                reason
            );
        }
    }
    cache
}

async fn fetch(client: &reqwest::Client, endpoint: &str) -> Result<Vec<String>> {
    let mut r = client
        .get(endpoint)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        if bytes.len() + chunk.len() > 262144 {
            bail!("列表超过 256 KiB");
        }
        bytes.extend(chunk);
    }
    let urls: std::collections::BTreeSet<_> = String::from_utf8(bytes)?
        .lines()
        .filter_map(normalize)
        .take(500)
        .collect();
    anyhow::ensure!(!urls.is_empty(), "列表为空或格式无效，保留原缓存");
    Ok(urls.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upgrade_preserves_settings_and_does_not_readd_deleted_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.refresh_hours = 48;
        config
            .sources
            .retain(|s| s.name == "XIU2" || s.name == "newTrackon");
        config.sources[1].enabled = false;
        config.sources.push(Source {
            name: "custom".into(),
            enabled: true,
            urls: vec!["https://example.com/list.txt".into()],
        });
        write(&dir.path().join("tracker-sources.json"), &config).unwrap();
        let catalog = Catalog::open(dir.path()).unwrap();
        let mut migrated = catalog.config.lock().unwrap().clone();
        assert_eq!(migrated.refresh_hours, 48);
        assert_eq!(migrated.sources.len(), 5);
        assert!(
            !migrated
                .sources
                .iter()
                .find(|s| s.name == "newTrackon")
                .unwrap()
                .enabled
        );
        assert!(!migrated.sources.iter().any(|s| s.name == "ngosang"));
        migrated.sources.retain(|s| s.name != "OpenTracker");
        write(&dir.path().join("tracker-sources.json"), &migrated).unwrap();
        drop(catalog);
        let reopened = Catalog::open(dir.path()).unwrap();
        assert_eq!(reopened.config.lock().unwrap().sources.len(), 4);
    }
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    #[tokio::test]
    async fn sources_refresh_concurrently_and_deduplicate() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/list", listener.local_addr().unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let app = axum::Router::new().route(
            "/list",
            axum::routing::get(move || {
                let barrier = barrier.clone();
                async move {
                    barrier.wait().await;
                    "udp://tracker.example:80/announce\nudp://tracker.example:80/announce"
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path()).unwrap();
        catalog
            .save(Config {
                sources: ["B", "A"]
                    .into_iter()
                    .map(|name| Source {
                        name: name.into(),
                        enabled: true,
                        urls: vec![endpoint.clone()],
                    })
                    .collect(),
                ..Config::default()
            })
            .await
            .unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (found, _) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            catalog.candidates(&client),
        )
        .await
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found.values().next().unwrap(), "A");
        server.abort();
    }
    #[tokio::test]
    async fn mirror_fallback_cache_survives_failure_restart_and_disable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let empty = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let flag = empty.clone();
        let count = calls.clone();
        let app = axum::Router::new()
            .route(
                "/bad",
                axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            )
            .route(
                "/mirror",
                axum::routing::get(move || {
                    let flag = flag.clone();
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        if flag.load(Ordering::SeqCst) {
                            "<html>expired</html>"
                        } else {
                            "udp://tracker.example:80/announce\nnot-a-url"
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path()).unwrap();
        let config = Config {
            sources: vec![Source {
                name: "test".into(),
                enabled: true,
                urls: vec![format!("{base}/bad"), format!("{base}/mirror")],
            }],
            ..Config::default()
        };
        catalog.save(config.clone()).await.unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (found, _) = catalog.candidates(&client).await;
        assert_eq!(found.len(), 1);
        catalog.candidates(&client).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fresh cache must avoid network"
        );
        empty.store(true, Ordering::SeqCst);
        for cached in catalog.cache.lock().unwrap().values_mut() {
            cached.updated = 1;
        }
        let (fallback, notes) = catalog.candidates(&client).await;
        assert_eq!(found, fallback);
        assert!(notes.join("").contains("沿用上次成功缓存"));
        drop(catalog);
        let reopened = Catalog::open(dir.path()).unwrap();
        assert_eq!(reopened.candidates(&client).await.0, found);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "failure cooldown survives restart"
        );
        let mut disabled = config;
        disabled.sources[0].enabled = false;
        reopened.save(disabled).await.unwrap();
        assert!(reopened.candidates(&client).await.0.is_empty());
        server.abort();
    }
}
