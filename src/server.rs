use actix::prelude::*;
use actix::{Actor, Addr};
use actix_web::rt::net::TcpListener;
use actix_web::web;

use quinn::{Connecting, Connection, Endpoint, ServerConfig};

use rcgen;
use tracing::{error, info};

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
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
        info!("starting quinn server on {:?}", server_address);
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

async fn send_uuid_over_uni_pipe(connection: Connection) -> Uuid {
    let id = Uuid::new_v4();
    let mut send = connection.open_uni().await.unwrap();
    send.write_all(id.as_bytes()).await.unwrap();
    send.finish().await.unwrap();
    id
}

async fn listen_available_port() -> TcpListener {
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

async fn start_session(connection_future: Connecting, server_address: Addr<StormGrokServer>) {
    let new_conn = connection_future.await.unwrap();
    let id = send_uuid_over_uni_pipe(new_conn.connection.clone()).await;

    let tcp_listener = listen_available_port().await;
    let tcp_addr = tcp_listener.local_addr().unwrap();
    info!("Found socket for session input {tcp_addr:?}");

    let session_address = session::StormGrokClientSession::start(
        id,
        tcp_addr.to_string(),
        new_conn,
        server_address.clone(),
    );

    server_address
        .send(Connect {
            id: id,
            session_data: (session_address.clone(), tcp_addr.to_string()),
        })
        .await
        .unwrap();

    session_address
        .send(session::StartListeningOnPort { tcp_listener })
        .await
        .unwrap();
}

impl StreamHandler<Connecting> for StormGrokServer {
    fn handle(&mut self, item: Connecting, ctx: &mut Self::Context) {
        info!("{:?}", item);
        start_session(item, ctx.address())
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
        match self.sessions.remove(&msg.id) {
            Some(_) => info!("Succesfully removed {:?}", msg.id),
            None => {
                error!("Tried to remove non existent session {:?}", msg.id);
                self.stop_handle.stop(true);
            }
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
        info!("Resolving {:?}", msg.id);

        match self.sessions.get(&msg.id) {
            Some(client_address) => Some(client_address.clone().1),
            None => None,
        }
    }
}
