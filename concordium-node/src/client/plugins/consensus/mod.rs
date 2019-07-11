pub const PAYLOAD_TYPE_LENGTH: u64 = 2;
pub const FILE_NAME_GENESIS_DATA: &str = "genesis.dat";
pub const FILE_NAME_PREFIX_BAKER_PRIVATE: &str = "baker_private_";
pub const FILE_NAME_SUFFIX_BAKER_PRIVATE: &str = ".dat";

use byteorder::{ByteOrder, LittleEndian, NetworkEndian, ReadBytesExt, WriteBytesExt};
use failure::Fallible;

use std::{
    collections::HashMap,
    convert::TryFrom,
    fs::OpenOptions,
    io::{Read, Write},
    mem,
    sync::{Arc, RwLock},
};

use concordium_common::{
    cache::Cache,
    stats_export_service::StatsExportService,
    PacketType::{self, *},
    RelayOrStopEnvelope, RelayOrStopSender, UCursor,
};

use concordium_consensus::{consensus, ffi};

use concordium_global_state::{
    block::{Delta, PendingBlock},
    common::{sha256, HashBytes, SerializeToBytes, SHA256},
    finalization::{FinalizationIndex, FinalizationRecord},
    transaction::Transaction,
    tree::{CatchupState, Skov, ConsensusMessage, SkovReqBody, SkovResult},
};

use crate::{common::P2PNodeId, configuration, network::NetworkId, p2p::*};

pub fn start_consensus_layer(
    conf: &configuration::BakerConfig,
    app_prefs: &configuration::AppPreferences,
) -> Option<consensus::ConsensusContainer> {
    conf.baker_id.and_then(|baker_id| {
        // Check for invalid configuration
        if baker_id > conf.baker_num_bakers {
            // Baker ID is higher than amount of bakers in the network. Bail!
            error!("Baker ID is higher than the number of bakers in the network! Disabling baking");
            return None;
        }

        info!("Starting up baker thread");
        #[cfg(feature = "profiling")]
        ffi::start_haskell(&conf.heap_profiling, conf.time_profiling);
        #[cfg(not(feature = "profiling"))]
        ffi::start_haskell();

        match get_baker_data(app_prefs, conf) {
            Ok((genesis_data, private_data)) => {
                let mut consensus_runner = consensus::ConsensusContainer::default();
                consensus_runner.start_baker(baker_id, genesis_data, private_data);

                Some(consensus_runner)
            }
            Err(_) => {
                error!("Can't read needed data...");
                None
            }
        }
    })
}

fn get_baker_data(
    app_prefs: &configuration::AppPreferences,
    conf: &configuration::BakerConfig,
) -> Fallible<(Vec<u8>, Vec<u8>)> {
    let mut genesis_loc = app_prefs.get_user_app_dir();
    genesis_loc.push(FILE_NAME_GENESIS_DATA);

    let mut private_loc = app_prefs.get_user_app_dir();

    if let Some(baker_id) = conf.baker_id {
        private_loc.push(format!(
            "{}{}{}",
            FILE_NAME_PREFIX_BAKER_PRIVATE, baker_id, FILE_NAME_SUFFIX_BAKER_PRIVATE
        ))
    };

    let (generated_genesis, generated_private_data) =
        if !genesis_loc.exists() || !private_loc.exists() {
            consensus::ConsensusContainer::generate_data(conf.baker_genesis, conf.baker_num_bakers)?
        } else {
            (vec![], HashMap::new())
        };

    let given_genesis = if !genesis_loc.exists() {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&genesis_loc)
        {
            Ok(mut file) => match file.write_all(&generated_genesis) {
                Ok(_) => generated_genesis,
                Err(_) => bail!("Couldn't write out genesis data"),
            },
            Err(_) => bail!("Couldn't open up genesis file for writing"),
        }
    } else {
        match OpenOptions::new().read(true).open(&genesis_loc) {
            Ok(mut file) => {
                let mut read_data = vec![];
                match file.read_to_end(&mut read_data) {
                    Ok(_) => read_data,
                    Err(_) => bail!("Couldn't read genesis file properly"),
                }
            }
            Err(e) => bail!("Can't open the genesis file ({})!", e),
        }
    };

    let given_private_data = if !private_loc.exists() {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&private_loc)
        {
            Ok(mut file) => {
                if let Some(baker_id) = conf.baker_id {
                    match file.write_all(&generated_private_data[&(baker_id as i64)]) {
                        Ok(_) => generated_private_data[&(baker_id as i64)].to_owned(),
                        Err(_) => bail!("Couldn't write out private baker data"),
                    }
                } else {
                    bail!("Couldn't write out private baker data");
                }
            }
            Err(_) => bail!("Couldn't open up private baker file for writing"),
        }
    } else {
        match OpenOptions::new().read(true).open(&private_loc) {
            Ok(mut file) => {
                let mut read_data = vec![];
                match file.read_to_end(&mut read_data) {
                    Ok(_) => read_data,
                    Err(_) => bail!("Couldn't open up private baker file for reading"),
                }
            }
            Err(e) => bail!("Can't open the private data file ({})!", e),
        }
    };

    debug!(
        "Obtained genesis data {:?}",
        sha256(&[&[0u8; 8], given_genesis.as_slice()].concat())
    );

    Ok((given_genesis, given_private_data))
}

