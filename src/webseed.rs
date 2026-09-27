//! BEP 19 source layout and verified HTTP piece fetching.
//! Does not write payload files: pieces must go through the torrent engine.
use anyhow::{Context, Result, bail, ensure};
use librqbit_sha1_wrapper::{ISha1, Sha1};
use serde_bencode::value::Value as B;
use std::time::Duration;
use url::Url;

const MAX_PIECE: u64 = 32 * 1024 * 1024;
const MAX_SOURCES: usize = 16;

#[derive(Clone, Debug)]
pub struct File {
    pub offset: u64,
    pub size: u64,
    pub parts: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct Layout {
    pub sources: Vec<Url>,
    pub name: String,
    pub files: Vec<File>,
    pub multi: bool,
    pub total: u64,
    pub piece_length: u64,
    pub hashes: Vec<[u8; 20]>,
}
#[derive(Debug)]
pub struct Span {
    pub url: Url,
    pub start: u64,
    pub length: u64,
    pub file_size: u64,
}
fn bytes(value: &B) -> Result<&[u8]> {
    match value {
        B::Bytes(bytes) => Ok(bytes),
        _ => bail!("WebSeed 元数据字段类型错误"),
    }
}
fn part(value: &B) -> Result<String> {
    let text = std::str::from_utf8(bytes(value)?)?;
    ensure!(
        !text.is_empty() && text != "." && text != ".." && !text.contains(['/', '\\', '\0']),
        "WebSeed 文件路径无效"
    );
    Ok(text.to_owned())
}
fn number(value: Option<&B>) -> Result<u64> {
    match value {
        Some(B::Int(value)) if *value >= 0 => Ok(*value as u64),
        _ => bail!("WebSeed 文件长度无效"),
    }
}
impl Layout {
    pub fn parse(torrent: &[u8], magnet: Option<&str>) -> Result<Option<Self>> {
        // Keep the original info bytes/hash untouched; this decoder is for layout only.
        librqbit::torrent_from_bytes(torrent)?;
        let B::Dict(root) = serde_bencode::from_bytes::<B>(torrent)? else {
            bail!("无效种子")
        };
        let mut raw = Vec::new();
        if let Some(value) = root.get(b"url-list".as_slice()) {
            match value {
                B::Bytes(value) => raw.push(std::str::from_utf8(value)?.to_owned()),
                B::List(values) => {
                    for value in values {
                        raw.push(std::str::from_utf8(bytes(value)?)?.to_owned());
                    }
                }
                _ => bail!("WebSeed url-list 格式无效"),
            }
        }
        if let Some(magnet) = magnet
            .and_then(|s| Url::parse(s).ok())
            .filter(|u| u.scheme() == "magnet")
        {
            raw.extend(
                magnet
                    .query_pairs()
                    .filter(|(key, _)| key == "ws")
                    .map(|(_, value)| value.into_owned()),
            );
        }
        let mut sources = Vec::new();
        for value in raw {
            let Ok(mut url) = Url::parse(&value) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                continue;
            }
            url.set_fragment(None);
            if !sources.contains(&url) {
                sources.push(url);
            }
            if sources.len() == MAX_SOURCES {
                break;
            }
        }
        if sources.is_empty() {
            return Ok(None);
        }
        let Some(B::Dict(info)) = root.get(b"info".as_slice()) else {
            bail!("缺少 info")
        };
        let name = part(
            info.get(b"name.utf-8".as_slice())
                .or_else(|| info.get(b"name".as_slice()))
                .context("缺少名称")?,
        )?;
        let piece_length = number(info.get(b"piece length".as_slice()))?;
        ensure!(
            (1..=MAX_PIECE).contains(&piece_length),
            "WebSeed 分片长度超出支持范围（最多 32 MiB）"
        );
        let mut files = Vec::new();
        let mut total = 0u64;
        let multi = info.contains_key(b"files".as_slice());
        if let Some(B::List(rows)) = info.get(b"files".as_slice()) {
            for row in rows {
                let B::Dict(row) = row else {
                    bail!("无效文件列表")
                };
                let size = number(row.get(b"length".as_slice()))?;
                let Some(B::List(parts)) = row
                    .get(b"path.utf-8".as_slice())
                    .or_else(|| row.get(b"path".as_slice()))
                else {
                    bail!("缺少文件路径")
                };
                ensure!(!parts.is_empty(), "文件路径为空");
                let parts = parts.iter().map(part).collect::<Result<Vec<_>>>()?;
                files.push(File {
                    offset: total,
                    size,
                    parts,
                });
                total = total.checked_add(size).context("文件总长度溢出")?;
            }
        } else {
            total = number(info.get(b"length".as_slice()))?;
            files.push(File {
                offset: 0,
                size: total,
                parts: Vec::new(),
            });
        }
        let pieces = bytes(info.get(b"pieces".as_slice()).context("缺少分片哈希")?)?;
        ensure!(
            pieces.len() % 20 == 0 && total.div_ceil(piece_length) == (pieces.len() / 20) as u64,
            "分片数量不匹配"
        );
        let hashes = pieces
            .chunks_exact(20)
            .map(|hash| hash.try_into().unwrap())
            .collect();
        Ok(Some(Self {
            sources,
            name,
            files,
            multi,
            total,
            piece_length,
            hashes,
        }))
    }
    pub fn spans(&self, source: &Url, index: usize) -> Result<Vec<Span>> {
        ensure!(index < self.hashes.len(), "无效分片索引");
        let start = index as u64 * self.piece_length;
        let end = (start + self.piece_length).min(self.total);
        let mut spans = Vec::new();
        for file in &self.files {
            let begin = start.max(file.offset);
            let finish = end.min(file.offset + file.size);
            if begin >= finish {
                continue;
            }
            let mut url = source.clone();
            if self.multi || url.path().ends_with('/') {
                let mut path = url
                    .path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("无效 HTTP 路径"))?;
                path.pop_if_empty().push(&self.name);
                for part in &file.parts {
                    path.push(part);
                }
            }
            spans.push(Span {
                url,
                start: begin - file.offset,
                length: finish - begin,
                file_size: file.size,
            });
        }
        ensure!(
            spans.iter().map(|s| s.length).sum::<u64>() == end - start,
            "分片文件映射不完整"
        );
        Ok(spans)
    }
    pub async fn fetch_piece(
        &self,
        client: &reqwest::Client,
        source: &Url,
        index: usize,
    ) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        for span in self.spans(source, index)? {
            let end = span.start + span.length - 1;
            let mut response = client
                .get(span.url)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .header(
                    reqwest::header::RANGE,
                    format!("bytes={}-{}", span.start, end),
                )
                .send()
                .await
                .context("WebSeed HTTP 请求失败")?;
            let headers = response.headers();
            ensure!(
                headers
                    .get(reqwest::header::CONTENT_ENCODING)
                    .is_none_or(|v| v == "identity"),
                "WebSeed 返回压缩内容，不能对应分片偏移"
            );
            match response.status() {
                reqwest::StatusCode::PARTIAL_CONTENT => {
                    let expected = format!("bytes {}-{}/{}", span.start, end, span.file_size);
                    ensure!(
                        headers
                            .get(reqwest::header::CONTENT_RANGE)
                            .and_then(|v| v.to_str().ok())
                            == Some(expected.as_str()),
                        "WebSeed Content-Range 与请求不匹配"
                    );
                }
                reqwest::StatusCode::OK if span.start == 0 && span.length == span.file_size => {}
                status => bail!("WebSeed HTTP 状态 {status}，来源未提供所需范围"),
            }
            let before = data.len();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    (data.len() - before) as u64 + chunk.len() as u64 <= span.length,
                    "WebSeed 响应超出所需范围"
                );
                data.extend_from_slice(&chunk);
            }
            ensure!(
                (data.len() - before) as u64 == span.length,
                "WebSeed 响应内容不完整"
            );
        }
        let mut hash = Sha1::new();
        hash.update(&data);
        ensure!(
            hash.finish() == self.hashes[index],
            "WebSeed 分片哈希不匹配，来源提供了错误数据"
        );
        Ok(data)
    }
}
pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("WebSeed 重定向过多");
            }
            if !matches!(attempt.url().scheme(), "http" | "https")
                || attempt
                    .previous()
                    .last()
                    .is_some_and(|u| u.scheme() == "https" && attempt.url().scheme() == "http")
            {
                return attempt.error("WebSeed 重定向协议不允许");
            }
            attempt.follow()
        }))
        .build()?)
}

