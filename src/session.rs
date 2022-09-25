use actix::prelude::*;
use actix::{Actor, Addr, StreamHandler};
use actix_web::rt::net::TcpListener;

use quinn::{Connection, ConnectionError, NewConnection, OpenUni, RecvStream};

use std::time::Duration;
use tracing::log::info;
use uuid::Uuid;

use crate::server;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct StormGrokClientSession {
    pub id: Uuid,
    // pub tcp_listener: TcpListener,
    pub tcp_addr: String,
    pub connection: Connection,
    pub server_address: Addr<server::StormGrokServer>,
}

impl StormGrokClientSession {
    fn heart_beat(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |_act, ctx| {
            ctx.address().do_send(Ping {});
        });
        info!("Started heartbeat");
    }
}

impl Actor for StormGrokClientSession {
    type Context = Context<Self>;
    fn started(&mut self, ctx: &mut Context<Self>) {
        info!("send test to server!");
        self.heart_beat(ctx);
        info!("got a conn oh yeah{:?}", self.connection)
    }

    fn stopped(&mut self, _ctx: &mut Context<Self>) {
        // self.server_address
        //     .do_send(server::Disconnect { id: self.id });
        info!("Client {:?} is stopped", self.id);
    }
}

impl StormGrokClientSession {
    pub fn start(
        id: Uuid,
        tcp_addr: String,
        new_conn: NewConnection,
        server_address: Addr<server::StormGrokServer>,
    ) -> Addr<Self> {
        StormGrokClientSession::create(|ctx| {
            ctx.add_stream(new_conn.uni_streams);
            // ctx.add_stream(tcp_listener.accept());  // TODO, turn incoming connections into stream!
            let connection = new_conn.connection;

            StormGrokClientSession {
                id,
                tcp_addr,
                connection,
                server_address,
            }
        })
    }
}

impl StreamHandler<Result<RecvStream, ConnectionError>> for StormGrokClientSession {
    fn handle(&mut self, item: Result<RecvStream, ConnectionError>, ctx: &mut Self::Context) {
        match item {
            Ok(recv) => {
                info!("found nothing");
                recv.read_to_end(100)
                    .into_actor(self)
                    .then(|buffed_data, _act, _ctx| {
                        info!("dat: {:?}", buffed_data);
                        fut::ready(())
                    })
                    .spawn(ctx);
            }
            Err(err) => {
                info!("gotta stop, shit blew up {:?}", err);
                self.server_address
                    .do_send(server::Disconnect { id: self.id });
                ctx.stop()
            }
        }
    }
}

async fn send_ping(uni_pipe: OpenUni) {
    let mut send = uni_pipe.await.unwrap();
    send.write_all(b"ping").await.unwrap();
    send.finish().await.unwrap();
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Ping {}
impl Handler<Ping> for StormGrokClientSession {
    type Result = ();

    fn handle(&mut self, _msg: Ping, ctx: &mut Self::Context) {
        send_ping(self.connection.open_uni())
            .into_actor(self)
            .spawn(ctx);
    }
}

async fn create_server_for_client(tcp_listener: TcpListener, connection: Connection) {
    while let Ok((mut client, addr)) = tcp_listener.accept().await {
        info!("Connected new client at {addr:?}");

        let (mut server_send, mut server_recv) = connection.clone().open_bi().await.unwrap();
        // ApplicationClosed(ApplicationClose { error_code: 0, reason: b"done" })
        tokio::spawn(async move {
            let (mut client_recv, mut client_send) = client.split();
            tokio::select! {
                _ = tokio::io::copy(&mut server_recv, &mut client_send) => {
                    info!("reached EOF on client")
                }
                _ = tokio::io::copy(&mut client_recv, &mut server_send) => {
                    info!("reached EOF on server")
                }
            };
        });
        info!("Hooked up the pipes and shoved them into their own thread");
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct StartListeningOnPort {
    pub tcp_listener: TcpListener,
}
impl Handler<StartListeningOnPort> for StormGrokClientSession {
    type Result = ();

    fn handle(&mut self, msg: StartListeningOnPort, ctx: &mut Self::Context) {
        create_server_for_client(msg.tcp_listener, self.connection.clone())
            .into_actor(self)
            .spawn(ctx);
    }
}