/// Handles packets coming from other peers
pub fn handle_pkt_out(
    _baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    mut msg: UCursor,
    skov_sender: &RelayOrStopSender<ConsensusMessage>,
    _transactions_cache: &Cache<Transaction>,
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
    let payload = Arc::from(&view.as_slice()[PAYLOAD_TYPE_LENGTH as usize..]);

    let request = RelayOrStopEnvelope::Relay(ConsensusMessage::new(
        Some((peer_id.0, is_broadcast)),
        packet_type,
        payload,
    ));

    skov_sender.send(request)?;

    Ok(())
}

pub fn handle_global_state_request(
    node: &mut P2PNode,
    network_id: NetworkId,
    baker: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
    skov: &mut Skov,
    stats_exporting: &Option<Arc<RwLock<StatsExportService>>>,
) -> Fallible<()> {
    let is_from_consensus = request.source.is_none();

    if is_from_consensus {
        process_internal_skov_entry(node, network_id, request, skov)?
    } else {
        process_external_skov_entry(node, network_id, baker, request, skov)?
    }

    if let Some(stats) = stats_exporting {
        if let Ok(mut stats) = safe_write!(stats) {
            let stats_values = skov.stats.query_stats();
            stats.set_skov_block_receipt(stats_values.0 as i64);
            stats.set_skov_block_entry(stats_values.1 as i64);
            stats.set_skov_block_query(stats_values.2 as i64);
            stats.set_skov_finalization_receipt(stats_values.3 as i64);
            stats.set_skov_finalization_entry(stats_values.4 as i64);
            stats.set_skov_finalization_query(stats_values.5 as i64);
        }
    }

    Ok(())
}

