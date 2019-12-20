pub const PAYLOAD_TYPE_LENGTH: u64 = 2;
pub const FILE_NAME_GENESIS_DATA: &str = "genesis.dat";
pub const FILE_NAME_CRYPTO_PROV_DATA: &str = "crypto_providers.json";
pub const FILE_NAME_ID_PROV_DATA: &str = "identity_providers.json";
pub const FILE_NAME_PREFIX_BAKER_PRIVATE: &str = "baker-";
pub const FILE_NAME_SUFFIX_BAKER_PRIVATE: &str = "-credentials.json";

use byteorder::{ReadBytesExt, WriteBytesExt};
use failure::Fallible;

use crossbeam_channel::TrySendError;
use std::{
    convert::TryFrom,
    fs::OpenOptions,
    io::{Read, Write},
    mem,
    sync::{Arc, RwLock},
};

use concordium_common::{
    serial::Endianness,
    ConsensusFfiResponse,
    PacketType::{self, *},
    QueueMsg,
};

use consensus_rust::{
    consensus::{self, PeerId, CALLBACK_QUEUE},
    ffi,
};

use globalstate_rust::{
    catch_up::{PeerList, PeerState, PeerStatus},
    common::sha256,
    tree::{
        messaging::{ConsensusMessage, DistributionMode, MessageType},
        GlobalState,
    },
};

use crate::{
    common::{get_current_stamp, P2PNodeId},
    configuration::{self, MAX_CATCH_UP_TIME},
    network::NetworkId,
    p2p::p2p_node::*,
};

pub fn start_consensus_layer(
    conf: &configuration::BakerConfig,
    gsptr: GlobalState,
    genesis_data: Vec<u8>,
    private_data: Option<Vec<u8>>,
    max_logging_level: consensus::ConsensusLogLevel,
) -> Fallible<consensus::ConsensusContainer> {
    info!("Starting up the consensus thread");

    #[cfg(feature = "profiling")]
    ffi::start_haskell(
        &conf.heap_profiling,
        conf.time_profiling,
        conf.backtraces_profiling,
        conf.gc_logging.clone(),
        conf.profiling_sampling_interval,
    );
    #[cfg(not(feature = "profiling"))]
    ffi::start_haskell();

    consensus::ConsensusContainer::new(
        u64::from(conf.maximum_block_size),
        conf.scheduler_outcome_logging,
        genesis_data,
        private_data,
        conf.baker_id,
        gsptr,
        max_logging_level,
    )
}

pub fn get_baker_private_data_json_file(
    app_prefs: &configuration::AppPreferences,
    conf: &configuration::BakerConfig,
) -> Option<String> {
    if let Some(baker_id) = conf.baker_id {
        let mut private_loc = app_prefs.get_user_app_dir();
        private_loc.push(format!(
            "{}{}{}",
            FILE_NAME_PREFIX_BAKER_PRIVATE, baker_id, FILE_NAME_SUFFIX_BAKER_PRIVATE
        ));
        if let Some(path) = private_loc.to_str() {
            Some(path.to_owned())
        } else {
            None
        }
    } else {
        None
    }
}

pub fn get_baker_data(
    app_prefs: &configuration::AppPreferences,
    conf: &configuration::BakerConfig,
    needs_private: bool,
) -> Fallible<(Vec<u8>, Option<Vec<u8>>)> {
    let mut genesis_loc = app_prefs.get_user_app_dir();
    genesis_loc.push(FILE_NAME_GENESIS_DATA);

    let mut private_loc = app_prefs.get_user_app_dir();

    if let Some(baker_id) = conf.baker_id {
        private_loc.push(format!(
            "{}{}{}",
            FILE_NAME_PREFIX_BAKER_PRIVATE, baker_id, FILE_NAME_SUFFIX_BAKER_PRIVATE
        ))
    };

    let genesis_data = match OpenOptions::new().read(true).open(&genesis_loc) {
        Ok(mut file) => {
            let mut read_data = vec![];
            match file.read_to_end(&mut read_data) {
                Ok(_) => read_data,
                Err(_) => bail!("Couldn't read genesis file properly"),
            }
        }
        Err(e) => bail!("Can't open the genesis file ({})!", e),
    };

    let private_data = if needs_private {
        match OpenOptions::new().read(true).open(&private_loc) {
            Ok(mut file) => {
                let mut read_data = vec![];
                match file.read_to_end(&mut read_data) {
                    Ok(_) => Some(read_data),
                    Err(_) => bail!("Couldn't open up private baker file for reading"),
                }
            }
            Err(e) => bail!("Can't open the private data file ({})!", e),
        }
    } else {
        None
    };

    debug!(
        "Obtained genesis data {:?}",
        sha256(&[&[0u8; 8], genesis_data.as_slice()].concat())
    );
    Ok((genesis_data, private_data))
}

