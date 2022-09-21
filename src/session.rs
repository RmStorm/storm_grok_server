use actix::dev::MessageResponse;
use actix::dev::OneshotSender;
use actix::prelude::*;
use actix::{Actor, Addr, StreamHandler};
use actix_web::web;
use actix_web_actors::ws;
use log::info;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{net::SocketAddr, time::Duration, time::Instant};
use uuid::Uuid;

use crate::server;

#[derive(Serialize, Deserialize, Debug)]
pub struct FullResponseData {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct OrderedResponseData {
    pub response_data: FullResponseData,
    pub response_number: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct FullRequestData {
    pub method: String,
    pub uri: String,
    pub version: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub peer_addr: Option<SocketAddr>,
    pub body: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct OrderedRequestData {
    pub request_data: FullRequestData,
    pub request_number: usize,
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct StormGrokClientSession {
    pub id: Uuid,
    pub last_heart_beat: Instant,
    pub server_address: Addr<server::StormGrokServer>,
    pub request_number: usize,
    pub responses: HashMap<usize, FullResponseData>,
}

impl StormGrokClientSession {
    fn heart_beat(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.last_heart_beat) > CLIENT_TIMEOUT {
                info!("Websocket Client heartbeat failed, disconnecting!");
                act.server_address
                    .do_send(server::Disconnect { id: act.id });
                ctx.stop();
                return;
            }

            ctx.ping(b"");
        });
    }
}

impl Actor for StormGrokClientSession {
    type Context = ws::WebsocketContext<Self>;
    fn started(&mut self, ctx: &mut ws::WebsocketContext<Self>) {
        self.heart_beat(ctx);
        self.server_address
            .send(server::Connect {
                id: self.id,
                client_address: ctx.address(),
            })
            .into_actor(self)
            .then(|res, act, ctx| {
                match res {
                    Ok(_) => {
                        info!("Client {:?} registered with server", act.id);
                        ctx.text(format!(
                            "Forwarding http://{}.localhost:3000 -> http://localhost:8000",
                            act.id
                        ));
                        ctx.text(format!("curl http://{}.localhost:3000", act.id));
                        ctx.text("\n");
                        ctx.text("HTTP Requests");
                        ctx.text("-------------");
                        ctx.text("\n");
                    }
                    Err(er) => {
                        info!("Client failed to register with server {er:?}\n {act:?}");
                        ctx.text("Could not register correctly");

                        ctx.close(Some(ws::CloseReason {
                            code: ws::CloseCode::Error,
                            description: Some(format!("Could not register correctly")),
                        }));
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn stopped(&mut self, _ctx: &mut ws::WebsocketContext<Self>) {
        self.server_address
            .do_send(server::Disconnect { id: self.id });
        info!("Client {:?} is stopped", self.id);
    }
}

// TODO: Rewrite the whole damn thing to use tcp sockets instead of websockets?
impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for StormGrokClientSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                self.last_heart_beat = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_msg)) => {
                self.last_heart_beat = Instant::now();
            }
            Ok(ws::Message::Text(text)) => ctx.text(text),
            Ok(ws::Message::Binary(bin)) => {
                info!("Receiving bin Message with length: {}", bin.len());
                let data: OrderedResponseData = bincode::deserialize(&bin).unwrap();
                info!("Received data: {:?}", data);
                self.responses
                    .insert(data.response_number, data.response_data);
                info!("responses={:?}", self.responses);
            }
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => {
                info!("An unhandled message:\n'{msg:?}'\nwas encountered by client:\n'{self:?}'");
            }
        }
    }
}

#[derive(Message)]
#[rtype(usize)]
pub struct ForwardRequest {
    pub request_data: FullRequestData,
}
impl Handler<ForwardRequest> for StormGrokClientSession {
    type Result = usize;

    fn handle(&mut self, msg: ForwardRequest, ctx: &mut ws::WebsocketContext<Self>) -> usize {
        info!("Sending shit: '{:?}' to {:?}", msg.request_data, self.id);
        let r = OrderedRequestData {
            request_data: msg.request_data,
            request_number: self.request_number,
        };
        self.request_number += 1;

        ctx.text("you got mail buddy");
        ctx.binary(bincode::serialize(&r).unwrap());

        self.request_number - 1
    }
}

#[derive(Message)]
#[rtype(result = "Option<FullResponseData>")]
pub struct PollResponse {
    pub request_number: usize,
}

impl<A, M> MessageResponse<A, M> for FullResponseData
where
    A: Actor,
    M: Message<Result = FullResponseData>,
{
    fn handle(self, ctx: &mut A::Context, tx: Option<OneshotSender<M::Result>>) {
        if let Some(tx) = tx {
            tx.send(self);
        }
    }
}

impl Handler<PollResponse> for StormGrokClientSession {
    type Result = Option<FullResponseData>;

    fn handle(&mut self, msg: PollResponse, ctx: &mut ws::WebsocketContext<Self>) -> Self::Result {
        self.responses.remove(&msg.request_number)
    }
}
