use std::sync::{ Arc, RwLock };
use std::sync::atomic::{ AtomicUsize, Ordering };
use std::net::{ IpAddr, SocketAddr };
use std::rc::{ Rc };
use std::cell::{ RefCell };
use mio::net::{ TcpListener, TcpStream };
use mio::{ Token, Poll, Event };
use std::sync::mpsc::Sender;
use rustls::{ ClientConfig, ServerConfig, ServerSession, ClientSession };
use webpki::{ DNSNameRef };
use failure::{Fallible, bail };
use super::fails;

use crate::prometheus_exporter::PrometheusServer;
use crate::common::functor::afunctor::{ AFunctor, AFunctorCW };

use crate::connection::{
    Connection, P2PNodeMode, P2PEvent, MessageHandler,
    MessageManager };
use crate::common::{ P2PNodeId, P2PPeer, ConnectionType };
use crate::network::{ NetworkRequest, NetworkMessage, Buckets };

use crate::p2p::peer_statistics::{ PeerStatistic };
use crate::p2p::tls_server_private::{ TlsServerPrivate };

pub type PreHandshakeCW = AFunctorCW<SocketAddr>;
pub type PreHandshake = AFunctor<SocketAddr>;

pub struct TlsServer {
    server: TcpListener,
    next_id: AtomicUsize,
    server_tls_config: Arc<ServerConfig>,
    client_tls_config: Arc<ClientConfig>,
    own_id: P2PNodeId,
    event_log: Option<Sender<P2PEvent>>,
    self_peer: P2PPeer,
    mode: P2PNodeMode,
    buckets: Arc< RwLock< Buckets > >,
    prometheus_exporter: Option<Arc<RwLock<PrometheusServer>>>,
    message_handler: Arc< RwLock< MessageHandler>>,
    dptr: Rc< RefCell< TlsServerPrivate>>,
    blind_trusted_broadcast: bool,

    prehandshake_validations: PreHandshake
}

impl TlsServer {
    pub fn new(server: TcpListener,
           server_cfg: Arc<ServerConfig>,
           client_cfg: Arc<ClientConfig>,
           own_id: P2PNodeId,
           event_log: Option<Sender<P2PEvent>>,
           self_peer: P2PPeer,
           mode: P2PNodeMode,
           prometheus_exporter: Option<Arc<RwLock<PrometheusServer>>>,
           networks: Vec<u16>,
           buckets: Arc< RwLock< Buckets > >,
           blind_trusted_broadcast: bool,
           )
           -> Self {
        let mdptr = Rc::new( RefCell::new(
                TlsServerPrivate::new(
                    networks,
                    prometheus_exporter.clone())));

        let mut mself = TlsServer { server,
                    next_id: AtomicUsize::new(2),
                    server_tls_config: server_cfg,
                    client_tls_config: client_cfg,
                    own_id,
                    event_log,
                    self_peer,
                    mode,
                    prometheus_exporter,
                    buckets,
                    message_handler: Arc::new( RwLock::new( MessageHandler::new())),
                    dptr: mdptr,
                    blind_trusted_broadcast,
                    prehandshake_validations: PreHandshake::new("TlsServer::Accept")
        };
        mself.add_default_prehandshake_validations();
        mself.setup_default_message_handler();
        mself
    }

    pub fn log_event(&mut self, event: P2PEvent) {
        if let Some(ref mut x) = self.event_log {
            if let Err(e) = x.send(event) {
                error!("Couldn't send error {:?}", e)
            }
        }
    }

    pub fn get_self_peer(&self) -> P2PPeer {
        self.self_peer.clone()
    }

    pub fn networks(&self) -> Arc<RwLock<Vec<u16>>> {
        self.dptr.borrow().networks.clone()
    }

    pub fn remove_network(&mut self, network_id: u16) -> Fallible<()> {
        self.dptr.borrow_mut().remove_network( network_id)
    }

