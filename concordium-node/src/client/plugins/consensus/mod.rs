pub const PAYLOAD_TYPE_LENGTH: u64 = 2;
pub const FILE_NAME_GENESIS_DATA: &str = "genesis.dat";
pub const FILE_NAME_CRYPTO_PROV_DATA: &str = "crypto_providers.json";
pub const FILE_NAME_ID_PROV_DATA: &str = "identity_providers.json";
pub const FILE_NAME_PREFIX_BAKER_PRIVATE: &str = "baker-";
pub const FILE_NAME_SUFFIX_BAKER_PRIVATE: &str = ".dat";

use byteorder::{ByteOrder, NetworkEndian, ReadBytesExt, WriteBytesExt};
use failure::Fallible;

use std::{
    convert::TryFrom,
    fs::OpenOptions,
    io::{Cursor, Read},
    mem,
    sync::Arc,
};

use concordium_common::{
    cache::Cache,
    stats_export_service::StatsExportService,
    ConsensusFfiResponse,
    PacketType::{self, *},
    RelayOrStopEnvelope, RelayOrStopSender, UCursor,
};

use concordium_consensus::{consensus, ffi};

use concordium_global_state::{
    block::{BlockHeight, PendingBlock},
    common::{sha256, SerializeToBytes},
    finalization::FinalizationRecord,
    transaction::{Transaction, TransactionHash},
    tree::{
        messaging::{
            ConsensusMessage, DistributionMode, GlobalMetadata, GlobalStateError,
            GlobalStateResult, MessageType,
        },
        GlobalState, ProcessingState,
    },
};

use crate::{common::P2PNodeId, configuration, network::NetworkId, p2p::p2p_node::*};

pub fn start_consensus_layer(
    conf: &configuration::BakerConfig,
    app_prefs: &configuration::AppPreferences,
) -> Option<consensus::ConsensusContainer> {
    info!("Starting up the consensus thread");

    #[cfg(feature = "profiling")]
    ffi::start_haskell(
        &conf.heap_profiling,
        conf.time_profiling,
        conf.backtraces_profiling,
        conf.gc_logging.clone(),
    );
    #[cfg(not(feature = "profiling"))]
    ffi::start_haskell();

    match get_baker_data(app_prefs, conf, conf.baker_id.is_some()) {
        Ok((genesis_data, private_data)) => {
            let consensus =
                consensus::ConsensusContainer::new(genesis_data, private_data, conf.baker_id);
            Some(consensus)
        }
        Err(_) => {
            error!("Can't start the consensus layer!");
            None
        }
    }
}

fn get_baker_data(
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
    dont_relay_to: Vec<P2PNodeId>,
    peer_id: P2PNodeId,
    mut msg: UCursor,
    skov_sender: &RelayOrStopSender<ConsensusMessage>,
    transactions_cache: &mut Cache<Arc<[u8]>>,
    is_broadcast: bool,
) -> Fallible<()> {
    ensure!(
        msg.len() >= msg.position() + PAYLOAD_TYPE_LENGTH,
        "Message needs at least {} bytes",
        PAYLOAD_TYPE_LENGTH
    );

    let consensus_type = msg.read_u16::<NetworkEndian>()?;
    let packet_type = PacketType::try_from(consensus_type)?;

    let view = msg.read_all_into_view()?;
    let payload: Arc<[u8]> = Arc::from(&view.as_slice()[PAYLOAD_TYPE_LENGTH as usize..]);
    let distribution_mode = if is_broadcast {
        DistributionMode::Broadcast
    } else {
        DistributionMode::Direct
    };

    if packet_type == PacketType::Transaction {
        let hash_offset = payload.len() - mem::size_of::<TransactionHash>();
        let hash = TransactionHash::new(&payload[hash_offset..]);
        transactions_cache.insert(hash, payload.clone());
    }

    let request = RelayOrStopEnvelope::Relay(ConsensusMessage::new(
        MessageType::Inbound(peer_id.0, distribution_mode),
        packet_type,
        payload,
        dont_relay_to.into_iter().map(P2PNodeId::as_raw).collect(),
    ));

    skov_sender.send(request)?;

    Ok(())
}

pub fn handle_global_state_request(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
    skov: &mut GlobalState,
    stats_exporting: &Option<StatsExportService>,
) -> Fallible<()> {
    if let MessageType::Outbound(_) = request.direction {
        process_internal_skov_entry(node, network_id, request, skov)?
    } else {
        process_external_skov_entry(node, network_id, consensus, request, skov)?
    }

    if let Some(stats) = stats_exporting {
        let stats_values = skov.stats.query_stats();
        stats.set_skov_block_receipt(stats_values.0 as i64);
        stats.set_skov_block_entry(stats_values.1 as i64);
        stats.set_skov_block_query(stats_values.2 as i64);
        stats.set_skov_finalization_receipt(stats_values.3 as i64);
        stats.set_skov_finalization_entry(stats_values.4 as i64);
        stats.set_skov_finalization_query(stats_values.5 as i64);
    }

    Ok(())
}

