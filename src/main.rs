#![feature(addr_parse_ascii)]

use actix::{Actor, Addr};
use actix_web::dev::RequestHead;
use actix_web::http::header::HeaderMap;
use actix_web::http::header::HeaderValue;
use actix_web::http::StatusCode;
use actix_web::HttpResponseBuilder;
use actix_web::{
    dev::ServerHandle,
    http::{Method, Uri, Version},
    web, App, Error, HttpRequest, HttpResponse, HttpServer,
};
use actix_web_actors::ws;
use async_std::task;
use log::info;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{collections::HashMap, str::FromStr, time::Instant};
use uuid::Uuid;

mod server;
mod session;

async fn index(
    req: HttpRequest,
    stream: web::Payload,
    srv: web::Data<Addr<server::StormGrokServer>>,
) -> Result<HttpResponse, Error> {
    let id = Uuid::new_v4();
    let resp = ws::start(
        session::StormGrokClientSession {
            id: id,
            last_heart_beat: Instant::now(),
            server_address: srv.get_ref().clone(),
            request_number: 0,
            responses: HashMap::new(),
        },
        &req,
        stream,
    );
    info!("{srv:?}");
    info!("{:?}", resp);
    resp
}

async fn forward(
    req: HttpRequest,
    body: web::Bytes,
    srv: web::Data<Addr<server::StormGrokServer>>,
) -> HttpResponse {
    let frd = session::FullRequestData {
        method: req.head().method.to_string(),
        uri: req.head().uri.to_string(),
        version: format!("{:?}", req.head().version),
        headers: req
            .head()
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_owned()))
            .collect::<Vec<_>>(),
        peer_addr: req.head().peer_addr,
        body: body.to_vec(),
    };

    // Todo: This error handling can probably be improved, lol
    if let Some(host) = req.headers().get("host") {
        if let Ok(host) = host.to_str() {
            if let Some(client_id) = host.split(".").next() {
                if let Ok(id) = Uuid::parse_str(client_id) {
                    if let Ok(Some(client_address)) =
                        srv.send(server::ResolveClient { id: id }).await
                    {
                        info!("client_address={client_address:?}");
                        let response_number = client_address
                            .send(session::ForwardRequest { request_data: frd })
                            .await
                            .unwrap();
                        info!("Start polling for response {response_number:?}");

                        // Todo: This polling is disgusting, change it to streamhandling somehow:
                        // https://actix.rs/actix/actix/trait.StreamHandler.html
                        for _ in 1..30 {
                            task::sleep(Duration::from_secs_f64(0.1)).await;
                            match client_address
                                .send(session::PollResponse {
                                    request_number: response_number,
                                })
                                .await
                                .unwrap()
                            {
                                Some(e) => {
                                    let mut r = HttpResponseBuilder::new(
                                        StatusCode::from_u16(e.status).unwrap(),
                                    );
                                    for (k, v) in e.headers.iter() {
                                        r.append_header((
                                            k.as_str(),
                                            HeaderValue::from_bytes(v).unwrap(),
                                        ));
                                    }
                                    return r.body(e.body);
                                }
                                None => info!("Polling for response {:?}!", response_number),
                            }
                        }
                        // TODO: This should be a 404 I guess? but actually this shouldn't exist and I need to get the stream thingy
                        HttpResponse::Ok().body("partyparty")
                    } else {
                        HttpResponse::Ok().body(format!("{client_id} is not currently connected"))
                    }
                } else {
                    HttpResponse::NotFound()
                        .body(format!("{client_id} could not be parsed to client_id"))
                }
            } else {
                HttpResponse::NotFound().body("This is impossibru!!!!")
            }
        } else {
            HttpResponse::NotFound().body("Host was not a valid string")
        }
    } else {
        HttpResponse::NotFound().body("Host not found")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let stop_handle = web::Data::new(StopHandle::default());
    let server_address = server::StormGrokServer {
        sessions: HashMap::new(),
        stop_handle: stop_handle.clone(),
    }
    .start();

    log::info!("starting storm grok server at http://localhost:3000",);

    let srv = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(server_address.clone()))
            .route("/ws/", web::get().to(index))
            .default_service(web::route().to(forward))
    })
    .bind(("127.0.0.1", 3000))?
    .run();
    stop_handle.register(srv.handle());
    srv.await
}

// This comes from: https://github.com/actix/examples/tree/master/shutdown-server
#[derive(Debug, Default)]
pub struct StopHandle {
    pub inner: Mutex<Option<ServerHandle>>,
}
impl StopHandle {
    /// Sets the server handle to stop.
    pub(crate) fn register(&self, handle: ServerHandle) {
        *self.inner.lock() = Some(handle);
    }

    /// Sends stop signal through contained server handle.
    pub(crate) fn stop(&self, graceful: bool) {
        let _ = self.inner.lock().as_ref().unwrap().stop(graceful);
    }
}
