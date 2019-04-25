#![recursion_limit = "1024"]
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate log;
// Explicitly defining allocator to avoid future reintroduction of jemalloc
use std::alloc::System;
#[global_allocator]
static A: System = System;

use env_logger::{Builder, Env};
use failure::Fallible;
use iron::{headers::ContentType, prelude::*, status};
use p2p_client::{
    common::{self, PeerType},
    configuration,
    db::P2PDB,
    network::{NetworkId, NetworkMessage, NetworkPacketType, NetworkRequest, NetworkResponse},
    p2p::*,
    utils,
};
use rand::{distributions::Standard, thread_rng, Rng};
use router::Router;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

#[derive(Clone)]
struct TestRunner {
    test_start:       Arc<Mutex<Option<u64>>>,
    test_running:     Arc<AtomicBool>,
    registered_times: Arc<Mutex<Vec<Measurement>>>,
    node:             Arc<Mutex<P2PNode>>,
    nid:              NetworkId,
    packet_size:      Arc<Mutex<Option<usize>>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Measurement {
    received_time: u64,
    node_id:       String,
}

impl Measurement {
    pub fn new(received_time: u64, node_id: String) -> Self {
        Measurement {
            received_time,
            node_id,
        }
    }
}

const DEFAULT_TEST_PACKET_SIZE: usize = 51_200;

impl TestRunner {
    pub fn new(node: P2PNode, nid: NetworkId) -> Self {
        TestRunner {
            test_start: Arc::new(Mutex::new(None)),
            test_running: Arc::new(AtomicBool::new(false)),
            registered_times: Arc::new(Mutex::new(vec![])),
            node: Arc::new(Mutex::new(node)),
            nid,
            packet_size: Arc::new(Mutex::new(None)),
        }
    }

    fn index(&self) -> IronResult<Response> {
        let mut resp = Response::with((
            status::Ok,
            format!(
                "<html><body><h1>Test runner service for {} v{}</h1>Operational!</p></body></html>",
                p2p_client::APPNAME,
                p2p_client::VERSION
            ),
        ));
        resp.headers.set(ContentType::html());
        Ok(resp)
    }

    fn register_receipt(&self, req: &mut Request<'_, '_>) -> IronResult<Response> {
        match req
            .extensions
            .get::<Router>()
            .expect("Malformed Iron request, lacks Router")
            .find("node_id")
        {
            Some(node_id) => {
                match req
                    .extensions
                    .get::<Router>()
                    .expect("Malformed Iron request, lacks Router")
                    .find("packet_id")
                {
                    Some(pktid) => {
                        let time = common::get_current_stamp();
                        if let Ok(mut list) = self.registered_times.lock() {
                            list.push(Measurement::new(time, node_id.to_string()));
                            info!("Registered time for {}/{} @ {}", node_id, pktid, time);
                            Ok(Response::with((
                                status::Ok,
                                format!(
                                    "REGISTERED packet {} FROM {} ON {}/{} @ {}",
                                    pktid,
                                    node_id,
                                    p2p_client::APPNAME,
                                    p2p_client::VERSION,
                                    time
                                ),
                            )))
                        } else {
                            error!("Couldn't register due to locking issues");
                            Ok(Response::with((
                                status::InternalServerError,
                                "Can't retrieve access to inner lock".to_string(),
                            )))
                        }
                    }
                    _ => {
                        error!("Couldn't register due to missing params");
                        Ok(Response::with((
                            status::NotFound,
                            "Missing packet id in url".to_string(),
                        )))
                    }
                }
            }
            _ => {
                error!("Couldn't register due to missing params");
                Ok(Response::with((
                    status::NotFound,
                    "Missing node id in url".to_string(),
                )))
            }
        }
    }