fn process_internal_skov_entry(
    node: &P2PNode,
    network_id: NetworkId,
    mut request: ConsensusMessage,
    skov: &mut GlobalState,
) -> Fallible<()> {
    let (entry_info, skov_result) = match request.variant {
        PacketType::Block => {
            let block = PendingBlock::new(&request.payload)?;
            (format!("{:?}", block.block), skov.add_block(block))
        }
        PacketType::FinalizationRecord => {
            let record = FinalizationRecord::deserialize(&request.payload)?;
            (format!("{:?}", record), skov.add_finalization(record))
        }
        PacketType::Transaction => {
            let transaction = Transaction::deserialize(&mut Cursor::new(&request.payload))?;
            (
                format!("{:?}", transaction.payload.transaction_type()),
                skov.add_transaction(transaction, false),
            )
        }
        PacketType::CatchupBlockByHash
        | PacketType::CatchupFinalizationRecordByHash
        | PacketType::CatchupFinalizationRecordByIndex => {
            error!(
                "Consensus should not be missing any data, yet it wants a {:?}!",
                request.variant
            );
            return Ok(());
        }
        _ => (request.variant.to_string(), GlobalStateResult::IgnoredEntry),
    };

    match skov_result {
        GlobalStateResult::SuccessfulEntry(entry) => {
            trace!(
                "GlobalState: successfully processed a {} from our consensus layer",
                entry
            );
        }
        GlobalStateResult::IgnoredEntry => {
            trace!(
                "GlobalState: ignoring a {} from our consensus layer",
                request.variant
            );
        }
        GlobalStateResult::Error(e) => skov.register_error(e),
        _ => {}
    }

    send_consensus_msg_to_net(
        node,
        request.dont_relay_to(),
        request.target_peer().map(P2PNodeId),
        network_id,
        request.variant,
        Some(entry_info),
        &request.payload,
    );

    Ok(())
}

