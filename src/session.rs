use actix::{prelude::*, Actor, Addr};
use actix_web::rt::net::TcpListener;
use futures_util::stream::StreamExt;
use quinn::{Connecting, Connection, NewConnection, OpenUni};

use anyhow::{bail, Result, anyhow};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::{io::ErrorKind, time::Duration};
use tracing::log::{debug, error, info};
use uuid::Uuid;

use crate::{server, settings};

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

impl StormGrokClientSession {
    pub fn start(
        id: Uuid,
        tcp_addr: String,
        new_conn: NewConnection,
        server_address: Addr<server::StormGrokServer>,
    ) -> Addr<Self> {
        StormGrokClientSession::create(|_ctx| {
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
        debug!("Created tcp listen port on {:?}", addr);
        let (mut server_send, mut server_recv) = connection.clone().open_bi().await.unwrap();
        tokio::spawn(async move {
            let (mut client_recv, mut client_send) = client.split();
            tokio::select! {
                _ = tokio::io::copy(&mut server_recv, &mut client_send) => {}
                _ = tokio::io::copy(&mut client_recv, &mut server_send) => {}
            };
        });
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
        info!("Forwarding to client {:?}", self.id);
        connect_tcp_to_bi_quic(msg.tcp_listener, self.connection.clone())
            .into_actor(self)
            .spawn(ctx);
    }
}

async fn listen_available_port() -> TcpListener {
    debug!("Finding available port");
    for port in 1025..65535 {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return l,
            Err(error) => match error.kind() {
                ErrorKind::AddrInUse => {}
                other_error => panic!(
                    "Encountered errr while setting up tcp server: {:?}",
                    other_error
                ),
            },
        }
    }
    panic!("No ports available")
}

pub async fn start_session(
    connection_future: Connecting,
    server_address: Addr<server::StormGrokServer>,
    auth: settings::AuthRules,
) {
    let new_conn: NewConnection = connection_future.await.unwrap();
    let conn = new_conn.connection.clone();
    if let Err(e) = do_handshake(new_conn, server_address, auth).await {
        error!("Encountered '{:?}' while handshaking client", e);
        conn.close(1u32.into(), e.to_string().as_bytes())
    };
}

async fn do_handshake(
    mut new_conn: NewConnection,
    server_address: Addr<server::StormGrokServer>,
    auth: settings::AuthRules,
) -> Result<()> {
    let id = Uuid::new_v4();
    if let Some(Ok((mut send, recv))) = new_conn.bi_streams.next().await {
        let received_bytes = recv.read_to_end(1000).await?;
        validate_token(received_bytes, auth).await?;
        send.write_all(id.as_bytes()).await.unwrap();
        send.finish().await.unwrap();
    }

    let tcp_listener = listen_available_port().await;
    let tcp_addr = tcp_listener.local_addr().unwrap();
    debug!(
        "Setting up client session with tcp listener on {:?}",
        tcp_listener
    );

    let session_address =
        StormGrokClientSession::start(id, tcp_addr.to_string(), new_conn, server_address.clone());

    session_address
        .send(StartListeningOnPort { tcp_listener })
        .await
        .unwrap();

    server_address
        .send(server::Connect {
            id: id,
            session_data: (session_address.clone(), tcp_addr.to_string()),
        })
        .await
        .unwrap();
    Ok(())
}

// Claims is a struct that implements Deserialize
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    hd: Option<String>,
    email: String,
    email_verified: bool,
}

async fn validate_token(received_bytes: Vec<u8>, auth: settings::AuthRules) -> Result<()> {
    let token = String::from_utf8_lossy(&received_bytes);
    let _header = decode_header(&token)?.kid.ok_or(anyhow!("No kid found in header"))?;
    // TODO: Fetch n from google and use correct key for kid
    // Copied n and e over from: https://www.googleapis.com/oauth2/v3/certs
    let n = "wRG8MxiNesaO9_BmIUgvWEmP6NJ6c2RM3FcNx0pzOhdsyRdLVuNw32CeFDLtxtjlan3Tqn6UflASqSp2LpZ_SWFJO7N7lcc0pTIVi10yKOSf7Cryl0X2-iUkpFy-ewYmqdK_fQCrgFipBvkiSO6lR3WtzkcQPqvatlBvV-eMo-i1_pIitB6Fx3ScefgDAvwTPNldRaCpPds10XeLbpg385_TJFVAzQozD2VwyDjUTFxiVnNzF0oAKrWw76UZBeoxc5EX61kqZBVdDlOQJU80SaH9EjlxLwfsDTy6JtsV4vvemM4rLWR_1_uxkch9Arzq-LZO0s73RoCy9nhxpIzbhQ";
    // let n = "-aCIh5BgnG_83z6njWPVVzlJvLdZvLoFIsMcN6lkuj-GwY9Z0MA86vL5XiH1hbYm0yMLizBYL3CM5Pplrb54o_EKY5uKxPtAWckceQJnZBNq9YFsbOI61Jf2iPhNt08IKrJ8sOq8aTqM8UUWPmKJByo8fvzBDbmZwNyyb0CLtB-jVvNURu1f-FVZwboAgKJIh6-XCL__KkPNgfW7ODaXXrk1cvm2GpgCNr7x-Ht5IJZwjx_TLwo9xdRPfUiEQtpUvVUghOUM_0JCfHHg95IDyz9Eo27GLvBLtyJK9qpm4_hhyWElXGSawvgr5ybovuoq1IUGshkQHkHX9ZK6NvBaNw";
    let e = "AQAB";
    let dec_key = DecodingKey::from_rsa_components(n, e).unwrap();
    let token_message = decode::<Claims>(&token, &dec_key, &Validation::new(Algorithm::RS256))?;
    if token_message.claims.email_verified && auth.users.contains(&token_message.claims.email) {
        return Ok(());
    }
    if let Some(host_domain) = token_message.claims.hd {
        if auth.host_domains.contains(&host_domain) {
            return Ok(());
        }
    }
    bail!("You are not allowed in here!");
}