/// Handles packets coming from other peers
pub fn handle_pkt_out(
    node: &P2PNode,
    dont_relay_to: Vec<P2PNodeId>,
    peer_id: P2PNodeId,
    msg: Arc<[u8]>,
    is_broadcast: bool,
) -> Fallible<()> {
    ensure!(
        msg.len() >= 2,
        "Packet payload can't be smaller than 2 bytes"
    );
    let consensus_type = (&msg[..2]).read_u16::<Endianness>()?;
    let packet_type = PacketType::try_from(consensus_type)?;

    let distribution_mode = if is_broadcast {
        DistributionMode::Broadcast
    } else {
        DistributionMode::Direct
    };

    let request = ConsensusMessage::new(
        MessageType::Inbound(peer_id.0, distribution_mode),
        packet_type,
        msg,
        dont_relay_to.into_iter().map(P2PNodeId::as_raw).collect(),
    );

    match if packet_type == PacketType::Transaction {
        CALLBACK_QUEUE.send_in_low_priority_message(request)
    } else {
        CALLBACK_QUEUE.send_in_high_priority_message(request)
    } {
        Ok(_) => {
            if packet_type == PacketType::Transaction {
                node.stats.inbound_low_priority_consensus_inc();
            } else {
                node.stats.inbound_high_priority_consensus_inc();
            }
        }
        Err(e) => match e.downcast::<TrySendError<QueueMsg<ConsensusMessage>>>()? {
            TrySendError::Full(_) => {
                if packet_type == PacketType::Transaction {
                    node.stats.inbound_low_priority_consensus_drops_inc();
                    warn!("The low priority inbound consensus queue is full!")
                } else {
                    node.stats.inbound_high_priority_consensus_drops_inc();
                    warn!("The high priority inbound consensus queue is full!")
                }
            }
            TrySendError::Disconnected(_) => {
                panic!("One of the inbound consensus queues has been shutdown!")
            }
        },
    }

    Ok(())
}

pub fn handle_consensus_inbound_message(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
    peers: &RwLock<PeerList>,
    no_rebroadcast_consensus_validation: bool,
) -> Fallible<()> {
    process_external_gs_entry(
        node,
        network_id,
        consensus,
        request,
        peers,
        no_rebroadcast_consensus_validation,
    )
}

pub fn handle_consensus_outbound_message(
    node: &P2PNode,
    network_id: NetworkId,
    request: ConsensusMessage,
) -> Fallible<()> {
    process_internal_gs_entry(node, network_id, request)
}

fn process_internal_gs_entry(
    node: &P2PNode,
    network_id: NetworkId,
    request: ConsensusMessage,
) -> Fallible<()> {
    send_consensus_msg_to_net(
        node,
        request.dont_relay_to(),
        node.self_peer.id,
        request.target_peer().map(P2PNodeId),
        network_id,
        NetworkPayload::Split(request.payload.to_vec(), request.variant),
    )
}

