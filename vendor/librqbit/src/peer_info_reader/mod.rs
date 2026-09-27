use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use bencode::from_bytes;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use librqbit_core::{
    constants::CHUNK_SIZE,
    hash_id::Id20,
    lengths::{ChunkInfo, last_element_size},
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{Mutex, RwLock};
use peer_binary_protocol::{
    Handshake, Message,
    extended::{
        ExtendedMessage,
        handshake::ExtendedHandshake,
        ut_metadata::{UtMetadata, UtMetadataData},
    },
};
use sha1w::{ISha1, Sha1};
use tokio::sync::mpsc::UnboundedSender;
use tracing::trace;

use crate::{
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    spawn_utils::BlockingSpawner,
    stream_connect::{ConnectionKind, StreamConnector},
};

pub(crate) async fn read_metainfo_from_peer(
    addr: SocketAddr,
    peer_id: Id20,
    info_hash: Id20,
    peer_connection_options: Option<PeerConnectionOptions>,
    spawner: BlockingSpawner,
    connector: Arc<StreamConnector>,
    client_name_and_version: String,
    trace: Option<Arc<crate::resolution_trace::Trace>>,
) -> anyhow::Result<TorrentAndInfoBytes> {
    if let Some(trace) = &trace {
        let cached = trace.metadata.read().as_ref().and_then(|inner| {
            inner
                .received_pieces
                .iter()
                .all(|p| *p)
                .then(|| Bytes::copy_from_slice(&inner.buffer))
        });
        if let Some(buf) = cached {
            use clone_to_owned::CloneToOwned;
            let info: TorrentMetaV1Info<ByteBufOwned> =
                from_bytes::<TorrentMetaV1Info<ByteBuf>>(&buf)
                    .map_err(|e| e.into_kind())?
                    .clone_to_owned(Some(&buf));
            return Ok((info, ByteBufOwned(buf)));
        }
    }
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<
        Result<(TorrentMetaV1Info<ByteBufOwned>, ByteBufOwned), bencode::DeserializeError>,
    >();
    let (writer_tx, writer_rx) = tokio::sync::mpsc::unbounded_channel::<WriterRequest>();
    let handler = Handler {
        addr,
        trace: trace.clone(),
        info_hash,
        writer_tx,
        result_tx: Mutex::new(Some(result_tx)),
        locked: trace
            .as_ref()
            .map(|t| t.metadata.clone())
            .unwrap_or_default(),
        requested: Mutex::new(HashSet::new()),
        rejected: Mutex::new(HashSet::new()),
        client_name_and_version,
    };
    let connection = PeerConnection::new(
        addr,
        info_hash,
        peer_id,
        handler,
        peer_connection_options,
        spawner,
        connector,
    );

    let mut result_reader = result_rx;
    let (_, brx) = tokio::sync::broadcast::channel(1);
    let connection_runner = async move { connection.manage_peer_outgoing(writer_rx, brx).await };

    tokio::select! {
        biased;
        result = &mut result_reader => Ok(result??),
        whatever = connection_runner => {
            // A peer may close immediately after the last valid fragment.
            // The handler can have delivered metadata during this same poll.
            if let Ok(result) = result_reader.try_recv() { return Ok(result?); }
            match whatever {
            Ok(_) => anyhow::bail!("connection runner completed first"),
            Err(e) => Err(e.into())
            }
        }
    }
}

pub(crate) struct HandlerLocked {
    metadata_size: u32,
    total_pieces: usize,
    buffer: Vec<u8>,
    received_pieces: Vec<bool>,
    last_progress: std::time::Instant,
}

static METADATA_MEMORY: AtomicUsize = AtomicUsize::new(0);
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
impl Drop for HandlerLocked {
    fn drop(&mut self) {
        METADATA_MEMORY.fetch_sub(self.buffer.len(), Ordering::Relaxed);
    }
}
impl HandlerLocked {
    pub(crate) fn progress(&self) -> (usize, usize, usize) {
        (
            self.received_pieces.iter().filter(|p| **p).count(),
            self.total_pieces,
            self.metadata_size as usize,
        )
    }
    fn new(metadata_size: u32) -> anyhow::Result<Self> {
        if metadata_size == 0 || metadata_size > 32 * 1024 * 1024 {
            anyhow::bail!("metadata size {} is too big", metadata_size);
        }
        METADATA_MEMORY
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(metadata_size as usize)
                    .filter(|n| *n <= MEMORY_LIMIT)
            })
            .map_err(|_| anyhow::anyhow!("metadata memory budget exhausted"))?;
        let buffer = vec![0u8; metadata_size as usize];
        let total_pieces: usize = (metadata_size as u64)
            .div_ceil(CHUNK_SIZE as u64)
            .try_into()?;
        let received_pieces = vec![false; total_pieces];
        Ok(Self {
            metadata_size,
            received_pieces,
            buffer,
            total_pieces,
            last_progress: std::time::Instant::now(),
        })
    }
    fn piece_size(&self, index: u32) -> usize {
        if index as usize == self.total_pieces - 1 {
            last_element_size(self.metadata_size as u64, CHUNK_SIZE as u64)
                .try_into()
                .unwrap()
        } else {
            CHUNK_SIZE as usize
        }
    }
    fn record_piece(
        &mut self,
        d: &UtMetadataData<ByteBuf>,
        info_hash: &Id20,
    ) -> anyhow::Result<bool> {
        if d.total_size() != self.metadata_size {
            anyhow::bail!("metadata size changed");
        }
        let piece = d.piece();
        if piece as usize >= self.total_pieces {
            anyhow::bail!("wrong index");
        }
        let offset = (piece * CHUNK_SIZE) as usize;
        let size = self.piece_size(piece);
        if d.len() != size {
            anyhow::bail!(
                "expected length of piece {} to be {}, but got {}",
                piece,
                size,
                d.len()
            );
        }
        if self.received_pieces[piece as usize] {
            let mut incoming = vec![0; d.len()];
            d.copy_to_slice(&mut incoming);
            if self.buffer[offset..offset + size] != incoming {
                anyhow::bail!("conflicting metadata piece {}", piece);
            }
            return Ok(false);
        }
        d.copy_to_slice(&mut self.buffer[offset..offset + d.len()]);
        self.received_pieces[piece as usize] = true;
        self.last_progress = std::time::Instant::now();

        if self.received_pieces.iter().all(|p| *p) {
            // check metadata
            let mut hash = Sha1::new();
            hash.update(&self.buffer);
            if hash.finish() != info_hash.0 {
                self.received_pieces.fill(false);
                self.buffer.fill(0);
                anyhow::bail!("info checksum invalid; discarded unverified fragments");
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub type TorrentAndInfoBytes = (TorrentMetaV1Info<ByteBufOwned>, ByteBufOwned);

struct Handler {
    trace: Option<Arc<crate::resolution_trace::Trace>>,
    addr: SocketAddr,
    info_hash: Id20,
    writer_tx: UnboundedSender<WriterRequest>,
    result_tx: Mutex<
        Option<
            tokio::sync::oneshot::Sender<Result<TorrentAndInfoBytes, bencode::DeserializeError>>,
        >,
    >,
    locked: Arc<RwLock<Option<HandlerLocked>>>,
    requested: Mutex<HashSet<u32>>,
    rejected: Mutex<HashSet<u32>>,
    client_name_and_version: String,
}

impl PeerConnectionHandler for Handler {
    fn on_connected(&self, _time: std::time::Duration) {
        if let Some(trace) = &self.trace {
            trace.connected(self.addr);
        }
    }
    fn should_send_bitfield(&self) -> bool {
        false
    }

    fn serialize_bitfield_message_to_buf(&self, _buf: &mut [u8]) -> anyhow::Result<usize> {
        Ok(0)
    }

    fn on_handshake(&self, handshake: Handshake, kind: ConnectionKind) -> anyhow::Result<()> {
        if let Some(trace) = &self.trace {
            trace.handshake(self.addr, &kind.to_string());
        }
        if !handshake.supports_extended() {
            anyhow::bail!(
                "this peer does not support extended handshaking, which is a prerequisite to download metadata"
            )
        }
        Ok(())
    }

    async fn on_received_message(&self, msg: Message<'_>) -> anyhow::Result<()> {
        trace!("{}: received message: {:?}", self.addr, msg);

        if let Message::Extended(ExtendedMessage::UtMetadata(UtMetadata::Data(utdata))) = msg {
            if let Some(trace) = &self.trace {
                trace.received(self.addr, utdata.len());
            }
            let mut locked = self.locked.write();
            if locked.as_ref().is_some_and(|m| {
                m.metadata_size != utdata.total_size()
                    && (!m.received_pieces.iter().any(|p| *p)
                        || m.last_progress.elapsed().as_secs() >= 120)
            }) {
                locked.take();
            }
            if locked.is_none() {
                if utdata.piece() != 0 {
                    anyhow::bail!("expected first metadata piece");
                }
                locked.replace(HandlerLocked::new(utdata.total_size())?);
            }
            let inner = locked.as_mut().unwrap();
            let piece_ready = inner.record_piece(&utdata, &self.info_hash)?;
            self.requested.lock().remove(&utdata.piece());
            let ready = piece_ready.then(|| Bytes::copy_from_slice(&inner.buffer));
            drop(locked);
            if let Some(trace) = &self.trace {
                trace.received(self.addr, 0);
            }
            if !piece_ready {
                self.request_more()?;
            }
            if let Some(buf) = ready {
                let info = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&buf)
                    .map(|i| {
                        use clone_to_owned::CloneToOwned;
                        i.clone_to_owned(Some(&buf))
                    })
                    .map_err(|e| {
                        trace!("error deserializing TorrentMetaV1Info: {e:#}");
                        e.into_kind()
                    })
                    .map(|i| (i, ByteBufOwned(buf)));

                self.result_tx
                    .lock()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("oneshot is consumed"))?
                    .send(info)
                    .map_err(|_| {
                        anyhow::anyhow!("torrent info deserialized, but consumer closed")
                    })?;
            }
        } else if let Message::Extended(ExtendedMessage::UtMetadata(UtMetadata::Reject(piece))) =
            msg
        {
            self.requested.lock().remove(&piece);
            self.rejected.lock().insert(piece);
            self.request_more()?;
            if self.requested.lock().is_empty() {
                anyhow::bail!("peer rejected remaining metadata requests");
            }
        }
        Ok(())
    }

    fn on_uploaded_bytes(&self, _bytes: u32) {}

    fn read_chunk(&self, _chunk: &ChunkInfo, _buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("the peer is not supposed to be requesting chunks")
    }

    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        if !extended_handshake.m.ut_metadata.is_some_and(|id| id != 0) {
            anyhow::bail!("peer does not support ut_metadata");
        }
        self.writer_tx
            .send(WriterRequest::Message(Message::Unchoke))?;
        self.writer_tx
            .send(WriterRequest::Message(Message::Interested))?;
        if let Some(size) = extended_handshake.metadata_size.filter(|size| *size > 0) {
            let mut locked = self.locked.write();
            if locked.as_ref().is_some_and(|m| {
                m.metadata_size != size
                    && (!m.received_pieces.iter().any(|p| *p)
                        || m.last_progress.elapsed().as_secs() >= 120)
            }) {
                locked.take();
            }
            match locked.as_ref() {
                Some(inner) if inner.metadata_size != size => {
                    anyhow::bail!("metadata size disagrees with collected fragments")
                }
                None => {
                    locked.replace(HandlerLocked::new(size)?);
                }
                _ => {}
            }
        }
        // Some compatible peers only disclose total_size in the first data reply.
        self.request_more()?;
        Ok(())
    }

    fn should_transmit_have(&self, _id: librqbit_core::lengths::ValidPieceIndex) -> bool {
        false
    }

    fn client_name_and_version(&self) -> &str {
        &self.client_name_and_version
    }
}

impl Handler {
    fn request_more(&self) -> anyhow::Result<()> {
        let locked = self.locked.read();
        let mut requested = self.requested.lock();
        let missing: Vec<u32> = match locked.as_ref() {
            Some(inner) => inner
                .received_pieces
                .iter()
                .enumerate()
                .filter_map(|(i, have)| (!have).then_some(i as u32))
                .collect(),
            None => vec![0],
        };
        let rejected = self.rejected.lock();
        for piece in missing {
            if rejected.contains(&piece) {
                continue;
            }
            if requested.len() >= 2 {
                break;
            }
            if requested.insert(piece) {
                self.writer_tx
                    .send(WriterRequest::Message(Message::Extended(
                        ExtendedMessage::UtMetadata(UtMetadata::Request(piece)),
                    )))?;
            }
        }
        Ok(())
    }
}