    pub fn add_network(&mut self, network_id: u16) -> Fallible<()> {
        self.dptr.borrow_mut().add_network( network_id)
    }

    /// It returns true if `ip` at port `port` is in `unreachable_nodes` list.
    pub fn is_unreachable(&self, ip: IpAddr, port: u16) -> bool {
        self.dptr.borrow().unreachable_nodes.contains( ip, port)
    }

    /// It adds the pair `ip`,`port` to its `unreachable_nodes` list.
    pub fn add_unreachable(&mut self, ip: IpAddr, port: u16) -> bool {
        self.dptr.borrow_mut().unreachable_nodes.insert( ip, port)
    }

    pub fn get_peer_stats(&self, nids: &[u16]) -> Vec<PeerStatistic> {
        self.dptr.borrow().get_peer_stats(nids)
    }

    pub fn ban_node(&mut self, peer: P2PPeer) -> bool {
        self.dptr.borrow_mut().ban_node( peer)
    }

    pub fn unban_node(&mut self, peer: &P2PPeer) -> bool {
        self.dptr.borrow_mut().unban_node( peer)
    }

    pub fn accept(&mut self, poll: &mut Poll, self_id: P2PPeer) -> Fallible<()> {
        let (socket, addr) = self.server.accept()?;
        debug!("Accepting new connection from {:?} to {:?}:{}", addr, self_id.ip(), self_id.port());

        if let Err(e) = (self.prehandshake_validations)(&addr) {
            bail!(e);
        }

        self.log_event(P2PEvent::ConnectEvent(addr.ip().to_string(), addr.port()));

        let tls_session = ServerSession::new(&self.server_tls_config);
        let token = Token(self.next_id.fetch_add(1, Ordering::SeqCst));

        let networks = self.dptr.borrow().networks.clone();
        let mut conn = Connection::new(ConnectionType::Node,
                                       socket,
                                       token,
                                       Some(tls_session),
                                       None,
                                       self.own_id.clone(),
                                       self_id,
                                       addr.ip().clone(),
                                       addr.port().clone(),
                                       self.mode,
                                       self.prometheus_exporter.clone(),
                                       self.event_log.clone(),
                                       networks,
                                       self.buckets.clone(),
                                       self.blind_trusted_broadcast,);
        self.register_message_handlers( &mut conn);

        let register_status = conn.register( poll);
        self.dptr.borrow_mut().add_connection( conn);

        register_status
    }

