use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use anyhow::Context;
use buffers::ByteBufOwned;
use futures::{Stream, StreamExt, stream::FuturesUnordered};
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use tracing::{Instrument, debug, debug_span};

use crate::{
    peer_connection::PeerConnectionOptions, peer_info_reader, spawn_utils::BlockingSpawner,
    stream_connect::StreamConnector,
};
use librqbit_core::hash_id::Id20;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ReadMetainfoResult<Rx> {
    Found {
        info: TorrentMetaV1Info<ByteBufOwned>,
        info_bytes: ByteBufOwned,
        rx: Rx,
        seen: HashSet<SocketAddr>,
    },
    ChannelClosed {
        #[allow(dead_code)]
        seen: HashSet<SocketAddr>,
    },
}

pub async fn read_metainfo_from_peer_receiver<A: Stream<Item = SocketAddr> + Unpin>(
    peer_id: Id20,
    info_hash: Id20,
    initial_addrs: Vec<SocketAddr>,
    addrs_stream: A,
    peer_connection_options: Option<PeerConnectionOptions>,
    connector: Arc<StreamConnector>,
    client_name_and_version: String,
    trace: Option<Arc<crate::resolution_trace::Trace>>,
) -> ReadMetainfoResult<A> {
    let mut seen = HashSet::<SocketAddr>::new();
    let mut addrs = addrs_stream;

    let semaphore = tokio::sync::Semaphore::new(32);
    let mut retries = std::collections::HashMap::<SocketAddr, (tokio::time::Instant, u32)>::new();
    let mut retry_counts = std::collections::HashMap::<SocketAddr, u32>::new();
    let mut retry_tick = tokio::time::interval(std::time::Duration::from_secs(1));

    let read_info_guarded = |addr| {
        let semaphore = &semaphore;
        let connector = connector.clone();
        let client_name_and_version = client_name_and_version.clone();
        let trace = trace.clone();
        async move {
            let token = semaphore.acquire().await?;
            if let Some(trace) = &trace {
                trace.attempt(addr);
            }
            let _guard = trace
                .as_ref()
                .map(|t| crate::resolution_trace::AttemptGuard(t.clone(), addr));
            let ret = peer_info_reader::read_metainfo_from_peer(
                addr,
                peer_id,
                info_hash,
                peer_connection_options,
                // This shouldn't be called anyway as we aren't reading/writing to disk, so it's
                // ok not to use a shared one.
                BlockingSpawner::new(1),
                connector,
                client_name_and_version,
                trace.clone(),
            )
            .instrument(debug_span!("read_metainfo_from_peer", ?addr))
            .await
            .with_context(|| format!("error reading metainfo from {addr}"));
            if let Some(trace) = &trace {
                trace.finished(addr, ret.as_ref().err().map(|e| format!("{e:#}")));
            }
            drop(token);
            Ok::<_, anyhow::Error>((addr, ret))
        }
    };

    let mut unordered = FuturesUnordered::new();

    for a in initial_addrs {
        if seen.insert(a) {
            unordered.push(read_info_guarded(a));
        }
    }

    let mut addrs_completed = false;

    loop {
        if addrs_completed && unordered.is_empty() && retries.is_empty() {
            return ReadMetainfoResult::ChannelClosed { seen };
        }

        tokio::select! {
            done = unordered.next(), if !unordered.is_empty() => {
                match done {
                    Some(Ok((_, Ok((info, info_bytes))))) => return ReadMetainfoResult::Found { info, info_bytes, seen, rx: addrs },
                    Some(Ok((addr,Err(e)))) => {
                        debug!("{:#}", e);
                        let count=retry_counts.entry(addr).or_default();
                        *count=count.saturating_add(1);
                        let delay=(30u64.saturating_mul(1u64 << (*count).min(4))).min(300) + u64::from(addr.port()%11);
                        if retries.len()<256 { retries.insert(addr,(tokio::time::Instant::now()+std::time::Duration::from_secs(delay),*count)); }
                    },
                    Some(Err(e)) => debug!("{:#}",e),
                    None => unreachable!()
                }
            }

            _ = retry_tick.tick(), if !retries.is_empty() => {
                let now=tokio::time::Instant::now();
                let ready: Vec<_>=retries.iter().filter_map(|(a,(at,_))| (*at<=now).then_some(*a)).take(32usize.saturating_sub(unordered.len())).collect();
                for addr in ready { retries.remove(&addr); unordered.push(read_info_guarded(addr)); }
            }
            next_addr = addrs.next(), if !addrs_completed && unordered.len()<128 => {
                match next_addr {
                    Some(addr) => {
                        if seen.len()<4096 && seen.insert(addr) {
                            unordered.push(read_info_guarded(addr));
                        }
                        continue;
                    },
                    None => {
                        addrs_completed = true;
                    },
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use dht::{DhtBuilder, Id20};
    use librqbit_core::peer_id::generate_peer_id;

    use super::*;
    use std::{
        str::FromStr,
        sync::{Arc, Once},
    };

    static LOG_INIT: Once = Once::new();

    fn init_logging() {
        #[allow(unused_must_use)]
        LOG_INIT.call_once(|| {
            // pretty_env_logger::try_init();
        })
    }

    #[tokio::test]
    #[ignore]
    async fn read_metainfo_from_dht() {
        init_logging();

        let info_hash = Id20::from_str("cab507494d02ebb1178b38f2e9d7be299c86b862").unwrap();
        let dht = DhtBuilder::new().await.unwrap();

        let peer_rx = dht.get_peers(info_hash, None);
        let peer_id = generate_peer_id(b"-xx1234-");
        match read_metainfo_from_peer_receiver(
            peer_id,
            info_hash,
            Vec::new(),
            peer_rx,
            None,
            Arc::new(StreamConnector::new(Default::default()).await.unwrap()),
            crate::client_name_and_version().to_owned(),
            None,
        )
        .await
        {
            ReadMetainfoResult::Found { info, .. } => dbg!(info),
            ReadMetainfoResult::ChannelClosed { .. } => todo!("should not have happened"),
        };
    }
}
