//! Bounded observations of real discovery and magnet metadata exchanges.
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
};

#[derive(Default, Serialize)]
struct Peer {
    sources: BTreeSet<String>,
    state: String,
    transport: String,
    error: String,
    attempts: u64,
    handshakes: u64,
    metadata_bytes: u64,
}
#[derive(Default)]
struct State {
    peers: BTreeMap<SocketAddr, Peer>,
    discoveries: u64,
    attempts: u64,
    handshakes: u64,
    errors: u64,
    metadata_bytes: u64,
    completed: u64,
    final_progress: (usize, usize, usize),
}
#[derive(Default)]
pub(crate) struct Trace {
    state: Mutex<State>,
    pub(crate) metadata:
        std::sync::Arc<parking_lot::RwLock<Option<crate::peer_info_reader::HandlerLocked>>>,
}
impl Trace {
    fn update(&self, addr: SocketAddr, f: impl FnOnce(&mut State, &mut Peer)) {
        let mut state = self.state.lock();
        if !state.peers.contains_key(&addr) && state.peers.len() >= 256 {
            return;
        }
        let mut peer = state.peers.remove(&addr).unwrap_or_default();
        f(&mut state, &mut peer);
        state.peers.insert(addr, peer);
    }
    pub fn discovered(&self, addr: SocketAddr, source: &str) {
        self.update(addr, |s, p| {
            if p.sources.insert(source.into()) {
                s.discoveries += 1;
            }
            if p.state.is_empty() {
                p.state = "discovered".into();
            }
        });
    }
    pub fn attempt(&self, addr: SocketAddr) {
        self.update(addr, |s, p| {
            s.attempts += 1;
            p.attempts += 1;
            p.state = "connecting".into();
            p.error.clear();
        });
    }
    pub fn connected(&self, addr: SocketAddr) {
        self.update(addr, |_, p| p.state = "handshaking".into());
    }
    pub fn handshake(&self, addr: SocketAddr, transport: &str) {
        self.update(addr, |s, p| {
            s.handshakes += 1;
            p.handshakes += 1;
            p.state = "waiting_metadata".into();
            p.transport = transport.into();
        });
    }
    pub fn received(&self, addr: SocketAddr, bytes: usize) {
        self.update(addr, |s, p| {
            s.metadata_bytes += bytes as u64;
            p.metadata_bytes += bytes as u64;
            p.state = "receiving_metadata".into();
        });
    }
    pub fn finished(&self, addr: SocketAddr, error: Option<String>) {
        self.update(addr, |s, p| {
            if let Some(error) = error {
                s.errors += 1;
                p.state = "failed".into();
                p.error = error.chars().take(1500).collect();
            } else {
                s.completed += 1;
                p.state = "metadata_ready".into();
            }
        });
    }
    pub fn cancelled(&self, addr: SocketAddr) {
        self.update(addr, |_, p| {
            if matches!(
                p.state.as_str(),
                "connecting" | "handshaking" | "waiting_metadata" | "receiving_metadata"
            ) {
                p.state = "cancelled".into();
            }
        });
    }
    pub fn release_verified_metadata(&self) {
        let mut metadata = self.metadata.write();
        if let Some(m) = metadata.take() {
            self.state.lock().final_progress = m.progress();
        }
    }
    pub fn snapshot(&self) -> Value {
        let progress = self.metadata.read().as_ref().map(|m| m.progress());
        let s = self.state.lock();
        let progress = progress.unwrap_or(s.final_progress);
        json!({"collected_pieces":progress.0,"total_pieces":progress.1,"metadata_size":progress.2,"observed_peers":s.peers.len(),"discoveries":s.discoveries,"attempts":s.attempts,
            "handshakes":s.handshakes,"errors":s.errors,"metadata_bytes":s.metadata_bytes,"completed":s.completed,
            "peers":s.peers.iter().map(|(addr,p)|{let mut v=serde_json::to_value(p).unwrap();v["address"]=json!(addr.to_string());v}).collect::<Vec<_>>()})
    }
}
pub(crate) struct AttemptGuard(pub std::sync::Arc<Trace>, pub SocketAddr);
impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.0.cancelled(self.1);
    }
}