fn process_external_gs_entry(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
    peers_lock: &RwLock<PeerList>,
    no_rebroadcast_consensus_validation: bool,
) -> Fallible<()> {
    let source = P2PNodeId(request.source_peer());

    if no_rebroadcast_consensus_validation {
        if request.distribution_mode() == DistributionMode::Broadcast {
            send_consensus_msg_to_net(
                &node,
                request.dont_relay_to(),
                source,
                None,
                network_id,
                NetworkPayload::Full(request.payload.clone(), request.variant.to_string()),
            )?;
        }

        // relay external messages to Consensus
        let consensus_result = send_msg_to_consensus(node, source, consensus, &request)?;

        // adjust the peer state(s) based on the feedback from Consensus
        update_peer_states(peers_lock, &request, consensus_result);
    } else {
        // relay external messages to Consensus
        let consensus_result = send_msg_to_consensus(node, source, consensus, &request)?;

        // adjust the peer state(s) based on the feedback from Consensus
        update_peer_states(peers_lock, &request, consensus_result);

        // rebroadcast incoming broadcasts if applicable
        if request.distribution_mode() == DistributionMode::Broadcast
            && consensus_result.is_rebroadcastable()
        {
            send_consensus_msg_to_net(
                &node,
                request.dont_relay_to(),
                source,
                None,
                network_id,
                NetworkPayload::Full(request.payload, request.variant.to_string()),
            )?;
        }
    }

    Ok(())
}

fn send_msg_to_consensus(
    node: &P2PNode,
    source_id: P2PNodeId,
    consensus: &mut consensus::ConsensusContainer,
    request: &ConsensusMessage,
) -> Fallible<ConsensusFfiResponse> {
    let raw_id = source_id.as_raw();
    let payload = &request.payload[2..];

    let consensus_response = match request.variant {
        Block => consensus.send_block(payload),
        Transaction => consensus.send_transaction(payload),
        FinalizationMessage => consensus.send_finalization(payload),
        FinalizationRecord => consensus.send_finalization_record(payload),
        CatchUpStatus => {
            consensus.receive_catch_up_status(payload, raw_id, node.config.catch_up_batch_limit)
        }
    };

    if consensus_response.is_acceptable() {
        info!("Processed a {} from {}", request.variant, source_id);
    } else {
        debug!(
            "Couldn't process a {} due to error code {:?}",
            request, consensus_response,
        );
    }

    Ok(consensus_response)
}

enum NetworkPayload {
    Full(Arc<[u8]>, String), // a payload prepended with the PacketType and a packet description
    Split(Vec<u8>, PacketType), // split packet type and the payload; the description is not needed
}

fn send_consensus_msg_to_net(
    node: &P2PNode,
    dont_relay_to: Vec<u64>,
    source_id: P2PNodeId,
    target_id: Option<P2PNodeId>,
    network_id: NetworkId,
    payload: NetworkPayload,
) -> Fallible<()> {
    let (payload, msg_desc) = match payload {
        NetworkPayload::Full(payload, desc) => (payload, desc),
        NetworkPayload::Split(payload, packet_type) => {
            let mut buffer = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize + payload.len());
            buffer
                .write_u16::<Endianness>(packet_type as u16)
                .expect("Can't write a packet payload to buffer");
            buffer.write_all(&payload)?;
            (Arc::from(buffer), packet_type.to_string())
        }
    };

    let result = if target_id.is_some() {
        send_direct_message(node, source_id, target_id, network_id, payload)
    } else {
        send_broadcast_message(
            node,
            source_id,
            dont_relay_to.into_iter().map(P2PNodeId).collect(),
            network_id,
            payload,
        )
    };

    let target_desc = if let Some(id) = target_id {
        format!("direct message to peer {}", id)
    } else {
        "broadcast".to_string()
    };

    match result {
        Ok(_) => info!("Sent a {} containing a {}", target_desc, msg_desc),
        Err(_) => error!("Couldn't send a {} containing a {}!", target_desc, msg_desc,),
    }
    Ok(())
}

