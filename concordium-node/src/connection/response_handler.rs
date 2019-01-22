use std::sync::{ Arc, Mutex };
use network::{ NetworkResponse };
use connection::parse_handler::{ ParseHandler, ParseCallback, ParseCallbackResult };

pub struct ResponseHandler {
    pub pong_handler: ParseHandler<NetworkResponse>,
    pub find_node_handler: ParseHandler<NetworkResponse>,
    pub peer_list_handler: ParseHandler<NetworkResponse>,
    pub handshake_handler: ParseHandler<NetworkResponse>,

    pub main_handler: ParseHandler<NetworkResponse>
}

impl ResponseHandler {
    pub fn new() -> Self {
        ResponseHandler {
            pong_handler: ParseHandler::<NetworkResponse>::new(
                "Network response pong handler"),
            find_node_handler: ParseHandler::<NetworkResponse>::new(
                "Network response find node handler"),
            peer_list_handler: ParseHandler::<NetworkResponse>::new(
                "Network response peer list handler"),
            handshake_handler: ParseHandler::<NetworkResponse>::new(
                "Network response handshake_handler"),
            main_handler: ParseHandler::<NetworkResponse>::new(
                "Main Network response handler")
        }
    }

    fn process_message(&self, msg: &NetworkResponse) -> ParseCallbackResult {
        let main_status = (self.main_handler)(msg); 
        let spec_status = match msg {
            ref pong_inner_pkt @ NetworkResponse::Pong(_) => {
                (&self.pong_handler)(pong_inner_pkt)
            },
            ref find_node_inner_pkt @ NetworkResponse::FindNode(_, _) => {
                (&self.find_node_handler)(find_node_inner_pkt)
            },
            ref peer_list_inner_pkt @ NetworkResponse::PeerList(_, _) => {
                (&self.peer_list_handler)(peer_list_inner_pkt)
            },
            ref handshake_inner_pkt @ NetworkResponse::Handshake(_, _, _) => {
                (&self.handshake_handler)(handshake_inner_pkt)
            }
        };

        main_status.and( spec_status)
    }

    pub fn add_callback(
            &mut self, 
            callback: Arc< Mutex< Box< ParseCallback<NetworkResponse>>>>) -> &mut Self {
        self.main_handler.add_callback( callback);
        self
    }

    pub fn add_pong_callback(
            &mut self, 
            callback: Arc< Mutex< Box< ParseCallback<NetworkResponse>>>>) -> &mut Self {
        self.pong_handler.add_callback( callback);
        self
    }

    pub fn add_find_node_callback(
            &mut self, 
            callback: Arc< Mutex< Box< ParseCallback<NetworkResponse>>>>) -> &mut Self {
        self.find_node_handler.add_callback( callback);
        self
    }

    pub fn add_peer_list_callback(
            &mut self, 
            callback: Arc< Mutex< Box< ParseCallback<NetworkResponse>>>>) -> &mut Self {
        self.peer_list_handler.add_callback( callback);
        self
    }

    pub fn add_handshake_callback(
            &mut self, 
            callback: Arc< Mutex< Box< ParseCallback<NetworkResponse>>>>) -> &mut Self {
        self.handshake_handler.add_callback( callback);
        self
    }
}

impl_all_fns!( ResponseHandler, NetworkResponse);
