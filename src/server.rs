use actix::{prelude::*, Actor, Addr};
use actix_web::web;
use quinn::{Connecting, Endpoint, ServerConfig};

use rcgen;
use tracing::{error, info};

use std::{collections::HashMap, net::SocketAddr};
use uuid::Uuid;

use crate::{session, StopHandle};

#[derive(Debug)]
pub struct StormGrokServer {
    pub sessions: HashMap<Uuid, (Addr<session::StormGrokClientSession>, String)>,
    pub server_endpoint: Endpoint,
    pub stop_handle: web::Data<StopHandle>,
}
impl Actor for StormGrokServer {
    type Context = Context<Self>;
}

fn generate_self_signed_cert() -> (rustls::Certificate, rustls::PrivateKey) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = rustls::PrivateKey(cert.serialize_private_key_der());
    (rustls::Certificate(cert.serialize_der().unwrap()), key)
}

impl StormGrokServer {
    pub fn start(stop_handle: web::Data<StopHandle>) -> Addr<Self> {
        let (cert, key) = generate_self_signed_cert();
        let server_config = ServerConfig::with_single_cert(vec![cert], key).unwrap();

        let server_address = "127.0.0.1:5000".parse::<SocketAddr>().unwrap();
        let (endpoint, incoming) = Endpoint::server(server_config, server_address).unwrap();

        StormGrokServer::create(|ctx| {
            ctx.add_stream(incoming);
            StormGrokServer {
                sessions: HashMap::new(),
                server_endpoint: endpoint,
                stop_handle: stop_handle,
            }
        })
    }
}

impl StreamHandler<Connecting> for StormGrokServer {
    fn handle(&mut self, item: Connecting, ctx: &mut Self::Context) {
        session::start_session(item, ctx.address())
            .into_actor(self)
            .spawn(ctx); // No waiting I think?
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Disconnect {
    pub id: Uuid,
}
impl Handler<Disconnect> for StormGrokServer {
    type Result = ();
    fn handle(&mut self, msg: Disconnect, _: &mut Context<Self>) {
        info!("Removing {:?} from sessions", msg.id);
        if let None = self.sessions.remove(&msg.id) {
            error!("Tried to remove non existent session {:?}", msg.id);
            self.stop_handle.stop(true);
        }
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Connect {
    pub id: Uuid,
    pub session_data: (Addr<session::StormGrokClientSession>, String),
}
impl Handler<Connect> for StormGrokServer {
    type Result = ();
    fn handle(&mut self, msg: Connect, _: &mut Context<Self>) {
        info!("Adding {:?} to sessions", msg.id);
        self.sessions.insert(msg.id, msg.session_data);
    }
}

#[derive(Message)]
#[rtype(result = "Option<String>")]
pub struct ResolveClient {
    pub id: Uuid,
}
impl Handler<ResolveClient> for StormGrokServer {
    type Result = Option<String>;
    fn handle(&mut self, msg: ResolveClient, _: &mut Context<Self>) -> Self::Result {
        match self.sessions.get(&msg.id) {
            Some(client_address) => Some(client_address.clone().1),
            None => None,
        }
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct LogAllClients { }
impl Handler<LogAllClients> for StormGrokServer {
    type Result = ();
    fn handle(&mut self, _: LogAllClients, _: &mut Context<Self>) -> Self::Result {
        info!("Printing all clients:");
        for key in self.sessions.keys() {
            info!("    {key}");
        }
    }
}