    fn start_test(&self, packet_size: usize) -> IronResult<Response> {
        if !self.test_running.load(Ordering::Relaxed) {
            self.test_running.store(true, Ordering::Relaxed);
            info!("Started test");
            *self.test_start.lock().expect("Couldn't lock test_start") =
                Some(common::get_current_stamp());
            *self.packet_size.lock().expect("Couldn't lock packet size") = Some(packet_size);
            let random_pkt: Vec<u8> = thread_rng()
                .sample_iter(&Standard)
                .take(packet_size)
                .collect();
            self.node
                .lock()
                .expect("Couldn't lock node")
                .send_message(None, self.nid, None, random_pkt, true)
                .map_err(|e| error!("{}", e))
                .ok();
            Ok(Response::with((
                status::Ok,
                format!(
                    "TEST STARTED ON {}/{} @ {}",
                    p2p_client::APPNAME,
                    p2p_client::VERSION,
                    common::get_current_stamp()
                ),
            )))
        } else {
            error!("Couldn't start test as it's already running");
            Ok(Response::with((
                status::Ok,
                "Test already running, can't start one!".to_string(),
            )))
        }
    }

    fn reset_test(&self) -> IronResult<Response> {
        if self.test_running.load(Ordering::Relaxed) {
            match self.test_start.lock() {
                Ok(mut inner_value) => *inner_value = None,
                _ => {
                    return Ok(Response::with((
                        status::InternalServerError,
                        "Can't retrieve access to inner lock".to_string(),
                    )))
                }
            }
            match self.registered_times.lock() {
                Ok(mut inner_value) => inner_value.clear(),
                _ => {
                    return Ok(Response::with((
                        status::InternalServerError,
                        "Can't retrieve access to inner lock".to_string(),
                    )))
                }
            }
            self.test_running.store(false, Ordering::Relaxed);
            *self.test_start.lock().expect("Couldn't lock test_start") = None;
            *self.packet_size.lock().expect("Couldn't lock packet size") = None;
            info!("Testing reset on runner");
            Ok(Response::with((
                status::Ok,
                format!(
                    "TEST RESET ON {}/{} @ {}",
                    p2p_client::APPNAME,
                    p2p_client::VERSION,
                    common::get_current_stamp()
                ),
            )))
        } else {
            error!("Test not running so can't reset right now");
            Ok(Response::with((
                status::Ok,
                "Test not running, can't reset now!".to_string(),
            )))
        }
    }

    fn get_results(&self) -> IronResult<Response> {
        if self.test_running.load(Ordering::Relaxed) {
            match self.test_start.lock() {
                Ok(test_start_time) => match self.registered_times.lock() {
                    Ok(inner_vals) => {
                        let return_json = json!({
                            "service_name": "TestRunner",
                            "service_version": p2p_client::VERSION,
                            "measurements": *inner_vals,
                            "test_start_time": *test_start_time,
                            "packet_size": *self.packet_size.lock().expect("Couldn't lock packet size") ,
                        });
                        let mut resp = Response::with((status::Ok, return_json.to_string()));
                        resp.headers.set(ContentType::json());
                        Ok(resp)
                    }
                    _ => {
                        error!("Couldn't send results due to locking issues");
                        Ok(Response::with((
                            status::InternalServerError,
                            "Can't retrieve access to inner lock",
                        )))
                    }
                },
                _ => {
                    error!("Couldn't send results due to locking issues");
                    Ok(Response::with((
                        status::InternalServerError,
                        "Can't retrieve access to inner lock",
                    )))
                }
            }
        } else {
            Ok(Response::with((
                status::Ok,
                "Test not running, can't get results now!",
            )))
        }
    }

