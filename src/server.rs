use actix::{prelude::*, Actor, Addr};
use actix_web::web;
use quinn::{Connecting, Endpoint, ServerConfig};
use tracing::{debug, error, info};

use std::{collections::HashMap, net::SocketAddr};
use uuid::Uuid;

use crate::{google_key_store, session, settings, StopHandle};

#[derive(Debug)]
pub struct StormGrokServer {
    pub sessions: HashMap<Uuid, (Addr<session::StormGrokClientSession>, String)>,
    pub server_endpoint: Endpoint,
    pub stop_handle: web::Data<StopHandle>,
    pub auth: settings::AuthRules,
    pub gkey_address: Addr<google_key_store::GoogleKeyStore>,
}
impl Actor for StormGrokServer {
    type Context = Context<Self>;
}

impl StormGrokServer {
    pub fn start(stop_handle: web::Data<StopHandle>, config: &settings::Settings) -> Addr<Self> {
        let (certs, key) = config.get_certs_and_key();
        let server_config =
            ServerConfig::with_single_cert(certs, key).expect("bad certificate/key");
        let server_address = format!("{}:{:?}", config.server.host, config.server.quic_port)
            .parse::<SocketAddr>()
            .unwrap();
        info!("Starting Quic server on {:?}", server_address);
        let (endpoint, incoming) = Endpoint::server(server_config, server_address).unwrap();
        let gkey_address = google_key_store::GoogleKeyStore::start();

        StormGrokServer::create(|ctx| {
            ctx.add_stream(incoming);
            StormGrokServer {
                sessions: HashMap::new(),
                server_endpoint: endpoint,
                stop_handle: stop_handle,
                auth: config.auth.clone(),
                gkey_address: gkey_address,
            }
        })
    }
}

impl StreamHandler<Connecting> for StormGrokServer {
    fn handle(&mut self, item: Connecting, ctx: &mut Self::Context) {
        session::start_session(
            item,
            ctx.address(),
            self.gkey_address.clone(),
            self.auth.clone(),
        )
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
        debug!("Resolving client for {:?}", &msg.id);
        match self.sessions.get(&msg.id) {
            Some(client_address) => Some(client_address.clone().1),
            None => None,
        }
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct LogAllClients {}
impl Handler<LogAllClients> for StormGrokServer {
    type Result = ();
    fn handle(&mut self, _: LogAllClients, _: &mut Context<Self>) -> Self::Result {
        info!("Printing all clients:");
        for key in self.sessions.keys() {
            info!("    {key}");
        }
    }
}
