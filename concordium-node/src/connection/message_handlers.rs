use crate::{
    common::{get_current_stamp, P2PNodeId, P2PPeer, PeerType},
    connection::{Connection, P2PEvent},
    network::{
        NetworkId, NetworkMessage, NetworkMessagePayload, NetworkPacket, NetworkPacketType,
        NetworkRequest, NetworkResponse,
    },
    p2p::banned_nodes::BannedNode,
    plugins::consensus::*,
};
use concordium_common::{read_or_die, write_or_die, QueueMsg::Relay};

use failure::{Error, Fallible};

use std::{collections::HashSet, net::SocketAddr, sync::atomic::Ordering};

impl Connection {
    pub fn handle_incoming_message(&self, full_msg: &NetworkMessage) {
        if let Err(e) = match &full_msg.payload {
            NetworkMessagePayload::NetworkRequest(
                NetworkRequest::Handshake(remote_node_id, remote_port, ref networks, _),
                ..,
            ) => self.handle_handshake_req(*remote_node_id, *remote_port, networks),
            NetworkMessagePayload::NetworkResponse(
                NetworkResponse::Handshake(remote_node_id, remote_port, ref nets, _),
                ..,
            ) => self.handle_handshake_resp(*remote_node_id, *remote_port, nets),
            NetworkMessagePayload::NetworkRequest(NetworkRequest::Ping, ..) => self.send_pong(),
            NetworkMessagePayload::NetworkResponse(NetworkResponse::Pong, ..) => self.handle_pong(),
            NetworkMessagePayload::NetworkRequest(NetworkRequest::GetPeers(ref networks), ..) => {
                self.handle_get_peers_req(networks)
            }
            NetworkMessagePayload::NetworkResponse(NetworkResponse::PeerList(ref peers), ..) => {
                self.handle_peer_list_resp(peers)
            }
            NetworkMessagePayload::NetworkRequest(NetworkRequest::JoinNetwork(network), ..) => {
                self.handle_join_network_req(*network)
            }
            NetworkMessagePayload::NetworkRequest(NetworkRequest::LeaveNetwork(network), ..) => {
                self.handle_leave_network_req(*network)
            }
            NetworkMessagePayload::NetworkRequest(NetworkRequest::BanNode(peer_to_ban), ..) => {
                self.handler().ban_node(*peer_to_ban)
            }
            NetworkMessagePayload::NetworkRequest(NetworkRequest::UnbanNode(peer_to_unban), ..) => {
                self.handle_unban(*peer_to_unban)
            }
            NetworkMessagePayload::NetworkPacket(pac, ..) => self.handle_incoming_packet(&pac),
        } {
            if !self.handler_ref.is_terminated.load(Ordering::Relaxed) {
                // In other case we are closing the node so we won't output the possibly closed
                // channels errors
                error!("Couldn't handle the network message {:?}: {}", full_msg, e);
            }
        }
    }

    fn handle_handshake_req(
        &self,
        remote_node_id: P2PNodeId,
        remote_port: u16,
        networks: &HashSet<NetworkId>,
    ) -> Fallible<()> {
        debug!("Got a Handshake request from peer {}", remote_node_id);

        if self.handler().is_banned(BannedNode::ById(remote_node_id))? {
            self.handler().remove_connection(self.token);
            bail!("Rejected a handshake request from a banned node");
        }

        self.promote_to_post_handshake(remote_node_id, remote_port)?;
        self.add_remote_end_networks(networks);

        let remote_peer = P2PPeer::from(
            self.remote_peer.peer_type(),
            remote_node_id,
            SocketAddr::new(self.remote_peer.addr().ip(), remote_port),
        );

        self.send_handshake_response(remote_node_id)?;

        if remote_peer.peer_type() != PeerType::Bootstrapper {
            write_or_die!(self.handler().connection_handler.buckets)
                .insert_into_bucket(&remote_peer, networks.clone());
        }

        if self.handler().peer_type() == PeerType::Bootstrapper {
            debug!("Running in bootstrapper mode; attempting to send a PeerList upon handshake");
            self.send_peer_list_resp(networks)?;
        }

        Ok(())
    }

    fn handle_handshake_resp(
        &self,
        remote_node_id: P2PNodeId,
        remote_port: u16,
        networks: &HashSet<NetworkId>,
    ) -> Fallible<()> {
        debug!("Got a Handshake response from peer {}", remote_node_id);

        self.send_ping()?;

        self.promote_to_post_handshake(remote_node_id, remote_port)?;
        self.add_remote_end_networks(networks);

        self.stats
            .sent_handshake
            .store(get_current_stamp(), Ordering::SeqCst);

        let remote_peer = P2PPeer::from(
            self.remote_peer.peer_type(),
            remote_node_id,
            SocketAddr::new(self.remote_peer.addr().ip(), remote_port),
        );

        if remote_peer.peer_type() != PeerType::Bootstrapper {
            write_or_die!(self.handler().connection_handler.buckets)
                .insert_into_bucket(&remote_peer, networks.clone());
        }

        if let Some(ref service) = self.handler().stats_export_service {
            service.peers_inc();
        };

        Ok(())
    }

    fn handle_pong(&self) -> Fallible<()> {
        self.stats.valid_latency.store(true, Ordering::Relaxed);

        let ping_time: u64 = self.stats.last_ping_sent.load(Ordering::SeqCst);
        let curr_time: u64 = get_current_stamp();

        if curr_time >= ping_time {
            self.set_last_latency(curr_time - ping_time);
        }

        Ok(())
    }

