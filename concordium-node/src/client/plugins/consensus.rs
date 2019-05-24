pub const PAYLOAD_TYPE_LENGTH: u64 = 2;
pub const FILE_NAME_GENESIS_DATA: &str = "genesis.dat";
pub const FILE_NAME_PREFIX_BAKER_PRIVATE: &str = "baker_private_";
pub const FILE_NAME_SUFFIX_BAKER_PRIVATE: &str = ".dat";

use byteorder::{ByteOrder, NetworkEndian, ReadBytesExt, WriteBytesExt};
use failure::Fallible;

use std::{
    collections::HashMap,
    convert::TryFrom,
    fs::OpenOptions,
    io::{Read, Write},
};

use concordium_common::{safe_read, safe_write, UCursor};

use concordium_consensus::{
    consensus,
    ffi::{
        self,
        PacketType::{self, *},
    },
};

use concordium_global_state::{
    block::{BakedBlock, BlockPtr, PendingBlock},
    common::{sha256, HashBytes, SerializeToBytes, SHA256},
    finalization::{FinalizationMessage, FinalizationRecord},
    tree::SKOV_DATA,
};

use crate::{
    common::{P2PNodeId, PacketDirection},
    configuration,
    network::NetworkId,
    p2p::*,
};

pub fn start_baker(
    node: &P2PNode,
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
        ffi::start_haskell();

        match get_baker_data(app_prefs, conf) {
            Ok((genesis_data, private_data)) => {
                let genesis_ptr = BlockPtr::genesis(&genesis_data);
                info!(
                    "Peer {} has genesis data with hash {:?} and block hash {:?}",
                    node.id(),
                    sha256(&genesis_data),
                    genesis_ptr.hash,
                );
                safe_write!(SKOV_DATA)
                    .expect("Couldn't write the genesis data to Skov!")
                    .add_genesis(genesis_ptr);

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
            Err(_e) => bail!("Can't open the genesis file!"),
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
            Err(_e) => bail!("Can't open the private data file!"),
        }
    };

    Ok((given_genesis, given_private_data))
}

pub fn handle_pkt_out(
    node: &mut P2PNode,
    baker: &mut Option<consensus::ConsensusContainer>,
    peer_id: P2PNodeId,
    network_id: NetworkId,
    mut msg: UCursor,
) -> Fallible<()> {
    use concordium_global_state::{common::DELTA_LENGTH, tree::SKOV_DATA};

    if let Some(ref mut baker) = baker {
        ensure!(
            msg.len() >= msg.position() + PAYLOAD_TYPE_LENGTH,
            "Message needs at least {} bytes",
            PAYLOAD_TYPE_LENGTH
        );

        let consensus_type = msg.read_u16::<NetworkEndian>()?;
        let view = msg.read_all_into_view()?;
        let content = &view.as_slice()[PAYLOAD_TYPE_LENGTH as usize..];
        let packet_type = PacketType::try_from(consensus_type)?;

        let is_unique = match packet_type {
            Block => {
                let pending_block = PendingBlock::new(content)?;

                // don't pattern match directly in order to release the lock quickly
                let result = if let Ok(ref mut skov) = safe_write!(SKOV_DATA) {
                    skov.add_block(pending_block.clone())
                } else {
                    error!("Can't obtain a write lock on Skov!");
                    Ok(None) // temporary placeholder; we don't want to suggest a duplicate
                };

                match result {
                    Ok(Some(_)) => false,
                    Ok(None) => true,
                    Err(e) => {
                        let e = e.to_string();
                        if e == "MissingParent" {
                            let mut inner_out_bytes = Vec::with_capacity(
                                pending_block.block.pointer.len() + DELTA_LENGTH as usize,
                            );
                            inner_out_bytes.extend_from_slice(&pending_block.block.pointer);
                            inner_out_bytes
                                .write_u64::<NetworkEndian>(0u64)
                                .expect("Can't write to buffer");
                            send_catchup_request_block_by_hash_to_consensus(
                                baker,
                                node,
                                peer_id,
                                network_id,
                                &inner_out_bytes,
                                PacketDirection::Outbound,
                            )?;
                            true
                        } else {
                            true
                        }
                    }
                }
            }
            FinalizationRecord => {
                let record = FinalizationRecord::deserialize(content)?;

                if let Ok(ref mut skov) = SKOV_DATA.write() {
                    skov.add_finalization(record)
                } else {
                    error!("Can't obtain a write lock on Skov!");
                    true // temporary placeholder; we don't want to suggest a duplicate
                }
            }
            _ => true,
        };

        if !is_unique {
            warn!("Peer {} sent us a duplicate {}", peer_id, packet_type,);
        } else {
            if let Err(e) =
                send_msg_to_consensus(node, baker, peer_id, network_id, packet_type, content)
            {
                error!("Send network message to baker has failed: {:?}", e);
            }
        }
    }

    Ok(())
}