fn process_external_skov_entry(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
    skov: &mut GlobalState,
) -> Fallible<()> {
    let self_node_id = node.self_peer.id;
    let source = P2PNodeId(request.source_peer());

    if skov.is_catching_up() {
        if skov.is_broadcast_delay_acceptable() {
            // delay broadcasts during catch-up rounds
            if request.distribution_mode() == DistributionMode::Broadcast {
                info!(
                    "Still catching up; the last received broadcast containing a {} will be \
                     processed after it's finished",
                    request,
                );
                // TODO: this check might not be needed; verify
                if source != self_node_id {
                    skov.delay_broadcast(request);
                }
                return Ok(());
            }
        } else {
            warn!("The catch-up round was taking too long; resuming regular state");
            conclude_catch_up_round(node, network_id, consensus, skov)?;
        }
    }

    let (skov_result, consensus_applicable) = match request.variant {
        PacketType::Block => {
            let block = PendingBlock::new(&request.payload)?;
            let skov_result = skov.add_block(block);
            (skov_result, true)
        }
        PacketType::FinalizationRecord => {
            let record = FinalizationRecord::deserialize(&request.payload)?;
            let skov_result = skov.add_finalization(record);
            (skov_result, true)
        }
        PacketType::Transaction => {
            let transaction = Transaction::deserialize(&mut Cursor::new(&request.payload))?;
            let skov_result = skov.add_transaction(transaction, false);
            (skov_result, true)
        }
        PacketType::GlobalStateMetadata => {
            let skov_result = if skov.peer_metadata.get(&source.0).is_none() {
                let metadata = GlobalMetadata::deserialize(&request.payload)?;
                skov.register_peer_metadata(request.source_peer(), metadata)
            } else {
                GlobalStateResult::IgnoredEntry
            };
            (skov_result, false)
        }
        PacketType::GlobalStateMetadataRequest => (skov.get_serialized_metadata(), false),
        PacketType::FullCatchupRequest => {
            let since = NetworkEndian::read_u64(&request.payload[..8]);
            send_catch_up_response(node, &skov, source, network_id, since);
            (
                GlobalStateResult::SuccessfulEntry(PacketType::FullCatchupRequest),
                false,
            )
        }
        PacketType::FullCatchupComplete => (
            GlobalStateResult::SuccessfulEntry(PacketType::FullCatchupComplete),
            false,
        ),
        _ => (GlobalStateResult::IgnoredEntry, true), // will be expanded later on
    };

    // relay external messages to Consensus if they are relevant to it
    let consensus_result = if consensus_applicable {
        Some(send_msg_to_consensus(
            self_node_id,
            source,
            consensus,
            &request,
        )?)
    } else {
        None
    };

    match skov_result {
        GlobalStateResult::SuccessfulEntry(entry_type) => {
            trace!(
                "Peer {} successfully processed a {}",
                node.self_peer.id,
                request
            );

            // reply to peer metadata with own metadata and begin catching up and/or baking
            match entry_type {
                PacketType::GlobalStateMetadata => {
                    let response_metadata = skov.get_metadata().serialize();

                    send_consensus_msg_to_net(
                        &node,
                        vec![],
                        Some(source),
                        network_id,
                        PacketType::GlobalStateMetadata,
                        Some(request.variant.to_string()),
                        &response_metadata,
                    );

                    if skov.state() == ProcessingState::JustStarted {
                        if let GlobalStateResult::BestPeer((best_peer, best_meta)) =
                            skov.best_metadata()
                        {
                            if best_meta.is_usable() {
                                send_catch_up_request(node, P2PNodeId(best_peer), network_id, 0);
                                skov.start_catchup_round(ProcessingState::FullyCatchingUp);
                            } else {
                                consensus.start_baker();
                                skov.data.state = ProcessingState::Complete;
                            }
                        }

                        request_finalization_messages(node, consensus, source, network_id);
                    }
                }
                PacketType::FullCatchupComplete => {
                    conclude_catch_up_round(node, network_id, consensus, skov)?;
                }
                _ => {
                    consensus_driven_rebroadcast(node, network_id, consensus_result, request, skov)
                }
            }
        }
        GlobalStateResult::SuccessfulQuery(result) => {
            let return_type = match request.variant {
                PacketType::GlobalStateMetadataRequest => PacketType::GlobalStateMetadata,
                _ => unreachable!("Impossible packet type in a query result!"),
            };

            let msg_desc = if skov.state() == ProcessingState::JustStarted
                && request.variant == PacketType::GlobalStateMetadataRequest
            {
                return_type.to_string()
            } else {
                format!("response to a {}", request.variant)
            };

            send_consensus_msg_to_net(
                &node,
                vec![],
                Some(source),
                network_id,
                return_type,
                Some(msg_desc),
                &result,
            );
        }
        GlobalStateResult::DuplicateEntry => {
            warn!("GlobalState: got a duplicate {}", request);
            return Ok(());
        }
        GlobalStateResult::Error(err) => {
            match err {
                GlobalStateError::MissingParentBlock(..)
                | GlobalStateError::MissingLastFinalizedBlock(..)
                | GlobalStateError::LastFinalizedNotFinalized(..)
                | GlobalStateError::MissingBlockToFinalize(..) => {
                    let curr_height = skov.data.get_last_finalized_height();
                    send_catch_up_request(node, source, network_id, curr_height);
                    skov.start_catchup_round(ProcessingState::FullyCatchingUp);
                }
                _ => {}
            }
            skov.register_error(err);
        }
        GlobalStateResult::IgnoredEntry if request.variant == PacketType::FinalizationMessage => {
            consensus_driven_rebroadcast(node, network_id, consensus_result, request, skov)
        }
        _ => {}
    }

    if skov.state() == ProcessingState::PartiallyCatchingUp && skov.is_tree_valid() {
        conclude_catch_up_round(node, network_id, consensus, skov)?;
    }

    Ok(())
}

fn consensus_driven_rebroadcast(
    node: &P2PNode,
    network_id: NetworkId,
    consensus_result: Option<ConsensusFfiResponse>,
    mut request: ConsensusMessage,
    skov: &mut GlobalState,
) {
    if let Some(consensus_result) = consensus_result {
        if !skov.is_catching_up() && consensus_result.is_rebroadcastable() {
            send_consensus_msg_to_net(
                &node,
                request.dont_relay_to(),
                None,
                network_id,
                request.variant,
                None,
                &request.payload,
            );
        }
    }
}

pub fn apply_delayed_broadcasts(
    node: &P2PNode,
    network_id: NetworkId,
    baker: &mut consensus::ConsensusContainer,
    skov: &mut GlobalState,
) -> Fallible<()> {
    let delayed_broadcasts = skov.get_delayed_broadcasts();

    if delayed_broadcasts.is_empty() {
        return Ok(());
    }

    info!("Applying {} delayed broadcast(s)", delayed_broadcasts.len());

    for request in delayed_broadcasts {
        process_external_skov_entry(node, network_id, baker, request, skov)?;
    }

    info!("Delayed broadcasts were applied");

    Ok(())
}

