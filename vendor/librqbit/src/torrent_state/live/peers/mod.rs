use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use dashmap::DashMap;
use librqbit_core::lengths::ValidPieceIndex;
use parking_lot::{Mutex, MutexGuard, RwLock};

use crate::{
    Error,
    torrent_state::utils::{TimedExistence, atomic_inc},
    type_aliases::{BF, PeerHandle},
};

use self::stats::{AggregatePeerStats, AggregatePeerStatsAtomic};

use super::peer::{LivePeerState, Peer, PeerRx, PeerState, PeerTx};

pub mod stats;

// The socket semaphore only bounds active connections. Discovery can otherwise
// retain an unlimited number of queued peers, errors and reconnect tasks.
pub(crate) const MAX_REMEMBERED_PEERS: usize = 4096;

pub(crate) struct PeerStates {
    pub session_stats: Arc<AggregatePeerStatsAtomic>,

    // This keeps track of live addresses we connected to, for PEX.
    pub live_outgoing_peers: RwLock<HashSet<PeerHandle>>,
    pub stats: AggregatePeerStatsAtomic,
    pub states: DashMap<PeerHandle, Peer>,
    pub admission: Mutex<()>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::Ipv4Addr, sync::atomic::Ordering};

    fn peers() -> PeerStates {
        PeerStates {
            session_stats: Default::default(),
            live_outgoing_peers: Default::default(),
            stats: Default::default(),
            states: Default::default(),
            admission: Default::default(),
        }
    }

    fn addr(index: usize) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::from(0x0a00_0000 + index as u32), 4242))
    }

    #[test]
    fn memory_sustained_discovery_is_bounded() {
        let peers = peers();
        for index in 0..MAX_REMEMBERED_PEERS * 2 {
            assert_eq!(
                peers.add_if_not_seen(addr(index)).is_some(),
                index < MAX_REMEMBERED_PEERS
            );
        }
        assert_eq!(peers.states.len(), MAX_REMEMBERED_PEERS);
        assert_eq!(peers.stats().queued as usize, MAX_REMEMBERED_PEERS);
        assert_eq!(
            peers.session_stats.snapshot().queued as usize,
            MAX_REMEMBERED_PEERS
        );
        assert!(peers.add_if_not_seen(addr(0)).is_none());
    }

    #[test]
    fn memory_concurrent_discovery_cannot_exceed_inventory_capacity() {
        let peers = Arc::new(peers());
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let peers = peers.clone();
                scope.spawn(move || {
                    for index in 0..MAX_REMEMBERED_PEERS / 4 {
                        peers.add_if_not_seen(addr(worker * MAX_REMEMBERED_PEERS + index));
                    }
                });
            }
        });
        assert_eq!(peers.states.len(), MAX_REMEMBERED_PEERS);
        assert_eq!(peers.stats().queued as usize, MAX_REMEMBERED_PEERS);
    }

    #[test]
    fn memory_inactive_slots_preserve_connections_and_retries() {
        let peers = peers();
        for index in 0..MAX_REMEMBERED_PEERS {
            peers.add_if_not_seen(addr(index));
        }
        let (_rx, _tx) = peers.mark_peer_connecting(addr(0)).unwrap();
        peers.with_peer_mut(addr(0), "test_live", |peer| {
            peer.stats
                .counters
                .outgoing_connections
                .fetch_add(1, Ordering::Relaxed);
            peer.connecting_to_live(
                librqbit_core::hash_id::Id20::new([0; 20]),
                &peers,
                crate::stream_connect::ConnectionKind::Tcp,
            );
        });
        let (_rx, _tx) = peers.mark_peer_connecting(addr(1)).unwrap();
        peers.with_peer_mut(addr(2), "test_retry", |peer| {
            peer.set_state(PeerState::Dead, &peers);
        });
        for index in 3..MAX_REMEMBERED_PEERS {
            peers.mark_peer_not_needed(addr(index));
        }
        for index in MAX_REMEMBERED_PEERS..MAX_REMEMBERED_PEERS * 2 - 3 {
            assert!(peers.add_if_not_seen(addr(index)).is_some());
        }
        assert!(peers.with_live(addr(0), |_| ()).is_some());
        assert!(peers.live_outgoing_peers.read().contains(&addr(0)));
        assert!(
            peers
                .with_peer(addr(1), |p| matches!(
                    p.get_state(),
                    PeerState::Connecting(_)
                ))
                .unwrap()
        );
        assert!(
            peers
                .with_peer(addr(2), |p| matches!(p.get_state(), PeerState::Dead))
                .unwrap()
        );
        assert_eq!(peers.states.len(), MAX_REMEMBERED_PEERS);
        let stats = peers.stats();
        assert_eq!(
            stats.live + stats.connecting + stats.dead + stats.queued + stats.not_needed,
            MAX_REMEMBERED_PEERS as u32
        );
    }

    #[test]
    fn memory_dead_incoming_peers_can_be_replaced() {
        let peers = peers();
        for index in 0..MAX_REMEMBERED_PEERS {
            peers.add_if_not_seen(addr(index));
        }
        peers.with_peer_mut(addr(0), "test_dead_incoming", |peer| {
            peer.outgoing_address = None;
            peer.set_state(PeerState::Dead, &peers);
        });
        assert!(peers.add_if_not_seen(addr(MAX_REMEMBERED_PEERS)).is_some());
        assert!(!peers.states.contains_key(&addr(0)));
        assert_eq!(peers.stats().dead, 0);
        assert_eq!(peers.states.len(), MAX_REMEMBERED_PEERS);
        // Existing addresses remain admissible for incoming reconnections.
        assert!(peers.admission_guard(addr(1)).is_some());
        assert!(
            peers
                .admission_guard(addr(MAX_REMEMBERED_PEERS + 1))
                .is_none()
        );
    }

    #[test]
    fn memory_disconnecting_peer_is_not_evicted_until_its_task_releases_it() {
        let peers = peers();
        for index in 0..MAX_REMEMBERED_PEERS {
            peers.add_if_not_seen(addr(index));
        }
        let task_counters = peers
            .with_peer(addr(0), |p| p.stats.counters.clone())
            .unwrap();
        peers.mark_peer_not_needed(addr(0));
        assert!(peers.add_if_not_seen(addr(MAX_REMEMBERED_PEERS)).is_none());
        assert!(peers.states.contains_key(&addr(0)));
        drop(task_counters);
        assert!(peers.add_if_not_seen(addr(MAX_REMEMBERED_PEERS)).is_some());
        assert!(!peers.states.contains_key(&addr(0)));
    }

    #[test]
    fn memory_dropping_inventory_releases_peer_allocations_and_session_counts() {
        let peers = peers();
        peers.add_if_not_seen(addr(0));
        let counters = peers
            .with_peer(addr(0), |p| Arc::downgrade(&p.stats.counters))
            .unwrap();
        let session_stats = peers.session_stats.clone();
        drop(peers);
        assert!(counters.upgrade().is_none());
        assert_eq!(session_stats.snapshot().queued, 0);
    }
}