fn send_msg_to_consensus(
    node: &mut P2PNode,
    baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    network_id: NetworkId,
    packet_type: PacketType,
    content: &[u8],
) -> Fallible<()> {
    use concordium_global_state::common::DELTA_LENGTH;

    match packet_type {
        Block => send_block_to_consensus(baker, peer_id, content),
        Transaction => send_transaction_to_consensus(baker, peer_id, content),
        FinalizationMessage => send_finalization_message_to_consensus(baker, peer_id, content),
        FinalizationRecord => send_finalization_record_to_consensus(baker, peer_id, content),
        CatchupBlockByHash => {
            ensure!(
                content.len() == SHA256 as usize + DELTA_LENGTH as usize,
                "{} needs {} bytes",
                CatchupBlockByHash,
                SHA256 + DELTA_LENGTH,
            );
            send_catchup_request_block_by_hash_to_consensus(
                baker,
                node,
                peer_id,
                network_id,
                content,
                PacketDirection::Inbound,
            )
        }
        CatchupFinalizationRecordByHash => {
            ensure!(
                content.len() == SHA256 as usize,
                "{} needs {} bytes",
                CatchupFinalizationRecordByHash,
                SHA256
            );
            send_catchup_request_finalization_record_by_hash_to_consensus(
                baker,
                node,
                peer_id,
                network_id,
                content,
                PacketDirection::Inbound,
            )
        }
        CatchupFinalizationRecordByIndex => {
            ensure!(
                content.len() == 8,
                "{} needs {} bytes",
                CatchupFinalizationRecordByIndex,
                8
            );
            send_catchup_request_finalization_record_by_index_to_consensus(
                baker,
                node,
                peer_id,
                network_id,
                content,
                PacketDirection::Inbound,
            )
        }
        CatchupFinalizationMessagesByPoint => {
            send_catchup_finalization_messages_by_point_to_consensus(baker, peer_id, content)
        }
    }
}

pub fn send_transaction_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    content: &[u8],
) -> Fallible<()> {
    baker.send_transaction(content);
    info!("Peer {} sent a transaction to the consensus layer", peer_id);
    Ok(())
}

pub fn send_finalization_record_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    content: &[u8],
) -> Fallible<()> {
    let record = FinalizationRecord::deserialize(content)?;

    match baker.send_finalization_record(peer_id.as_raw(), &record) {
        0i64 => info!("Peer {} sent a {} to consensus", peer_id, record),
        err_code => error!(
            "Peer {} can't send a finalization record to consensus due to error code #{} (bytes: \
             {:?}, length: {})",
            peer_id,
            err_code,
            content,
            content.len(),
        ),
    }

    Ok(())
}

pub fn send_finalization_message_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    content: &[u8],
) -> Fallible<()> {
    let message = FinalizationMessage::deserialize(content)?;

    baker.send_finalization(peer_id.as_raw(), &message);
    info!("Peer {} sent a {} to the consensus layer", peer_id, message);

    Ok(())
}

pub fn send_block_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    content: &[u8],
) -> Fallible<()> {
    let baked_block = BakedBlock::deserialize(content)?;

    // send unique blocks to the consensus layer
    match baker.send_block(peer_id.as_raw(), &baked_block) {
        0i64 => info!(
            "Peer {} sent a block ({:?}) to consensus",
            peer_id,
            sha256(content),
        ),
        err_code => error!(
            "Peer {} can't send block from network to consensus due to error code #{} (bytes: \
             {:?}, length: {})",
            peer_id,
            err_code,
            content,
            content.len(),
        ),
    }

    Ok(())
}

// Upon handshake completion we ask the consensus layer for a finalization point
// we want to catchup from. This information is relayed to the peer we just
// connected to, which will then emit all finalizations past this point.
pub fn send_catchup_finalization_messages_by_point_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    peer_id: P2PNodeId,
    content: &[u8],
) -> Fallible<()> {
    match baker.get_finalization_messages(content, peer_id.as_raw())? {
        0i64 => info!(
            "Peer {} requested finalization messages by point from consensus",
            peer_id
        ),
        err_code => error!(
            "Peer {} could not request finalization messages by point from consensus due to error \
             code {} (bytes: {:?}, length: {})",
            peer_id,
            err_code,
            content,
            content.len(),
        ),
    }
    Ok(())
}