fn send_catch_up_status(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    peers_lock: &RwLock<PeerList>,
    target: PeerId,
) -> Fallible<()> {
    debug!("Global state: I'm catching up with peer {:016x}", target);

    let peers = &mut write_or_die!(peers_lock);

    peers
        .peers
        .change_priority(&target, PeerState::new(PeerStatus::CatchingUp));

    peers.catch_up_stamp = get_current_stamp();

    send_consensus_msg_to_net(
        node,
        vec![],
        node.self_peer.id,
        Some(P2PNodeId(target)),
        network_id,
        NetworkPayload::Split(consensus.get_catch_up_status(), PacketType::CatchUpStatus),
    )
}

pub fn update_peer_list(node: &P2PNode, peers_lock: &RwLock<PeerList>) {
    debug!("The peers have changed; updating the catch-up peer list");

    let peer_ids = node.get_node_peer_ids();

    let mut peers = write_or_die!(peers_lock);
    // remove global state peers whose connections were dropped
    for (live_peer, state) in mem::replace(&mut peers.peers, Default::default())
        .into_iter()
        .filter(|(id, _)| peer_ids.contains(&id))
    {
        peers.peers.push(live_peer, state);
    }

    // include newly added peers
    peers.peers.reserve(peer_ids.len());
    for id in peer_ids {
        if peers.peers.get(&id).is_none() {
            peers.peers.push(id, PeerState::new(PeerStatus::Pending));
        }
    }
}

pub fn check_peer_states(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    peers_lock: &RwLock<PeerList>,
) -> Fallible<()> {
    use PeerStatus::*;

    // take advantage of the priority queue ordering
    let priority_peer = read_or_die!(peers_lock)
        .peers
        .peek()
        .map(|(&i, s)| (i.to_owned(), *s));

    if let Some((id, state)) = priority_peer {
        match state.status {
            CatchingUp => {
                // don't send any catch-up statuses while
                // there are peers that are catching up
                if get_current_stamp() > read_or_die!(peers_lock).catch_up_stamp + MAX_CATCH_UP_TIME
                {
                    debug!("Global state: peer {:016x} took too long to catch up", id);
                    if let Some(token) = node
                        .find_connection_by_id(P2PNodeId(id))
                        .map(|conn| conn.token)
                    {
                        node.remove_connection(token);
                    }
                }
            }
            Pending => {
                // send a catch-up message to the first Pending peer
                debug!("Global state: I need to catch up with peer {:016x}", id);
                send_catch_up_status(node, network_id, consensus, &peers_lock, id)?;
            }
            UpToDate => {
                consensus.start_baker();
            }
        }
    }

    Ok(())
}

fn update_peer_states(
    peers_lock: &RwLock<PeerList>,
    request: &ConsensusMessage,
    consensus_result: ConsensusFfiResponse,
) {
    use PeerStatus::*;

    let source_peer = request.source_peer();
    let mut peers = write_or_die!(peers_lock);
    if request.variant == CatchUpStatus {
        if consensus_result.is_successful() {
            peers.peers.push(source_peer, PeerState::new(UpToDate));
        } else if consensus_result.is_pending() {
            peers.peers.push(source_peer, PeerState::new(Pending));
        } else if consensus_result == ConsensusFfiResponse::ContinueCatchUp {
            peers
                .peers
                .change_priority_by(&source_peer, |state| match state.status {
                    UpToDate => PeerState::new(Pending),
                    _ => state,
                });
        }
    } else if [Block, FinalizationRecord].contains(&request.variant) {
        match request.distribution_mode() {
            DistributionMode::Direct if consensus_result.is_successful() => {
                let up_to_date_peers = peers
                    .peers
                    .iter()
                    .filter(|(_, &state)| state.status == UpToDate)
                    .map(|(&id, _)| id)
                    .collect::<Vec<_>>();

                for up_to_date_peer in up_to_date_peers {
                    peers
                        .peers
                        .change_priority(&up_to_date_peer, PeerState::new(Pending));
                }
            }
            DistributionMode::Broadcast if consensus_result.is_pending() => {
                peers.peers.push(source_peer, PeerState::new(Pending));
            }
            _ => {}
        }
    }
}
