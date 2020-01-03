use crate::{common::P2PNodeId, connection::ConnectionStats};

use std::{
    cmp::Ordering,
    fmt::{self, Display},
    hash::{Hash, Hasher},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU16, Ordering as AtomicOrdering},
        Arc, RwLock,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "s11n_serde", derive(Serialize, Deserialize))]
pub enum PeerType {
    Node,
    Bootstrapper,
}

impl fmt::Display for PeerType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", match self {
            PeerType::Node => "Node",
            PeerType::Bootstrapper => "Bootstrapper",
        })
    }
}

#[derive(Debug, Clone, Copy, Builder)]
#[cfg_attr(feature = "s11n_serde", derive(Serialize, Deserialize))]
pub struct P2PPeer {
    pub id:        P2PNodeId,
    pub addr:      SocketAddr,
    pub peer_type: PeerType,
}

impl P2PPeer {
    pub fn from(peer_type: PeerType, id: P2PNodeId, addr: SocketAddr) -> Self {
        P2PPeer {
            peer_type,
            id,
            addr,
        }
    }

    pub fn id(&self) -> P2PNodeId { self.id }

    pub fn ip(&self) -> IpAddr { self.addr.ip() }

    pub fn port(&self) -> u16 { self.addr.port() }

    pub fn peer_type(&self) -> PeerType { self.peer_type }
}

impl PartialEq for P2PPeer {
    fn eq(&self, other: &P2PPeer) -> bool { self.id == other.id() }
}

impl Eq for P2PPeer {}

impl Hash for P2PPeer {
    fn hash<H: Hasher>(&self, state: &mut H) { self.id.hash(state); }
}

impl Ord for P2PPeer {
    fn cmp(&self, other: &P2PPeer) -> Ordering { self.id.cmp(&other.id()) }
}

impl PartialOrd for P2PPeer {
    fn partial_cmp(&self, other: &P2PPeer) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Display for P2PPeer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}/{}:{}", self.id(), self.addr.ip(), self.addr.port())
    }
}

#[derive(Debug, Clone)]
pub struct RemotePeer {
    pub id:                 Arc<RwLock<Option<P2PNodeId>>>,
    pub addr:               SocketAddr,
    pub peer_external_port: Arc<AtomicU16>,
    pub peer_type:          PeerType,
}

impl RemotePeer {
    pub fn peer(self) -> Option<P2PPeer> {
        if let Some(id) = &*read_or_die!(self.id) {
            Some(P2PPeer {
                id:        *id,
                addr:      self.addr,
                peer_type: self.peer_type,
            })
        } else {
            None
        }
    }

    pub fn peer_external(self) -> Option<P2PPeer> {
        if let Some(id) = &*read_or_die!(self.id) {
            Some(P2PPeer {
                id:        *id,
                addr:      SocketAddr::new(
                    self.addr.ip(),
                    self.peer_external_port.load(AtomicOrdering::SeqCst),
                ),
                peer_type: self.peer_type,
            })
        } else {
            None
        }
    }

    pub fn addr(&self) -> SocketAddr { self.addr }

    pub fn peer_type(&self) -> PeerType { self.peer_type }

    pub fn peer_external_port(&self) -> u16 { self.peer_external_port.load(AtomicOrdering::SeqCst) }

    pub fn peer_external_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr.ip(), self.peer_external_port.load(AtomicOrdering::SeqCst))
    }
}

impl From<P2PPeer> for RemotePeer {
    fn from(peer: P2PPeer) -> Self {
        Self {
            id:                 Arc::new(RwLock::new(Some(peer.id))),
            addr:               peer.addr,
            peer_external_port: Arc::new(AtomicU16::new(peer.addr.port())),
            peer_type:          peer.peer_type,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerStats {
    pub id:                 u64,
    pub addr:               SocketAddr,
    pub peer_external_port: u16,
    pub peer_type:          PeerType,
    pub sent:               u64,
    pub received:           u64,
    pub valid_latency:      bool,
    pub measured_latency:   u64,
    pub bytes_sent:         u64,
    pub bytes_received:     u64,
}

impl PeerStats {
    pub fn new(
        id: u64,
        addr: SocketAddr,
        peer_external_port: u16,
        peer_type: PeerType,
        conn_stats: &ConnectionStats,
    ) -> PeerStats {
        PeerStats {
            id,
            addr,
            peer_external_port,
            peer_type,
            sent: conn_stats.messages_sent.load(AtomicOrdering::Relaxed),
            received: conn_stats.messages_received.load(AtomicOrdering::Relaxed),
            valid_latency: conn_stats.valid_latency.load(AtomicOrdering::Relaxed),
            measured_latency: conn_stats.last_latency.load(AtomicOrdering::Relaxed),
            bytes_sent: conn_stats.bytes_sent.load(AtomicOrdering::Relaxed),
            bytes_received: conn_stats.bytes_received.load(AtomicOrdering::Relaxed),
        }
    }

    pub fn external_address(&self) -> SocketAddr {
        SocketAddr::new(self.addr.ip(), self.peer_external_port)
    }
}