// A loopback adapter lets librqbit own piece selection, disk writes, hashing,
// resume state and bandwidth control. This is never exposed as an Internet seed.
// Its advertised remote ranges are candidates, not measured swarm availability.
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct SourceState {
    active_requests: u32,
    failures: u32,
    rejected: bool,
    retry_at: u64,
    verified_bytes: u64,
    verified_pieces: u64,
    error: String,
}
struct ActiveRequest<'a> {
    states: &'a Mutex<Vec<SourceState>>,
    index: usize,
}
impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        let mut states = self.states.lock().unwrap();
        states[self.index].active_requests = states[self.index].active_requests.saturating_sub(1);
    }
}
struct Adapter {
    layout: Layout,
    hash: [u8; 20],
    peer_id: [u8; 20],
    client: reqwest::Client,
    states: Mutex<Vec<SourceState>>,
    enabled: watch::Sender<bool>,
    stop: CancellationToken,
}
pub struct Bridge {
    pub address: SocketAddr,
    adapter: Arc<Adapter>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.adapter.stop.cancel();
    }
}
impl Bridge {
    pub async fn start(layout: Layout, hash: [u8; 20], enabled: bool) -> Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (sender, _) = watch::channel(enabled);
        let mut peer_id = *b"-FLWS01-000000000000";
        peer_id[8..].copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..12]);
        let adapter = Arc::new(Adapter {
            states: Mutex::new(
                (0..layout.sources.len())
                    .map(|_| SourceState::default())
                    .collect(),
            ),
            layout,
            hash,
            peer_id,
            client: client()?,
            enabled: sender,
            stop: CancellationToken::new(),
        });
        let bridge = Arc::new(Self {
            address,
            adapter: adapter.clone(),
        });
        tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _=adapter.stop.cancelled()=>break,
                    Some(_)=connections.join_next(), if !connections.is_empty()=>{},
                    result=listener.accept()=>{
                        let Ok((socket,remote))=result else { break };
                        if !remote.ip().is_loopback() || !*adapter.enabled.borrow() || connections.len()>=2 { continue; }
                        let adapter=adapter.clone();
                        connections.spawn(async move {
                            let mut enabled=adapter.enabled.subscribe();
                            tokio::select! {
                                _=adapter.stop.cancelled()=>{},
                                _=async { loop { if !*enabled.borrow_and_update() { break } if enabled.changed().await.is_err() { break } } }=>{},
                                _=adapter.connection(socket)=>{},
                            }
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(bridge)
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.adapter.enabled.send_if_modified(|old| {
            if *old == enabled {
                false
            } else {
                *old = enabled;
                true
            }
        });
    }
    pub fn snapshot(&self) -> Value {
        let states = self.adapter.states.lock().unwrap();
        json!(self.adapter.layout.sources.iter().zip(states.iter()).map(|(source,state)|{
            let mut display=source.clone();display.set_query(None);display.set_fragment(None);
            json!({"url":display.as_str(),"verified_bytes":state.verified_bytes,"verified_pieces":state.verified_pieces,
                "failures":state.failures,"active_requests":state.active_requests,"rejected":state.rejected,"retry_at":state.retry_at,"error":state.error,
                "state":if state.active_requests>0 {"requesting"} else if state.rejected {"rejected"} else if state.retry_at>crate::trackers::now(){"backoff"} else if state.verified_pieces>0{"verified_transfer"}else{"unverified"}})
        }).collect::<Vec<_>>())
    }
}
impl Adapter {
    async fn piece(&self, index: usize) -> Result<Vec<u8>> {
        let now = crate::trackers::now();
        for (i, source) in self.layout.sources.iter().enumerate() {
            {
                let mut states = self.states.lock().unwrap();
                if states[i].rejected || states[i].retry_at > now {
                    continue;
                }
                states[i].active_requests += 1;
            }
            let active = ActiveRequest {
                states: &self.states,
                index: i,
            };
            let result = self.layout.fetch_piece(&self.client, source, index).await;
            drop(active);
            match result {
                Ok(data) => {
                    let mut states = self.states.lock().unwrap();
                    states[i].verified_bytes += data.len() as u64;
                    states[i].verified_pieces += 1;
                    states[i].retry_at = 0;
                    states[i].error.clear();
                    return Ok(data);
                }
                Err(error) => {
                    let message = error.to_string();
                    let mut states = self.states.lock().unwrap();
                    let state = &mut states[i];
                    state.failures += 1;
                    state.rejected = message.contains("哈希不匹配")
                        || message.contains("Content-Range")
                        || message.contains("压缩内容");
                    state.retry_at =
                        now + [15, 45, 180][(state.failures.saturating_sub(1) as usize).min(2)];
                    state.error = message;
                }
            }
        }
        bail!("WebSeed 暂无可用来源；错误来源已隔离，临时失败稍后重试")
    }
    async fn connection(&self, mut socket: TcpStream) -> Result<()> {
        socket.set_nodelay(true)?;
        let mut handshake = [0u8; 68];
        tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut handshake)).await??;
        ensure!(
            handshake[..20] == *b"\x13BitTorrent protocol" && handshake[28..48] == self.hash,
            "WebSeed 本机协议握手无效"
        );
        handshake[20..28].fill(0);
        handshake[48..68].copy_from_slice(&self.peer_id);
        socket.write_all(&handshake).await?;
        let count = self.layout.hashes.len();
        let mut bitfield = vec![0xff; count.div_ceil(8)];
        if count % 8 != 0 {
            if let Some(last) = bitfield.last_mut() {
                *last <<= 8 - count % 8;
            }
        }
        socket.write_u32((1 + bitfield.len()) as u32).await?;
        socket.write_u8(5).await?;
        socket.write_all(&bitfield).await?;
        socket.write_u32(1).await?;
        socket.write_u8(1).await?;
        let (mut reader, mut writer) = socket.into_split();
        let (requests, mut pending) = mpsc::channel::<(usize, usize, usize)>(64);
        let cancelled = Arc::new(Mutex::new(HashSet::new()));
        let read = async {
            loop {
                let length = reader.read_u32().await? as usize;
                ensure!(length <= 65536, "WebSeed 本机消息过长");
                if length == 0 {
                    continue;
                }
                let mut message = vec![0; length];
                reader.read_exact(&mut message).await?;
                match message[0] {
                    6 | 8 => {
                        ensure!(length == 13, "无效分片请求");
                        let index = u32::from_be_bytes(message[1..5].try_into().unwrap()) as usize;
                        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
                        let size = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                        ensure!(
                            index < count && (1..=16384).contains(&size),
                            "无效分片请求范围"
                        );
                        let piece_size =
                            (self.layout.total - index as u64 * self.layout.piece_length)
                                .min(self.layout.piece_length) as usize;
                        ensure!(
                            begin <= piece_size && size <= piece_size - begin,
                            "请求超过分片边界"
                        );
                        let key = (index, begin, size);
                        if message[0] == 8 {
                            let mut entries = cancelled.lock().unwrap();
                            if entries.len() < 128 {
                                entries.insert(key);
                            }
                        } else {
                            cancelled.lock().unwrap().remove(&key);
                            if requests.send(key).await.is_err() {
                                break;
                            }
                        }
                    }
                    0..=5 | 7 | 9 | 20 => {}
                    _ => bail!("不支持的本机消息"),
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let write = async {
            let mut cache: VecDeque<(usize, Vec<u8>)> = VecDeque::new();
            while let Some(key) = pending.recv().await {
                if cancelled.lock().unwrap().remove(&key) {
                    continue;
                }
                let (index, begin, size) = key;
                if !cache.iter().any(|(i, _)| *i == index) {
                    // HTTP may take longer than the engine's peer read timeout.
                    // Keep the local transport alive without claiming payload progress.
                    let fetch = self.piece(index);
                    tokio::pin!(fetch);
                    let data = loop {
                        tokio::select! {
                            result = &mut fetch => break result?,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => writer.write_u32(0).await?,
                        }
                    };
                    cache.push_back((index, data));
                    while cache.len() > 2 {
                        cache.pop_front();
                    }
                }
                if cancelled.lock().unwrap().remove(&key) {
                    continue;
                }
                let data = &cache.iter().find(|(i, _)| *i == index).unwrap().1;
                writer.write_u32((9 + size) as u32).await?;
                writer.write_u8(7).await?;
                writer.write_u32(index as u32).await?;
                writer.write_u32(begin as u32).await?;
                writer.write_all(&data[begin..begin + size]).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! { result=read=>result, result=write=>result }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn torrent(data: &[u8], piece_length: usize, source: &str) -> Vec<u8> {
        let mut hashes = Vec::new();
        for piece in data.chunks(piece_length) {
            let mut h = Sha1::new();
            h.update(piece);
            hashes.extend_from_slice(&h.finish());
        }
        let info = B::Dict(
            [
                (b"name".to_vec(), B::Bytes(b"fixture.bin".to_vec())),
                (b"length".to_vec(), B::Int(data.len() as i64)),
                (b"piece length".to_vec(), B::Int(piece_length as i64)),
                (b"pieces".to_vec(), B::Bytes(hashes)),
            ]
            .into_iter()
            .collect(),
        );
        serde_bencode::to_bytes(&B::Dict(
            [
                (b"info".to_vec(), info),
                (b"url-list".to_vec(), B::Bytes(source.as_bytes().to_vec())),
            ]
            .into_iter()
            .collect(),
        ))
        .unwrap()
    }
    #[test]
    fn source_urls_preserve_signed_queries_and_escape_filenames() {
        let data = vec![1; 20000];
        let bytes = torrent(&data, 16384, "https://example.com/download?signature=a%2Bb");
        let layout = Layout::parse(&bytes, None).unwrap().unwrap();
        let spans = layout.spans(&layout.sources[0], 1).unwrap();
        assert_eq!(spans[0].start, 16384);
        assert_eq!(spans[0].length, 3616);
        assert_eq!(
            spans[0].url.as_str(),
            "https://example.com/download?signature=a%2Bb"
        );
        assert!(
            Layout::parse(&torrent(&data, 16384, "file:///C:/secret"), None)
                .unwrap()
                .is_none()
        );
        let magnet = "magnet:?xt=urn:btih:unused&ws=https%3A%2F%2Fexample.com%2Froot%2F";
        let layout = Layout::parse(
            &torrent(&data, 16384, "ftp://example.com/file"),
            Some(magnet),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            layout.spans(&layout.sources[0], 0).unwrap()[0].url.as_str(),
            "https://example.com/root/fixture.bin"
        );
    }
    #[tokio::test]
    async fn http_ranges_deliver_verified_pieces_and_reject_wrong_offsets_and_content() {
        use axum::{
            Router,
            body::Body,
            http::{HeaderMap, StatusCode},
            response::Response,
            routing::get,
        };
        let data = std::sync::Arc::new((0..40000).map(|i| (i % 251) as u8).collect::<Vec<_>>());
        let expected = data.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/{mode}",
            get(
                move |headers: HeaderMap,
                      axum::extract::Path(mode): axum::extract::Path<String>| {
                    let data = data.clone();
                    async move {
                        let range = headers["range"]
                            .to_str()
                            .unwrap()
                            .strip_prefix("bytes=")
                            .unwrap();
                        let (start, end) = range.split_once('-').unwrap();
                        let start = start.parse::<usize>().unwrap();
                        let end = end.parse::<usize>().unwrap();
                        let mut body = data[start..=end].to_vec();
                        if mode == "corrupt" {
                            body[0] ^= 1;
                        }
                        let header = if mode == "offset" {
                            format!("bytes 0-{end}/{}", data.len())
                        } else {
                            format!("bytes {start}-{end}/{}", data.len())
                        };
                        Response::builder()
                            .status(StatusCode::PARTIAL_CONTENT)
                            .header("content-range", header)
                            .body(Body::from(body))
                            .unwrap()
                    }
                },
            ),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = client().unwrap();
        let bytes = torrent(&expected, 16384, &format!("http://{addr}/good"));
        let layout = Layout::parse(&bytes, None).unwrap().unwrap();
        assert_eq!(
            layout
                .fetch_piece(&client, &layout.sources[0], 1)
                .await
                .unwrap(),
            expected[16384..32768]
        );
        assert!(
            layout
                .fetch_piece(
                    &client,
                    &Url::parse(&format!("http://{addr}/offset")).unwrap(),
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("Content-Range")
        );
        assert!(
            layout
                .fetch_piece(
                    &client,
                    &Url::parse(&format!("http://{addr}/corrupt")).unwrap(),
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("哈希")
        );
        server.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_http_source_keeps_transport_alive_without_fake_progress() {
        use axum::{Router, body::Body, response::Response, routing::get};
        use std::sync::atomic::{AtomicU64, Ordering};
        let expected = vec![23u8; 16384];
        let payload = expected.clone();
        let requests = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/slow",
            get(move || {
                let payload = payload.clone();
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(12)).await;
                    Response::builder()
                        .status(206)
                        .header("content-range", "bytes 0-16383/16384")
                        .body(Body::from(payload))
                        .unwrap()
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("slow.torrent");
        std::fs::write(
            &path,
            torrent(&expected, 16384, &format!("http://{addr}/slow")),
        )
        .unwrap();
        let engine = crate::engine::Engine::new(root.path(), true).await.unwrap();
        let output = root.path().join("downloads");
        let id = engine
            .add(
                path.display().to_string(),
                output.display().to_string(),
                false,
                None,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while requests.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            engine.snapshot(&id)["tasks"][0]["done"],
            0,
            "Keepalive counted as payload"
        );
        assert_eq!(
            engine.snapshot(&id)["tasks"][0]["webseeds"][0]["state"],
            "requesting"
        );
        assert!(
            engine.snapshot(&id)["tasks"][0]["diagnosis"]
                .as_str()
                .unwrap()
                .contains("HTTP WebSeed 正在请求")
        );
        tokio::time::timeout(Duration::from_secs(25), async {
            while engine.snapshot(&id)["tasks"][0]["progress"]
                .as_f64()
                .unwrap_or(0.)
                < 1.
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "Slow source was needlessly reconnected"
        );
        assert_eq!(std::fs::read(output.join("fixture.bin")).unwrap(), expected);
        engine.session.stop().await;
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn webseed_only_torrent_pauses_resumes_and_finishes_through_engine() {
        use axum::{Router, body::Body, http::HeaderMap, response::Response, routing::get};
        use std::sync::atomic::{AtomicU64, Ordering};
        let requests = Arc::new(AtomicU64::new(0));
        let observed = requests.clone();
        let expected = Arc::new(
            (0..4 * 1024 * 1024usize)
                .map(|i| ((i * 37 + i / 4096) % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let payload = expected.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/fixture.bin",
            get(move |headers: HeaderMap| {
                let payload = payload.clone();
                let requests = requests.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(35)).await;
                    let (start, end) = headers["range"]
                        .to_str()
                        .unwrap()
                        .strip_prefix("bytes=")
                        .unwrap()
                        .split_once('-')
                        .unwrap();
                    let start = start.parse::<usize>().unwrap();
                    let end = end.parse::<usize>().unwrap();
                    Response::builder()
                        .status(206)
                        .header(
                            "content-range",
                            format!("bytes {start}-{end}/{}", payload.len()),
                        )
                        .body(Body::from(payload[start..=end].to_vec()))
                        .unwrap()
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let torrent_path = root.path().join("fixture.torrent");
        std::fs::write(
            &torrent_path,
            torrent(&expected, 65536, &format!("http://{addr}/fixture.bin")),
        )
        .unwrap();
        let engine = crate::engine::Engine::new(root.path(), true).await.unwrap();
        let downloads = root.path().join("result");
        std::fs::create_dir(&downloads).unwrap();
        let id = engine
            .add(
                torrent_path.display().to_string(),
                downloads.display().to_string(),
                false,
                None,
            )
            .await
            .unwrap();
        let task = || {
            engine.snapshot(&id)["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id)
                .unwrap()
                .clone()
        };
        tokio::time::timeout(Duration::from_secs(40), async {
            while task()["done"].as_u64().unwrap_or(0) < 65536 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("WebSeed produced no verified engine progress");
        assert_eq!(task()["availability"], -1);
        assert_eq!(
            task()["coverage"]["known_peers"],
            0,
            "HTTP bridge inflated BT coverage"
        );
        engine.action(&id, "pause", "").await.unwrap();
        let paused = task()["done"].as_u64().unwrap();
        assert!(paused < expected.len() as u64);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let paused_requests = observed.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(task()["done"].as_u64().unwrap(), paused);
        assert_eq!(
            task()["webseeds"][0]["active_requests"],
            0,
            "Cancelled request remained active"
        );
        assert_eq!(
            observed.load(Ordering::SeqCst),
            paused_requests,
            "HTTP requests continued after pause"
        );
        engine.action(&id, "resume", "").await.unwrap();
        tokio::time::timeout(Duration::from_secs(45), async {
            while task()["progress"].as_f64().unwrap_or(0.) < 1. {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("WebSeed did not resume/finish");
        assert_eq!(
            std::fs::read(downloads.join("fixture.bin")).unwrap(),
            *expected
        );
        let finished = task();
        assert_eq!(finished["peers"], 0);
        assert!(finished["webseeds"][0]["verified_bytes"].as_u64().unwrap() > 0);
        assert!(
            engine.snapshot(&id)["detail"]["peers"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        engine.action(&id, "remove", "all").await.unwrap();
        assert!(!downloads.join("fixture.bin").exists());
        engine.session.stop().await;
        server.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multifile_webseed_rejects_bad_source_and_restores_file_selection() {
        use axum::{Router, body::Body, http::HeaderMap, response::Response, routing::get};
        let names = ["prefix.bin", "clip #1.mkv", "neighbor.bin", "never.bin"];
        let sizes = [5000, 100000, 60000, 65000];
        let files = Arc::new(
            sizes
                .iter()
                .enumerate()
                .map(|(n, size)| {
                    (0..*size)
                        .map(|i| ((i * 37 + n * 11) % 251) as u8)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        );
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        let payload = files.clone();
        let logs = requests.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app =
            Router::new().route(
                "/{mode}/{*path}",
                get(
                    move |headers: HeaderMap,
                          axum::extract::Path((mode, path)): axum::extract::Path<(
                        String,
                        String,
                    )>| {
                        let files = payload.clone();
                        let logs = logs.clone();
                        async move {
                            logs.lock().unwrap().push(format!("{mode}/{path}"));
                            tokio::time::sleep(Duration::from_millis(45)).await;
                            let file = path.strip_prefix("collection/").unwrap();
                            let i = names.iter().position(|name| *name == file).unwrap();
                            let (start, end) = headers["range"]
                                .to_str()
                                .unwrap()
                                .strip_prefix("bytes=")
                                .unwrap()
                                .split_once('-')
                                .unwrap();
                            let start = start.parse::<usize>().unwrap();
                            let end = end.parse::<usize>().unwrap();
                            let mut data = files[i][start..=end].to_vec();
                            if mode == "bad" {
                                data[0] ^= 1;
                            }
                            Response::builder()
                                .status(206)
                                .header(
                                    "content-range",
                                    format!("bytes {start}-{end}/{}", files[i].len()),
                                )
                                .body(Body::from(data))
                                .unwrap()
                        }
                    },
                ),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let joined = files.iter().flatten().copied().collect::<Vec<_>>();
        let piece_length = 16384;
        let mut hashes = Vec::new();
        for piece in joined.chunks(piece_length) {
            let mut hash = Sha1::new();
            hash.update(piece);
            hashes.extend_from_slice(&hash.finish());
        }
        let rows = names
            .iter()
            .zip(sizes)
            .map(|(name, size)| {
                B::Dict(
                    [
                        (b"length".to_vec(), B::Int(size as i64)),
                        (
                            b"path".to_vec(),
                            B::List(vec![B::Bytes(name.as_bytes().to_vec())]),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect();
        let info = B::Dict(
            [
                (b"name".to_vec(), B::Bytes(b"collection".to_vec())),
                (b"files".to_vec(), B::List(rows)),
                (b"piece length".to_vec(), B::Int(piece_length as i64)),
                (b"pieces".to_vec(), B::Bytes(hashes)),
            ]
            .into_iter()
            .collect(),
        );
        let bytes = serde_bencode::to_bytes(&B::Dict(
            [
                (b"info".to_vec(), info),
                (
                    b"url-list".to_vec(),
                    B::List(vec![
                        B::Bytes(format!("http://{addr}/bad/").into_bytes()),
                        B::Bytes(format!("http://{addr}/good/").into_bytes()),
                    ]),
                ),
            ]
            .into_iter()
            .collect(),
        ))
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let torrent_path = root.path().join("multi.torrent");
        std::fs::write(&torrent_path, bytes).unwrap();
        let engine = crate::engine::Engine::new(root.path(), true).await.unwrap();
        let output = root.path().join("result");
        std::fs::create_dir(&output).unwrap();
        let id = engine
            .add(
                torrent_path.display().to_string(),
                output.display().to_string(),
                false,
                Some(vec![1]),
            )
            .await
            .unwrap();
        let task = |engine: &crate::engine::Engine| {
            engine.snapshot(&id)["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id)
                .unwrap()
                .clone()
        };
        tokio::time::timeout(Duration::from_secs(35), async {
            while task(&engine)["done"].as_u64().unwrap_or(0) < 16384 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        engine.action(&id, "pause", "").await.unwrap();
        let paused = task(&engine);
        assert_eq!(paused["webseeds"][0]["rejected"], true);
        assert!(paused["webseeds"][1]["verified_bytes"].as_u64().unwrap() > 0);
        engine.session.stop().await;
        drop(engine);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let engine = crate::engine::Engine::new(root.path(), true).await.unwrap();
        engine.restore().await;
        assert_eq!(engine.snapshot(&id)["detail"]["files"][1]["selected"], true);
        assert_eq!(
            engine.snapshot(&id)["detail"]["files"][3]["selected"],
            false
        );
        engine.action(&id, "resume", "").await.unwrap();
        let completed = tokio::time::timeout(Duration::from_secs(40), async {
            while task(&engine)["state"] != "complete" {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            completed.is_ok(),
            "Selected WebSeed download failed after restore: task={}, detail={}, requests={:?}",
            task(&engine),
            engine.snapshot(&id)["detail"],
            seen.lock().unwrap()
        );
        assert_eq!(
            std::fs::read(output.join("collection").join(names[1])).unwrap(),
            files[1]
        );
        assert!(
            !seen
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.ends_with("never.bin")),
            "Unselected unrelated file was fetched"
        );
        assert_eq!(engine.snapshot(&id)["detail"]["files"][1]["selected"], true);
        engine.action(&id, "remove", "all").await.unwrap();
        engine.session.stop().await;
        server.abort();
    }
}
