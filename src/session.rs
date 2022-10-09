use actix::{prelude::*, Actor, Addr};
use actix_web::rt::net::TcpListener;

use futures_util::stream::StreamExt;
use quinn::{Connecting, Connection, NewConnection, OpenUni};

use anyhow::{anyhow, bail, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::{Deserialize, Serialize};
use std::{io::ErrorKind, time::Duration};
use tracing::log::{debug, error, info};
use uuid::Uuid;

use crate::{google_key_store, server, settings};

#[derive(Debug, Copy, Clone)]
enum Mode {
    Http,
    Tcp,
}

impl From<u8> for Mode {
    fn from(num: u8) -> Self {
        match num {
            116 => Mode::Tcp, // 116 = t in ascii
            _ => Mode::Http,  // default to Http
        }
    }
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(4);

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
        self.heart_beat(ctx);
    }

    fn stopped(&mut self, _ctx: &mut Context<Self>) {
        self.server_address
            .do_send(server::Disconnect { id: self.id });
        info!("Client {:?} is stopped", self.id);
    }
}

async fn send_ping(uni_pipe: OpenUni) -> Result<()> {
    let mut send = uni_pipe.await?;
    send.write_all(b"ping").await?;
    send.finish().await?;
    Ok(())
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Ping {}
impl Handler<Ping> for StormGrokClientSession {
    type Result = ();

    fn handle(&mut self, _msg: Ping, ctx: &mut Self::Context) {
        send_ping(self.connection.open_uni())
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("encountered connection error in uni_stream: {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .spawn(ctx);
    }
}

async fn connect_tcp_to_bi_quic(tcp_listener: TcpListener, connection: Connection) {
    while let Ok((mut client, addr)) = tcp_listener.accept().await {
        debug!("accepted tcp conn on {:?}", addr);
        if let Ok((mut server_send, mut server_recv)) = connection.clone().open_bi().await {
            debug!("accepted quic bi-conn");
            tokio::spawn(async move {
                let (mut client_recv, mut client_send) = client.split();
                debug!("Hooking up tcp conn to quic bi-conn");
                tokio::select! {
                    _ = tokio::io::copy(&mut server_recv, &mut client_send) => {}
                    _ = tokio::io::copy(&mut client_recv, &mut server_send) => {}
                };
            });
        }
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
        connect_tcp_to_bi_quic(msg.tcp_listener, self.connection.clone())
            .into_actor(self)
            .spawn(ctx);
        debug!("Forwarding to client {:?}", self.id);
    }
}

async fn listen_available_port() -> Result<TcpListener> {
    debug!("Finding available port");
    for port in 1025..65535 {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(error) => match error.kind() {
                ErrorKind::AddrInUse => {}
                e => bail!("Encountered error while setting up tcp server: {:?}", e),
            },
        }
    }
    bail!("No ports available")
}

pub async fn start_session(
    connection_future: Connecting,
    server_address: Addr<server::StormGrokServer>,
    key_store_address: Addr<google_key_store::GoogleKeyStore>,
    auth: settings::AuthRules,
) {
    match connection_future.await {
        Ok(new_conn) => {
            let conn = new_conn.connection.clone();
            if let Err(e) = do_handshake(new_conn, server_address, key_store_address, auth).await {
                error!("Encountered '{:?}' while handshaking client", e);
                conn.close(1u32.into(), e.to_string().as_bytes())
            };
        }
        Err(e) => error!("Error while instantiating connection to client {:?}", e),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    hd: Option<String>,
    email: String,
    email_verified: bool,
}

async fn do_handshake(
    mut new_conn: NewConnection,
    server_address: Addr<server::StormGrokServer>,
    key_store_address: Addr<google_key_store::GoogleKeyStore>,
    auth: settings::AuthRules,
) -> Result<()> {
    let id = Uuid::new_v4();

    let tcp_listener = match listen_available_port().await {
        Ok(l) => l,
        Err(e) => {
            error!("Error while finding free port for new client: {:?}", e);
            bail!("internal server error, could not find free port for you");
        }
    };
    let tcp_addr = tcp_listener.local_addr()?;
    debug!("Reserved: {:?} for new client", &tcp_addr);

    if let Some(Ok((mut send, recv))) = new_conn.bi_streams.next().await {
        let received_bytes = recv.read_to_end(1000).await?;
        info!("First byte = {:?}", Mode::from(received_bytes[0]));
        let token = String::from_utf8_lossy(&received_bytes[1..]);
        let kid = decode_header(&token)?
            .kid
            .ok_or(anyhow!("No kid found in token header"))?;
        let dec_key = google_key_store::get_key_for_kid(key_store_address, kid).await?;
        let token_message = decode::<Claims>(&token, &dec_key, &Validation::new(Algorithm::RS256))?;
        validate_claims(token_message.claims, auth).await?;
        send.write_all(id.as_bytes()).await?;
        send.finish().await?;
    }

    let session_address = StormGrokClientSession {
        id: id,
        tcp_addr: tcp_addr.to_string(),
        connection: new_conn.connection,
        server_address: server_address.clone(),
    }
    .start();

    session_address
        .send(StartListeningOnPort { tcp_listener })
        .await?;

    server_address
        .send(server::Connect {
            id: id,
            session_data: (session_address.clone(), tcp_addr.to_string()),
        })
        .await?;
    Ok(())
}

async fn validate_claims(claims: Claims, auth: settings::AuthRules) -> Result<()> {
    if claims.email_verified && auth.users.contains(&claims.email) {
        return Ok(());
    }
    if let Some(host_domain) = claims.hd {
        if auth.host_domains.contains(&host_domain) {
            return Ok(());
        }
    }
    bail!("This token is not authorized!");
}