    pub fn start_server(&mut self, listen_ip: &str, port: u16) -> thread::JoinHandle<()> {
        let mut router = Router::new();
        let _self_clone = Arc::new(self.clone());
        let _self_clone_2 = Arc::clone(&_self_clone);
        let _self_clone_3 = Arc::clone(&_self_clone);
        let _self_clone_4 = Arc::clone(&_self_clone);
        let _self_clone_5 = Arc::clone(&_self_clone);
        let _self_clone_6 = Arc::clone(&_self_clone);
        router.get(
            "/",
            move |_: &mut Request<'_, '_>| Arc::clone(&_self_clone).index(),
            "index",
        );
        router.get(
            "/register/:node_id/:packet_id",
            move |req: &mut Request<'_, '_>| Arc::clone(&_self_clone_2).register_receipt(req),
            "register",
        );
        router.get(
            "/start_test/:test_packet_size",
            move |req: &mut Request<'_, '_>| match req
                .extensions
                .get::<Router>()
                .and_then(|router| router.find("test_packet_size"))
            {
                Some(size_str) => match size_str.parse::<usize>() {
                    Ok(size) => Arc::clone(&_self_clone_3).start_test(size),
                    _ => Ok(Response::with((
                        status::BadRequest,
                        "Invalid size for test packet given",
                    ))),
                },
                _ => Ok(Response::with((
                    status::BadRequest,
                    "Missing test packet size",
                ))),
            },
            "start_test_specific",
        );
        router.get(
            "/start_test",
            move |_: &mut Request<'_, '_>| {
                Arc::clone(&_self_clone_4).start_test(DEFAULT_TEST_PACKET_SIZE)
            },
            "start_test_generic",
        );
        router.get(
            "/reset_test",
            move |_: &mut Request<'_, '_>| Arc::clone(&_self_clone_5).reset_test(),
            "reset_test",
        );
        router.get(
            "/get_results",
            move |_: &mut Request<'_, '_>| Arc::clone(&_self_clone_6).get_results(),
            "get_results",
        );
        let addr = format!("{}:{}", listen_ip, port);
        thread::spawn(move || {
            Iron::new(router).http(addr).ok();
        })
    }
}

fn get_config_and_logging_setup() -> (configuration::Config, configuration::AppPreferences) {
    let conf = configuration::parse_config();
    let app_prefs = configuration::AppPreferences::new(
        conf.common.config_dir.to_owned(),
        conf.common.data_dir.to_owned(),
    );

    info!(
        "Starting up {}-TestRunner version {}!",
        p2p_client::APPNAME,
        p2p_client::VERSION
    );
    info!(
        "Application data directory: {:?}",
        app_prefs.get_user_app_dir()
    );
    info!(
        "Application config directory: {:?}",
        app_prefs.get_user_config_dir()
    );

    let env = if conf.common.trace {
        Env::default().filter_or("MY_LOG_LEVEL", "trace")
    } else if conf.common.debug {
        Env::default().filter_or("MY_LOG_LEVEL", "debug")
    } else {
        Env::default().filter_or("MY_LOG_LEVEL", "info")
    };

    let mut log_builder = Builder::from_env(env);
    if conf.common.no_log_timestamp {
        log_builder.default_format_timestamp(false);
    }
    log_builder.init();

    p2p_client::setup_panics();
    (conf, app_prefs)
}

fn instantiate_node(
    conf: &configuration::Config,
    app_prefs: &mut configuration::AppPreferences,
) -> (P2PNode, mpsc::Receiver<Arc<NetworkMessage>>) {
    let (pkt_in, pkt_out) = mpsc::channel::<Arc<NetworkMessage>>();

    let node_id = if conf.common.id.is_some() {
        conf.common.id.clone()
    } else {
        app_prefs.get_config(configuration::APP_PREFERENCES_PERSISTED_NODE_ID)
    };

    let node_sender = if conf.common.debug {
        let (sender, receiver) = mpsc::channel();
        let _guard = thread::spawn(move || loop {
            if let Ok(msg) = receiver.recv() {
                info!("{}", msg);
            }
        });
        Some(sender)
    } else {
        None
    };

    let node = P2PNode::new(node_id, &conf, pkt_in, node_sender, PeerType::Node, None);

    (node, pkt_out)
}

fn setup_process_output(
    node: &P2PNode,
    conf: &configuration::Config,
    pkt_out: mpsc::Receiver<Arc<NetworkMessage>>,
    db: P2PDB,
) {
    let mut _node_self_clone = node.clone();

    let _no_trust_bans = conf.common.no_trust_bans;
    let _no_trust_broadcasts = conf.connection.no_trust_broadcasts;
    let _desired_nodes_clone = conf.connection.desired_nodes;
    let _guard_pkt = thread::spawn(move || loop {
        if let Ok(full_msg) = pkt_out.recv() {
            match *full_msg {
                NetworkMessage::NetworkPacket(ref pac, ..) => match pac.packet_type {
                    NetworkPacketType::DirectMessage(..) => {
                        info!(
                            "DirectMessage/{}/{} with size {} received",
                            pac.network_id,
                            pac.message_id,
                            pac.message.len()
                        );
                    }
                    NetworkPacketType::BroadcastedMessage => {
                        if !_no_trust_broadcasts {
                            info!(
                                "BroadcastedMessage/{}/{} with size {} received",
                                pac.network_id,
                                pac.message_id,
                                pac.message.len()
                            );
                            _node_self_clone
                                .send_message_from_cursor(
                                    None,
                                    pac.network_id,
                                    Some(pac.message_id.to_owned()),
                                    (*pac.message).to_owned(),
                                    true,
                                )
                                .map_err(|e| error!("Error sending message {}", e))
                                .ok();
                        }
                    }
                },
                NetworkMessage::NetworkRequest(NetworkRequest::BanNode(ref peer, x), ..) => {
                    utils::ban_node(&mut _node_self_clone, peer, x, &db, _no_trust_bans);
                }
                NetworkMessage::NetworkRequest(NetworkRequest::UnbanNode(ref peer, x), ..) => {
                    utils::unban_node(&mut _node_self_clone, peer, x, &db, _no_trust_bans);
                }
                NetworkMessage::NetworkResponse(NetworkResponse::PeerList(_, ref peers), ..) => {
                    info!("Received PeerList response, attempting to satisfy desired peers");
                    let mut new_peers = 0;
                    let stats = _node_self_clone.get_peer_stats(&[]);

                    for peer_node in peers {
                        if _node_self_clone
                            .connect(PeerType::Node, peer_node.addr, Some(peer_node.id()))
                            .map_err(|e| error!("{}", e))
                            .is_ok()
                        {
                            new_peers += 1;
                        }
                        if new_peers + stats.len() as u8 >= _desired_nodes_clone {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

fn main() -> Fallible<()> {
    let (conf, mut app_prefs) = get_config_and_logging_setup();

    if conf.common.print_config {
        // Print out the configuration
        info!("{:?}", conf);
    }

    let mut db_path = app_prefs.get_user_app_dir();
    db_path.push("p2p.db");

    let db = P2PDB::new(db_path.as_path());

    info!("Debugging enabled {}", conf.common.debug);

    let dns_resolvers =
        utils::get_resolvers(&conf.connection.resolv_conf, &conf.connection.dns_resolver);

    for resolver in &dns_resolvers {
        debug!("Using resolver: {}", resolver);
    }

    let bootstrap_nodes = utils::get_bootstrap_nodes(
        conf.connection.bootstrap_server.clone(),
        &dns_resolvers,
        conf.connection.no_dnssec,
        &conf.connection.bootstrap_node,
    );

    let (mut node, pkt_out) = instantiate_node(&conf, &mut app_prefs);

    node.spawn();

    match db.get_banlist() {
        Some(nodes) => {
            info!("Found existing banlist, loading up!");
            for n in nodes {
                node.ban_node(n);
            }
        }
        None => {
            info!("Couldn't find existing banlist. Creating new!");
            db.create_banlist();
        }
    };

    if !app_prefs.set_config(
        configuration::APP_PREFERENCES_PERSISTED_NODE_ID,
        Some(node.id().to_string()),
    ) {
        error!("Failed to persist own node id");
    }

    setup_process_output(&node, &conf, pkt_out, db);

    for connect_to in conf.connection.connect_to {
        match utils::parse_host_port(&connect_to, &dns_resolvers, conf.connection.no_dnssec) {
            Some((ip, port)) => {
                info!("Connecting to peer {}", &connect_to);
                node.connect(PeerType::Node, SocketAddr::new(ip, port), None)
                    .map_err(|e| error!("{}", e))
                    .ok();
            }
            None => error!("Can't parse IP to connect to '{}'", &connect_to),
        }
    }

    if !conf.connection.no_bootstrap_dns {
        info!("Attempting to bootstrap");
        match bootstrap_nodes {
            Ok(nodes) => {
                for (ip, port) in nodes {
                    let addr = SocketAddr::new(ip, port);
                    info!("Found bootstrap node: {}", addr);
                    node.connect(PeerType::Bootstrapper, addr, None)
                        .map_err(|e| error!("{}", e))
                        .ok();
                }
            }
            Err(e) => error!("Couldn't retrieve bootstrap node list! {:?}", e),
        };
    }

    let mut testrunner = TestRunner::new(node.clone(), NetworkId::from(conf.common.network_ids[0]));

    let _th = testrunner.start_server(
        &conf.testrunner.listen_http_address,
        conf.testrunner.listen_http_port,
    );

    _th.join().unwrap_or_else(|e| error!("{:?}", e));

    Ok(())
}