fn process_internal_skov_entry(
    node: &mut P2PNode,
    network_id: NetworkId,
    request: ConsensusMessage,
    skov: &mut Skov,
) -> Fallible<()> {
    if skov.catchup_state() == CatchupState::InProgress && request.variant == PacketType::Block {
        // ignore outgoing blocks until the catch-up phase is complete; they would be
        // dropped by the other nodes anyway
        warn!("Our consensus layer wanted to bake a block while catching up! Not relaying it.");
        return Ok(());
    }

    let (entry_info, skov_result) = match request.variant {
        PacketType::Block => {
            let block = PendingBlock::new(&request.payload)?;
            (format!("{:?}", block.block), skov.add_block(block))
        },
        PacketType::FinalizationRecord => {
            let record = FinalizationRecord::deserialize(&request.payload)?;
            (format!("{:?}", record), skov.add_finalization(record))
        }
        PacketType::CatchupBlockByHash
        | PacketType::CatchupFinalizationRecordByHash
        | PacketType::CatchupFinalizationRecordByIndex => {
            let skov_result = if skov.catchup_state() != CatchupState::InProgress {
                skov.start_catchup_round()
            } else {
                SkovResult::IgnoredEntry
            };
            (request.variant.to_string(), skov_result)
        },
        _ => (request.variant.to_string(), SkovResult::IgnoredEntry),
    };

    match skov_result {
        SkovResult::SuccessfulEntry(entry) => {
            trace!(
                "Skov: successfully processed a {} from our consensus layer",
                entry
            );
        }
        SkovResult::IgnoredEntry => {
            trace!(
                "Skov: ignoring a {} from our consensus layer",
                request.variant
            );
        }
        SkovResult::Error(e) => skov.register_error(e),
        _ => {}
    }

    let mut out_bytes =
        Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize + request.payload.len());

    match out_bytes.write_u16::<NetworkEndian>(request.variant as u16) {
        Ok(_) => {
            out_bytes.extend(&*request.payload);

            let res = if let Some((peer_id, _)) = request.source {
                node.send_direct_message(
                    Some(P2PNodeId(peer_id)),
                    network_id,
                    None,
                    out_bytes,
                )
            } else {
                node.send_broadcast_message(None, network_id, None, out_bytes)
            };

            let msg_metadata = if let Some((tgt, _)) = request.source {
                format!("direct message to peer {}", P2PNodeId(tgt))
            } else {
                "broadcast".to_string()
            };

            match res {
                Ok(_) => info!(
                    "Peer {} sent a {} containing a {}",
                    node.id(),
                    msg_metadata,
                    entry_info,
                ),
                Err(_) => error!(
                    "Peer {} couldn't send a {} containing a {}!",
                    node.id(),
                    msg_metadata,
                    entry_info,
                ),
            }
        }
        Err(_) => error!("Can't write a consensus message type to a packet"),
    }

    Ok(())
}

