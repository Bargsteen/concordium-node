use crate::{
    common::P2PPeer,
    network::{
        make_header, NetworkId, ProtocolMessageType,
    },
};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "s11n_serde", derive(Serialize, Deserialize))]
pub enum NetworkResponse {
    Pong(P2PPeer),
    FindNode(P2PPeer, Vec<P2PPeer>),
    PeerList(P2PPeer, Vec<P2PPeer>),
    Handshake(P2PPeer, HashSet<NetworkId>, Vec<u8>),
}

impl NetworkResponse {
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            NetworkResponse::Pong(_) => {
                format!("{}{}", make_header(), ProtocolMessageType::ResponsePong ).into_bytes()
            }
            NetworkResponse::FindNode(_, peers) => format!(
                "{}{}{:03}{}",
                make_header(),
                ProtocolMessageType::ResponseFindNode,
                peers.len(),
                peers
                    .iter()
                    .map(|peer| peer.serialize())
                    .collect::<String>()
            )
            .into_bytes(),
            NetworkResponse::PeerList(_, peers) => format!(
                "{}{}{:03}{}",
                make_header(),
                ProtocolMessageType::ResponsePeersList,
                peers.len(),
                peers
                    .iter()
                    .map(|peer| peer.serialize())
                    .collect::<String>()
            )
            .into_bytes(),
            NetworkResponse::Handshake(me, networks, zk) => {
                let mut pkt = format!(
                    "{}{}{}{:05}{:05}{}{:010}",
                    make_header(),
                    ProtocolMessageType::ResponseHandshake,
                    me.id(),
                    me.port(),
                    networks.len(),
                    networks
                        .iter()
                        .map(|net| net.to_string())
                        .collect::<String>(),
                    zk.len()
                )
                .into_bytes();
                pkt.extend_from_slice(zk.as_slice());
                pkt
            }
        }
    }
}