    fn handle_get_peers_req(&self, networks: &HashSet<NetworkId>) -> Fallible<()> {
        let peer_id = self.remote_id().unwrap(); // safe, post-handshake

        debug!("Got a GetPeers request from peer {}", peer_id);

        self.send_peer_list_resp(networks)
    }

    fn handle_peer_list_resp(&self, peers: &[P2PPeer]) -> Fallible<()> {
        let peer_id = self.remote_id().unwrap(); // safe, post-handshake

        debug!("Received a PeerList response from peer {}", peer_id);

        let mut new_peers = 0;
        let current_peers = self.handler().get_peer_stats(Some(PeerType::Node));

        let curr_peer_count = current_peers.len();

        let applicable_candidates = peers.iter().filter(|candidate| {
            !current_peers
                .iter()
                .map(|peer| peer.id)
                .any(|id| id == candidate.id.as_raw())
        });

        let mut locked_buckets = safe_write!(self.handler().connection_handler.buckets)?;
        for peer in applicable_candidates {
            trace!(
                "Got info for peer {}/{}/{}",
                peer.id(),
                peer.ip(),
                peer.port()
            );
            if self
                .handler()
                .connect(PeerType::Node, peer.addr, Some(peer.id()))
                .map_err(|e| trace!("{}", e))
                .is_ok()
            {
                new_peers += 1;
                locked_buckets.insert_into_bucket(peer, HashSet::new());
            }

            if new_peers + curr_peer_count >= self.handler().config.desired_nodes_count as usize {
                break;
            }
        }

        Ok(())
    }

    fn handle_join_network_req(&self, network: NetworkId) -> Fallible<()> {
        let remote_peer = self.remote_peer().peer().unwrap(); // safe, post-handshake

        debug!(
            "Received a JoinNetwork request from peer {}",
            remote_peer.id
        );

        self.add_remote_end_network(network);
        safe_write!(self.handler().connection_handler.buckets)?.update_network_ids(
            &remote_peer,
            read_or_die!(self.remote_end_networks).to_owned(),
        );

        if let Some(ref log) = self.handler().connection_handler.event_log {
            if log
                .send(Relay(P2PEvent::JoinedNetwork(remote_peer, network)))
                .is_err()
            {
                error!("A JoinNetwork Event cannot be sent to the P2PEvent log");
            }
        }

        Ok(())
    }

    fn handle_leave_network_req(&self, network: NetworkId) -> Fallible<()> {
        let remote_peer = self.remote_peer().peer().unwrap(); // safe, post-handshake

        debug!(
            "Received a LeaveNetwork request from peer {}",
            remote_peer.id
        );

        self.remove_remote_end_network(network);
        safe_write!(self.handler().connection_handler.buckets)?.update_network_ids(
            &remote_peer,
            read_or_die!(self.remote_end_networks).to_owned(),
        );

        if let Some(ref log) = self.handler().connection_handler.event_log {
            if log
                .send(Relay(P2PEvent::LeftNetwork(remote_peer, network)))
                .is_err()
            {
                error!("Left Network Event cannot be sent to the P2PEvent log");
            }
        };

        Ok(())
    }

    fn handle_unban(&self, peer: BannedNode) -> Fallible<()> {
        let is_self_unban = match peer {
            BannedNode::ById(id) => Some(id) == self.remote_id(),
            BannedNode::ByAddr(addr) => addr == self.remote_addr().ip(),
        };
        if is_self_unban {
            bail!("Rejecting a self-unban attempt");
        }

        self.handler().unban_node(peer)
    }

    pub fn handle_incoming_packet(&self, pac: &NetworkPacket) -> Fallible<()> {
        let peer_id = self.remote_id().unwrap(); // safe, post-handshake

        trace!("Received a Packet from peer {}", peer_id);

        let is_broadcast = match pac.packet_type {
            NetworkPacketType::BroadcastedMessage(..) => true,
            _ => false,
        };

        #[cfg(feature = "benchmark")]
        {
            if !is_broadcast && self.handler().config.enable_tps_test {
                let mut stats_engine = write_or_die!(self.handler().stats_engine);
                if let Ok(len) = pac.message.len() {
                    stats_engine.add_stat(len);

                    if stats_engine.msg_count == self.handler().config.tps_message_count {
                        info!(
                            "TPS over {} messages is {}",
                            self.handler().config.tps_message_count,
                            stats_engine.calculate_total_tps_average()
                        );
                        stats_engine.clear();
                    }
                }
            }
        }

        let dont_relay_to =
            if let NetworkPacketType::BroadcastedMessage(ref peers) = pac.packet_type {
                let mut list = peers.clone().to_owned();
                list.push(peer_id);
                list
            } else {
                vec![]
            };

        handle_pkt_out(
            self.handler(),
            dont_relay_to,
            peer_id,
            pac.message.clone(),
            is_broadcast,
        )
    }

    pub fn handle_invalid_network_msg(&self, err: Error) {
        if let Some(peer_id) = self.remote_id() {
            debug!("Invalid network message from peer {}: {}", peer_id, err);
        }

        self.stats.failed_pkts.fetch_add(1, Ordering::Relaxed);

        if let Some(ref service) = self.handler().stats_export_service {
            service.invalid_pkts_received_inc();
        }
    }
}