fn process_external_skov_entry(
    node: &mut P2PNode,
    network_id: NetworkId,
    baker: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
    skov: &mut Skov,
) -> Fallible<()> {
    if skov.catchup_state() == CatchupState::InProgress {
        // delay broadcasts during catch-up rounds
        if let Some((peer_id, is_broadcast)) = request.source {
            if is_broadcast {
                info!(
                    "Still catching up; the last received broadcast containing a {} \
                     will be processed after it's finished",
                    request,
                );
                // TODO: this check might not be needed; verify
                if P2PNodeId(peer_id) != node.id() {
                    skov.delay_broadcast(request);
                }
                return Ok(());
            }
        }
    }

    let (request_body, consensus_applicable) = match request.variant {
        PacketType::Block => {
            let payload = PendingBlock::new(&request.payload)?;
            let body = Some(SkovReqBody::AddBlock(payload));
            (body, true)
        }
        PacketType::FinalizationRecord => {
            let payload = FinalizationRecord::deserialize(&request.payload)?;
            let body = Some(SkovReqBody::AddFinalizationRecord(payload));
            (body, true)
        }
        PacketType::CatchupBlockByHash => {
            let hash = HashBytes::new(&request.payload[..SHA256 as usize]);
            let delta = LittleEndian::read_u64(
                &request.payload[SHA256 as usize..][..mem::size_of::<Delta>()],
            );

            (Some(SkovReqBody::GetBlock(hash, delta)), false)
        }
        PacketType::CatchupFinalizationRecordByHash => {
            let payload = HashBytes::new(&request.payload);
            let body = Some(SkovReqBody::GetFinalizationRecordByHash(payload));
            (body, false)
        }
        PacketType::CatchupFinalizationRecordByIndex => {
            let idx =
                LittleEndian::read_u64(&request.payload[..mem::size_of::<FinalizationIndex>()]);
            let body = Some(SkovReqBody::GetFinalizationRecordByIdx(idx));
            (body, false)
        }
        _ => (None, true), // will be expanded later on
    };

    if let Some(req_body) = request_body {
        let result = match req_body {
            SkovReqBody::AddBlock(pending_block) => skov.add_block(pending_block),
            SkovReqBody::AddFinalizationRecord(record) => skov.add_finalization(record),
            SkovReqBody::GetBlock(hash, delta) => skov.get_block(&hash, delta),
            SkovReqBody::GetFinalizationRecordByHash(hash) => {
                skov.get_finalization_record_by_hash(&hash)
            }
            SkovReqBody::GetFinalizationRecordByIdx(i) => skov.get_finalization_record_by_idx(i),
            _ => SkovResult::Housekeeping,
        };

        match result {
            SkovResult::SuccessfulEntry(_) => {
                trace!("Peer {} successfully processed a {}", node.id(), request);
            }
            SkovResult::SuccessfulQuery(result) => {
                let return_type = match request.variant {
                    PacketType::CatchupBlockByHash => PacketType::Block,
                    PacketType::CatchupFinalizationRecordByHash => PacketType::FinalizationRecord,
                    PacketType::CatchupFinalizationRecordByIndex => PacketType::FinalizationRecord,
                    _ => unreachable!("Impossible packet type in a query result!"),
                };

                let mut out_bytes = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize + result.len());
                out_bytes
                    .write_u16::<NetworkEndian>(return_type as u16)
                    .expect("Can't write to buffer");
                out_bytes.extend(&*result);

                let source = request
                    .source
                    .map(|(peer_id, ..)| P2PNodeId(peer_id))
                    .unwrap();

                match node.send_direct_message(Some(source), network_id, None, out_bytes) {
                    Ok(_) => info!("Peer {} responded to a {}", node.id(), request),
                    Err(_) => error!("Peer {} couldn't respond to a {}!", node.id(), request),
                }
            }
            SkovResult::DuplicateEntry => {
                warn!("Skov: got a duplicate {}", request);
            }
            SkovResult::Error(e) => skov.register_error(e),
            _ => {}
        }

        // relay external messages to Consensus if they are relevant to it
        if request.source.is_some() {
            if consensus_applicable {
                send_msg_to_consensus(node.id(), baker, request)?
            }
        }

        if let CatchupState::InProgress = skov.catchup_state() {
            if skov.is_tree_valid() {
                skov.end_catchup_round();
                apply_delayed_broadcasts(node, network_id, baker, skov)?;
            }
        }
    } else {
        // relay external messages to Consensus if they are relevant to it
        if request.source.is_some() {
            if consensus_applicable {
                send_msg_to_consensus(node.id(), baker, request)?
            }
        }

        // not handled in Skov yet (FinalizationMessages)
    }

    Ok(())
}

pub fn apply_delayed_broadcasts(
    node: &mut P2PNode,
    network_id: NetworkId,
    baker: &mut consensus::ConsensusContainer,
    skov: &mut Skov,
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
    our_node_id: P2PNodeId,
    baker: &mut consensus::ConsensusContainer,
    request: ConsensusMessage,
) -> Fallible<()> {
    let raw_id = request.source.unwrap().0; // safe, always there in external messages

    let consensus_response = match request.variant {
        Block => baker.send_block(raw_id, &request.payload),
        Transaction => baker.send_transaction(&request.payload),
        FinalizationMessage => baker.send_finalization(raw_id, &request.payload),
        FinalizationRecord => baker.send_finalization_record(raw_id, &request.payload),
        CatchupFinalizationMessagesByPoint => {
            baker.get_finalization_messages(raw_id, &request.payload)
        }
        _ => unreachable!("Impossible! A Skov-only request was passed on to consensus"),
    };

    if consensus_response.is_acceptable() {
        info!(
            "Peer {} processed a {}",
            our_node_id,
            request,
        );
    } else {
        error!(
            "Peer {} couldn't process a {} due to error code {:?}",
            our_node_id,
            request,
            consensus_response,
        );
    }

    Ok(())
}