impl Drop for PeerStates {
    fn drop(&mut self) {
        for (_, p) in std::mem::take(&mut self.states).into_iter() {
            p.destroy(self);
        }
    }
}

impl PeerStates {
    pub(crate) fn admission_guard(&self, addr: SocketAddr) -> Option<MutexGuard<'_, ()>> {
        let guard = self.admission.lock();
        if self.states.contains_key(&addr) {
            return Some(guard);
        }
        if self.states.len() >= MAX_REMEMBERED_PEERS {
            // Outgoing Dead peers have a pending retry task. Keep those, queued
            // peers and active connections; only replace inactive records whose
            // connection/retry task has released its counters. This prevents an
            // old task from mutating a newly admitted peer at the same address.
            let disposable = self.states.iter().find_map(|entry| {
                let peer = entry.value();
                (Arc::strong_count(&peer.stats.counters) == 1
                    && (matches!(peer.get_state(), PeerState::NotNeeded)
                        || (matches!(peer.get_state(), PeerState::Dead)
                            && peer.outgoing_address.is_none())))
                .then_some(*entry.key())
            });
            if let Some(addr) = disposable {
                // Recheck under the shard lock: an incoming connection may have
                // activated the peer since the inventory scan.
                let removed = self.states.remove_if(&addr, |_, peer| {
                    Arc::strong_count(&peer.stats.counters) == 1
                        && (matches!(peer.get_state(), PeerState::NotNeeded)
                            || (matches!(peer.get_state(), PeerState::Dead)
                                && peer.outgoing_address.is_none()))
                });
                if let Some((_, peer)) = removed {
                    peer.destroy(self);
                }
            }
        }
        (self.states.len() < MAX_REMEMBERED_PEERS).then_some(guard)
    }

    pub fn stats(&self) -> AggregatePeerStats {
        self.stats.snapshot()
    }

    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        let _admission = self.admission_guard(addr)?;
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Peer::new_with_outgoing_address(addr));
                atomic_inc(&self.stats.queued);
                atomic_inc(&self.session_stats.queued);

                atomic_inc(&self.stats.seen);
                atomic_inc(&self.session_stats.seen);
                Some(addr)
            }
        }
    }
    pub fn with_peer<R>(&self, addr: PeerHandle, f: impl FnOnce(&Peer) -> R) -> Option<R> {
        self.states.get(&addr).map(|e| f(e.value()))
    }

    pub fn with_peer_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut Peer) -> R,
    ) -> Option<R> {
        use crate::torrent_state::utils::timeit;
        timeit(reason, || self.states.get_mut(&addr))
            .map(|e| f(TimedExistence::new(e, reason).value_mut()))
    }

    pub fn with_live<R>(&self, addr: PeerHandle, f: impl FnOnce(&LivePeerState) -> R) -> Option<R> {
        self.with_peer(addr, |peer| peer.get_live().map(f))
            .flatten()
    }

    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| peer.get_live_mut().map(f))
            .flatten()
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        let p = self.states.remove(&handle).map(|r| r.1)?;
        let s = p.get_state();
        self.stats.dec(s);
        self.session_stats.dec(s);

        Some(p)
    }

    pub fn is_peer_not_interested_and_has_full_torrent(
        &self,
        handle: PeerHandle,
        total_pieces: usize,
    ) -> bool {
        self.with_live(handle, |live| {
            !live.peer_interested && live.has_full_torrent(total_pieces)
        })
        .unwrap_or(false)
    }

    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }

    pub fn update_bitfield(&self, handle: PeerHandle, bitfield: BF) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield", |live| {
            live.bitfield = bitfield;
        })
    }

    pub fn mark_peer_connecting(&self, h: PeerHandle) -> crate::Result<(PeerRx, PeerTx)> {
        let rx = self
            .with_peer_mut(h, "mark_peer_connecting", |peer| {
                peer.idle_to_connecting(self)
                    .ok_or(Error::BugInvalidPeerState)
            })
            .ok_or(Error::BugPeerNotFound)??;
        Ok(rx)
    }

    pub fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.reset_backoff();
        });
    }

    #[cfg(test)]
    fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<PeerState> {
        let prev = self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.set_not_needed(self)
        })?;
        Some(prev)
    }

    pub(crate) fn mark_queued_peer_not_needed(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "skip_queued_peer", |peer| {
            if matches!(peer.get_state(), PeerState::Queued) {
                peer.set_not_needed(self);
            }
        });
    }

    pub(crate) fn on_steal(
        &self,
        from_peer: SocketAddr,
        to_peer: SocketAddr,
        stolen_idx: ValidPieceIndex,
    ) {
        self.with_peer(to_peer, |p| {
            atomic_inc(&p.stats.counters.times_i_stole);
        });
        self.with_peer(from_peer, |p| {
            atomic_inc(&p.stats.counters.times_stolen_from_me);
        });
        self.stats.inc_steals();
        self.session_stats.inc_steals();

        self.with_live_mut(from_peer, "send_cancellations", |live| {
            live.cancel_inflight_requests_for_piece(stolen_idx);
        });
    }
}