    pub fn connect(&mut self,
               connection_type: ConnectionType,
               poll: &mut Poll,
               ip: IpAddr,
               port: u16,
               peer_id_opt: Option<P2PNodeId>,
               self_id: &P2PPeer)
               -> Fallible<()> {
        if connection_type == ConnectionType::Node && self.is_unreachable(ip, port) {
            error!("Node marked as unreachable, so not allowing the connection");
            bail!(fails::UnreachablePeerError);
        }
        let self_peer = self.get_self_peer();
        if self_peer.ip() == ip && self_peer.port() == port {
            bail!(fails::DuplicatePeerError);
        }

        if let Ok(target_id) = P2PNodeId::from_ip_port( ip, port) {
            if let Some(_rc_conn) = self.dptr.borrow().find_connection_by_id( &target_id) {
                bail!(fails::DuplicatePeerError);
            }
        }

        if let Some(ref peer_id) = peer_id_opt {
            if let Some(_rc_conn) = self.dptr.borrow().find_connection_by_id( peer_id) {
                bail!(fails::DuplicatePeerError);
            }
        }

        match TcpStream::connect(&SocketAddr::new(ip, port)) {
            Ok(x) => {
                if let Some(ref prom) = &self.prometheus_exporter {
                    safe_write!(prom)?
                        .conn_received_inc()
                        .map_err(|e| error!("{}", e))
                        .ok();
                };
                let tls_session = ClientSession::new(
                    &self.client_tls_config,
                    DNSNameRef::try_from_ascii_str(&"node.concordium.com").unwrap_or_else(|e|
                        panic!("The error is: {:?}", e),
                    )
                );

                let token = Token(self.next_id.fetch_add(1, Ordering::SeqCst));

                let networks = self.dptr.borrow().networks.clone();
                let mut conn = Connection::new(connection_type,
                                           x,
                                           token,
                                           None,
                                           Some(tls_session),
                                           self.own_id.clone(),
                                           self_id.clone(),
                                           ip,
                                           port,
                                           self.mode,
                                           self.prometheus_exporter.clone(),
                                           self.event_log.clone(),
                                           networks.clone(),
                                           self.buckets.clone(),
                                           self.blind_trusted_broadcast,);

                self.register_message_handlers( &mut conn);
                conn.register(poll)?;

                self.dptr.borrow_mut().add_connection( conn);
                self.log_event(P2PEvent::ConnectEvent(ip.to_string(), port));
                debug!("Requesting handshake from new peer {}:{}",
                       ip.to_string(),
                       port);
                let self_peer = self.get_self_peer().clone();

                if let Some(ref rc_conn) = self.dptr.borrow().find_connection_by_token( &token)
                {
                    let mut conn = rc_conn.borrow_mut();
                    conn.serialize_bytes(
                        &NetworkRequest::Handshake(self_peer,
                                                   safe_read!(networks)?
                                                   .clone(),
                            vec![]).serialize())?;
                    conn.set_measured_handshake_sent();
                }
                Ok(())
            }
            Err(e) => {
                if connection_type == ConnectionType::Node
                   && !self.add_unreachable(ip, port)
                {
                    error!("Can't insert unreachable peer!");
                }
                into_err!(Err(e))
            }
        }
    }

    pub fn conn_event(&mut self,
                  poll: &mut Poll,
                  event: &Event,
                  packet_queue: &Sender<Arc<NetworkMessage>>)
                  -> Fallible<()> {
        self.dptr.borrow_mut().conn_event( poll, event, packet_queue)
    }

    pub fn cleanup_connections(&self, poll: &mut Poll)
            -> Fallible<()> {
        self.dptr.borrow_mut().cleanup_connections( self.mode, poll)
    }

    pub fn liveness_check(&self) -> Fallible<()> {
        self.dptr.borrow_mut().liveness_check()
    }

    /// It sends `data` message over all filtered connections.
    ///
    /// # Arguments
    /// * `data` - Raw message.
    /// * `filter_conn` - It will send using all connection, where this function returns `true`.
    /// * `send_status` - It will called after each sent, to notify the result of the operation.
    pub fn send_over_all_connections( &self,
            data: &Vec<u8>,
            filter_conn: &dyn Fn( &Connection) -> bool,
            send_status: &dyn Fn( &Connection, Fallible<usize>))
    {
        self.dptr.borrow_mut()
            .send_over_all_connections( data, filter_conn, send_status)
    }

    /// It setups default message handler at TLSServer level.
    fn setup_default_message_handler(&mut self) {
    }

    /// It adds all message handler callback to this connection.
    fn register_message_handlers(&self, conn: &mut Connection) {
        let mh = &self.message_handler.read().expect("Couldn't read when registering message handlers");
        conn.common_message_handler.clone().borrow_mut().merge(mh);
    }

    fn add_default_prehandshake_validations(&mut self) {
            self.prehandshake_validations.add_callback(self.make_check_banned());
    }

    fn make_check_banned(&self) -> PreHandshakeCW {
        let cloned_dptr = self.dptr.clone();
        make_atomic_callback!(
            move |sockaddr: &SocketAddr| {
                if cloned_dptr.borrow().addr_is_banned(sockaddr)? {
                    bail!(fails::BannedNodeRequestedConnectionError);
                }
                Ok(())
            })
    }


}

impl MessageManager for TlsServer {
    fn message_handler(&self) -> Arc< RwLock< MessageHandler>> {
        self.message_handler.clone()
    }
}