fn send_msg_to_consensus(
    our_id: P2PNodeId,
    source_id: P2PNodeId,
    consensus: &mut consensus::ConsensusContainer,
    request: &ConsensusMessage,
) -> Fallible<ConsensusFfiResponse> {
    let raw_id = source_id.as_raw();

    let consensus_response = match request.variant {
        Block => consensus.send_block(raw_id, &request.payload),
        Transaction => consensus.send_transaction(&request.payload),
        FinalizationMessage => consensus.send_finalization(raw_id, &request.payload),
        FinalizationRecord => consensus.send_finalization_record(raw_id, &request.payload),
        CatchupFinalizationMessagesByPoint => {
            consensus.get_finalization_messages(&request.payload, raw_id)
        }
        _ => unreachable!("Impossible! A GlobalState-only request was passed on to consensus"),
    };

    if consensus_response.is_acceptable() {
        info!("Peer {} processed a {}", our_id, request,);
    } else {
        error!(
            "Peer {} couldn't process a {} due to error code {:?}",
            our_id, request, consensus_response,
        );
    }

    Ok(consensus_response)
}

pub fn send_consensus_msg_to_net(
    node: &P2PNode,
    dont_relay_to: Vec<u64>,
    target_id: Option<P2PNodeId>,
    network_id: NetworkId,
    payload_type: PacketType,
    payload_desc: Option<String>,
    payload: &[u8],
) {
    let self_node_id = node.self_peer.id;
    let mut packet_buffer = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize + payload.len());
    packet_buffer
        .write_u16::<NetworkEndian>(payload_type as u16)
        .expect("Can't write a packet payload to buffer");
    packet_buffer.extend(payload);

    let result = if target_id.is_some() {
        send_direct_message(node, target_id, network_id, None, packet_buffer)
    } else {
        send_broadcast_message(
            node,
            dont_relay_to.into_iter().map(P2PNodeId).collect(),
            network_id,
            None,
            packet_buffer,
        )
    };

    let target_desc = if let Some(id) = target_id {
        format!("direct message to peer {}", id)
    } else {
        "broadcast".to_string()
    };
    let message_desc = payload_desc.unwrap_or_else(|| payload_type.to_string());

    match result {
        Ok(_) => info!(
            "Peer {} sent a {} containing a {}",
            self_node_id, target_desc, message_desc,
        ),
        Err(_) => error!(
            "Peer {} couldn't send a {} containing a {}!",
            self_node_id, target_desc, message_desc,
        ),
    }
}

fn request_finalization_messages(
    node: &P2PNode,
    consensus: &consensus::ConsensusContainer,
    target: P2PNodeId,
    network: NetworkId,
) {
    let response = consensus.get_finalization_point();

    send_consensus_msg_to_net(
        node,
        vec![],
        Some(target),
        network,
        PacketType::CatchupFinalizationMessagesByPoint,
        None,
        &response,
    );
}

fn send_catch_up_request(
    node: &P2PNode,
    target: P2PNodeId,
    network: NetworkId,
    since: BlockHeight,
) {
    let packet_type = PacketType::FullCatchupRequest;
    let mut buffer = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize);
    buffer
        .write_u16::<NetworkEndian>(packet_type as u16)
        .and_then(|_| buffer.write_u64::<NetworkEndian>(since))
        .expect("Can't write a packet payload to buffer");

    let result = send_direct_message(node, Some(target), network, None, buffer);

    match result {
        Ok(_) => info!(
            "Peer {} sent a direct {} to peer {}",
            node.self_peer.id, packet_type, target,
        ),
        Err(_) => error!(
            "Peer {} couldn't send a direct {} to peer {}!",
            node.self_peer.id, packet_type, target,
        ),
    }
}

fn send_catch_up_response(
    node: &P2PNode,
    skov: &GlobalState,
    target: P2PNodeId,
    network: NetworkId,
    since: BlockHeight,
) {
    for (block, fin_rec) in skov.iter_tree_since(since) {
        send_consensus_msg_to_net(
            &node,
            vec![],
            Some(target),
            network,
            PacketType::Block,
            None,
            &block.serialize(),
        );
        if let Some(rec) = fin_rec {
            send_consensus_msg_to_net(
                &node,
                vec![],
                Some(target),
                network,
                PacketType::FinalizationRecord,
                None,
                &rec.serialize(),
            );
        }
    }

    let mut blob = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize);
    let packet_type = PacketType::FullCatchupComplete;
    blob.write_u16::<NetworkEndian>(packet_type as u16)
        .expect("Can't write a packet payload to buffer");

    send_consensus_msg_to_net(
        &node,
        vec![],
        Some(target),
        network,
        packet_type,
        None,
        &blob,
    );
}

fn conclude_catch_up_round(
    node: &P2PNode,
    network_id: NetworkId,
    consensus: &mut consensus::ConsensusContainer,
    skov: &mut GlobalState,
) -> Fallible<()> {
    skov.end_catchup_round();
    apply_delayed_broadcasts(node, network_id, consensus, skov)?;

    if !consensus.is_baking() {
        consensus.start_baker();
    }

    Ok(())
}