macro_rules! send_catchup_request_to_consensus {
    (
        $req_type:expr,
        $node:ident,
        $baker:ident,
        $content:ident,
        $peer_id:ident,
        $network_id:ident,
        $consensus_req_call:expr,
        $packet_direction:expr,
    ) => {{
        debug!("Got a consensus catch-up request for \"{}\"", $req_type);

        if $packet_direction == PacketDirection::Inbound {
            let res = $consensus_req_call($baker, $content)?;
            let return_type = match $req_type {
                CatchupBlockByHash => Block,
                CatchupFinalizationRecordByHash => FinalizationRecord,
                CatchupFinalizationRecordByIndex => FinalizationRecord,
                catchall_val => panic!("Can't respond to catchup type {}", catchall_val),
            };

            if !res.is_empty() && NetworkEndian::read_u64(&res[..8]) > 0 {
                let mut out_bytes = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize + res.len());
                out_bytes
                    .write_u16::<NetworkEndian>(return_type as u16)
                    .expect("Can't write to buffer");
                out_bytes.extend(res);

                match &$node.send_message(Some($peer_id), $network_id, None, out_bytes, false) {
                    Ok(_) => info!(
                        "Responded to a catch-up request type \"{}\" from peer {}",
                        $req_type, $peer_id
                    ),
                    Err(_) => error!(
                        "Couldn't respond to a catch-up request type \"{}\" from peer {}!",
                        $req_type, $peer_id
                    ),
                }
            } else {
                error!(
                    "Consensus doesn't have the data to fulfill a catch-up request type \"{}\" \
                     (to obtain a \"{}\") that peer {} requested (response: {:?})",
                    $req_type, return_type, $peer_id, res
                );
            }
        } else {
            let mut out_bytes = Vec::with_capacity(PAYLOAD_TYPE_LENGTH as usize + $content.len());
            out_bytes
                .write_u16::<NetworkEndian>($req_type as u16)
                .expect("Can't write to buffer");
            out_bytes.extend($content);

            match &$node.send_message(Some($peer_id), $network_id, None, out_bytes, false) {
                Ok(_) => info!(
                    "Sent a catch-up request type \"{}\" to peer {}",
                    $req_type, $peer_id
                ),
                Err(_) => error!(
                    "Couldn't respond to a catch-up request type \"{}\" to peer {}!",
                    $req_type, $peer_id
                ),
            }
        }

        Ok(())
    }};
}

// This function requests the finalization record for a certain finalization
// index (this function is triggered by consensus on another peer actively asks
// the p2p layer to request this for it)
pub fn send_catchup_request_finalization_record_by_index_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    node: &mut P2PNode,
    peer_id: P2PNodeId,
    network_id: NetworkId,
    content: &[u8],
    direction: PacketDirection,
) -> Fallible<()> {
    send_catchup_request_to_consensus!(
        ffi::PacketType::CatchupFinalizationRecordByIndex,
        node,
        baker,
        content,
        peer_id,
        network_id,
        |baker: &consensus::ConsensusContainer, content: &[u8]| -> Fallible<Vec<u8>> {
            let index = NetworkEndian::read_u64(&content[..8]);
            baker.get_indexed_finalization(index)
        },
        direction,
    )
}

pub fn send_catchup_request_finalization_record_by_hash_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    node: &mut P2PNode,
    peer_id: P2PNodeId,
    network_id: NetworkId,
    content: &[u8],
    direction: PacketDirection,
) -> Fallible<()> {
    // extra debug
    if let Ok(skov) = safe_read!(SKOV_DATA) {
        let hash = HashBytes::new(content);
        if skov.get_finalization_record_by_hash(&hash).is_some() {
            info!(
                "Peer {} here; I do have the finalization record for block {:?}",
                node.id(),
                hash
            );
        }
    } else {
        error!("Can't obtain a read lock on Skov!");
    }

    send_catchup_request_to_consensus!(
        ffi::PacketType::CatchupFinalizationRecordByHash,
        node,
        baker,
        content,
        peer_id,
        network_id,
        |baker: &consensus::ConsensusContainer, content: &[u8]| -> Fallible<Vec<u8>> {
            baker.get_block_finalization(content)
        },
        direction,
    )
}

pub fn send_catchup_request_block_by_hash_to_consensus(
    baker: &mut consensus::ConsensusContainer,
    node: &mut P2PNode,
    peer_id: P2PNodeId,
    network_id: NetworkId,
    content: &[u8],
    direction: PacketDirection,
) -> Fallible<()> {
    use concordium_global_state::common::{DELTA_LENGTH, SHA256};
    // extra debug
    let hash = &content[..SHA256 as usize];
    let delta = NetworkEndian::read_u64(&content[SHA256 as usize..][..DELTA_LENGTH as usize]);

    add_block_to_skov(node.id(), &hash);

    if delta == 0 {
        send_catchup_request_to_consensus!(
            ffi::PacketType::CatchupBlockByHash,
            node,
            baker,
            content,
            peer_id,
            network_id,
            |baker: &consensus::ConsensusContainer, content: &[u8]| -> Fallible<Vec<u8>> {
                baker.get_block(content)
            },
            direction,
        )
    } else {
        send_catchup_request_to_consensus!(
            ffi::PacketType::CatchupBlockByHash,
            node,
            baker,
            content,
            peer_id,
            network_id,
            |baker: &consensus::ConsensusContainer, _: &[u8]| -> Fallible<Vec<u8>> {
                baker.get_block_by_delta(hash, delta)
            },
            direction,
        )
    }
}

pub fn add_block_to_skov(node_id: P2PNodeId, hash_bytes: &[u8]) {
    if let Ok(skov) = safe_read!(SKOV_DATA) {
        let hash = HashBytes::new(&hash_bytes);
        if skov.get_block_by_hash(&hash).is_some() {
            info!("Peer {} here; I do have block {:?}", node_id, hash);
        }
    } else {
        error!("Can't obtain a read lock on Skov!");
    }
}
