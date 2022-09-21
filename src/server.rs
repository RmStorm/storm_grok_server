use actix::prelude::*;
use actix::{Actor, Addr};
use actix_web::web;

use log::info;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{session, StopHandle};

type ClientAddress = Addr<session::StormGrokClientSession>;

#[derive(Debug)]
pub struct StormGrokServer {
    pub sessions: HashMap<Uuid, ClientAddress>,
    pub stop_handle: web::Data<StopHandle>,
}
impl Actor for StormGrokServer {
    type Context = Context<Self>;
    fn started(&mut self, _ctx: &mut Context<Self>) {
        info!("StormGrokServer is alive");
    }

    fn stopped(&mut self, _ctx: &mut Context<Self>) {
        info!("StormGrokServer is stopped");
        self.stop_handle.stop(true)
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
        self.sessions.remove(&msg.id);
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Connect {
    pub id: Uuid,
    pub client_address: Addr<session::StormGrokClientSession>,
}
impl Handler<Connect> for StormGrokServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _: &mut Context<Self>) {
        info!("Adding {:?} to sessions", msg.id);
        self.sessions.insert(msg.id, msg.client_address);
    }
}

#[derive(Message)]
#[rtype(result = "Option<ClientAddress>")]
pub struct ResolveClient {
    pub id: Uuid,
}
impl Handler<ResolveClient> for StormGrokServer {
    type Result = Option<ClientAddress>;

    fn handle(&mut self, msg: ResolveClient, _: &mut Context<Self>) -> Self::Result {
        info!("Resolving {:?}", msg.id);

        match self.sessions.get(&msg.id) {
            Some(client_address) => Some(client_address.clone()),
            None => None,
        }
    }
}
