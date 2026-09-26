use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use url::Url;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn normalize(s: &str) -> Option<String> {
    let mut url = Url::parse(s.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "udp")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    if url.scheme() == "udp" && url.port().is_none() {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Metric {
    pub consecutive_failures: u32,
    pub retry_at: u64,
    pub successes: u64,
    pub failures: u64,
    pub updated: u64,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub latency_ms: Option<u64>,
    pub message: String,
    pub status: String,
    pub source: String,
}
impl Metric {
    pub fn score(&self) -> Option<f64> {
        if self.updated == 0
            || now().saturating_sub(self.updated) > 86400
            || self.successes + self.failures == 0
        {
            return None;
        }
        let reliability = self.successes as f64 / (self.successes + self.failures) as f64;
        let seeds = (1.0 + self.seeders.unwrap_or(0) as f64).ln() / 101.0f64.ln();
        let response = self
            .latency_ms
            .map(|ms| 1.0 / (1.0 + ms as f64 / 1000.0))
            .unwrap_or(0.0);
        Some(
            (1000.0 * (0.75 * reliability + 0.05 * seeds.min(1.0) + 0.20 * response)).round()
                / 10.0,
        )
    }
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(offset..offset + 4)
            .context("Tracker 返回数据不完整")?
            .try_into()?,
    ))
}

async fn udp_scrape(url: &Url, hash: [u8; 20]) -> Result<(u32, u32)> {
    let host = url.host_str().context("Tracker 地址缺少域名")?;
    let addr = tokio::net::lookup_host((host, url.port().context("缺少端口")?))
        .await?
        .find(|a| a.is_ipv4())
        .context("没有可用 IPv4 地址")?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(addr).await?;
    let tx = uuid::Uuid::new_v4();
    let transaction = &tx.as_bytes()[..4];
    let mut connect = Vec::from(0x41727101980u64.to_be_bytes());
    connect.extend(0u32.to_be_bytes());
    connect.extend(transaction);
    socket.send(&connect).await?;
    let mut response = [0u8; 2048];
    let n = socket.recv(&mut response).await?;
    if n < 16 || &response[4..8] != transaction || u32_at(&response[..n], 0)? != 0 {
        bail!("UDP connect 响应无效或被拒绝");
    }
    let mut query = Vec::from(&response[8..16]);
    query.extend(2u32.to_be_bytes());
    query.extend(transaction);
    query.extend(hash);
    socket.send(&query).await?;
    let n = socket.recv(&mut response).await?;
    if n < 8 || &response[4..8] != transaction {
        bail!("UDP scrape 事务不匹配");
    }
    if u32_at(&response[..n], 0)? == 3 {
        bail!("{}", String::from_utf8_lossy(&response[8..n]));
    }
    if u32_at(&response[..n], 0)? != 2 || n < 20 {
        bail!("UDP scrape 响应无效");
    }
    Ok((u32_at(&response[..n], 8)?, u32_at(&response[..n], 16)?))
}

async fn http_scrape(client: &reqwest::Client, url: &Url, hash: [u8; 20]) -> Result<(u32, u32)> {
    use serde_bencode::value::Value as B;
    let mut url = url.clone();
    let path = url.path().to_string();
    let (prefix, last) = path.rsplit_once('/').context("Tracker 无 scrape 路径")?;
    if !last.starts_with("announce") {
        bail!("不支持标准 scrape，仍可能可用于下载");
    }
    url.set_path(&format!(
        "{prefix}/{}",
        last.replacen("announce", "scrape", 1)
    ));
    let separator = if url.query().is_some() { '&' } else { '?' };
    let encoded = hash.iter().map(|b| format!("%{b:02X}")).collect::<String>();
    let mut r = client
        .get(format!("{url}{separator}info_hash={encoded}"))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(c) = r.chunk().await? {
        if bytes.len() + c.len() > 1048576 {
            bail!("Tracker 返回数据过大");
        }
        bytes.extend(c);
    }
    let B::Dict(root) = serde_bencode::from_bytes::<B>(&bytes)? else {
        bail!("无效 bencode 响应");
    };
    if let Some(B::Bytes(reason)) = root.get(b"failure reason".as_slice()) {
        bail!("{}", String::from_utf8_lossy(reason));
    }
    let Some(B::Dict(files)) = root.get(b"files".as_slice()) else {
        bail!("未提供本资源 scrape 数据");
    };
    let Some(B::Dict(item)) = files.get(hash.as_slice()) else {
        bail!("此 Tracker 未返回该哈希的数据");
    };
    let read = |key: &[u8]| -> Result<u32> {
        match item.get(key) {
            Some(B::Int(v)) => Ok((*v).try_into()?),
            _ => bail!("缺少做种统计"),
        }
    };
    Ok((read(b"complete")?, read(b"incomplete")?))
}

pub async fn probe(client: &reqwest::Client, url: &str, hash: [u8; 20], old: Metric) -> Metric {
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        let parsed = Url::parse(url)?;
        if parsed.scheme() == "udp" {
            udp_scrape(&parsed, hash).await
        } else {
            http_scrape(client, &parsed, hash).await
        }
    })
    .await;
    let mut metric = old;
    metric.updated = now();
    metric.latency_ms = Some(started.elapsed().as_millis() as u64);
    match result {
        Ok(Ok((seeds, peers))) => {
            metric.consecutive_failures = 0;
            metric.retry_at = 0;
            metric.successes += 1;
            metric.seeders = Some(seeds);
            metric.leechers = Some(peers);
            metric.status = "响应正常".into();
            metric.message = format!("报告 {seeds} 个做种者、{peers} 个下载者（非已连接人数）");
        }
        other => {
            metric.consecutive_failures = metric.consecutive_failures.saturating_add(1);
            metric.retry_at =
                now() + (300u64 * 2u64.pow(metric.consecutive_failures.min(7))).min(21600);
            metric.seeders = None;
            metric.leechers = None;
            metric.failures += 1;
            metric.status = "查询失败".into();
            metric.message = match other {
                Ok(Err(e)) => format!("{e:#}"),
                _ => "8 秒超时；不代表 announce 一定不可用".into(),
            };
        }
    }
    metric
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    #[test]
    fn score_never_invents_samples() {
        assert!(Metric::default().score().is_none());
        assert!(normalize("file:///etc/passwd").is_none());
        assert!(normalize("udp://a").is_none());
        assert!(normalize("https://a/announce").is_some());
    }
    #[tokio::test]
    async fn udp_scrape_checks_actual_response() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = server.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut b = [0u8; 128];
            let (_, peer) = server.recv_from(&mut b).await.unwrap();
            let mut r = Vec::from(0u32.to_be_bytes());
            r.extend(&b[12..16]);
            r.extend(123u64.to_be_bytes());
            server.send_to(&r, peer).await.unwrap();
            let (_, peer) = server.recv_from(&mut b).await.unwrap();
            assert_eq!(&b[16..36], &[1u8; 20]);
            let mut r = Vec::from(2u32.to_be_bytes());
            r.extend(&b[12..16]);
            r.extend(25u32.to_be_bytes());
            r.extend(80u32.to_be_bytes());
            r.extend(6u32.to_be_bytes());
            server.send_to(&r, peer).await.unwrap();
        });
        let result = probe(
            &reqwest::Client::new(),
            &format!("udp://{addr}/announce"),
            [1; 20],
            Metric::default(),
        )
        .await;
        task.await.unwrap();
        assert_eq!(result.seeders, Some(25));
        assert_eq!(result.leechers, Some(6));
        assert!(result.score().unwrap() > 0.0);
    }
}
