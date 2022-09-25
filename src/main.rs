use uuid::Uuid;
use actix::Addr;
use actix_web::{dev::ServerHandle, error, web, App, HttpRequest, HttpResponse, HttpServer, Error, http::header::HeaderValue};
use parking_lot::Mutex;

use tracing::info;
use tracing_subscriber;
use awc::Client;
use url::Url;

mod server;
mod session;


async fn forward(
    req: HttpRequest,
    payload: web::Payload,
    client_addr: String,
    http_client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    let mut new_url = Url::parse(&format!("http://{client_addr}")).unwrap();
    new_url.set_path(req.uri().path());
    new_url.set_query(req.uri().query());

    // TODO: This forwarded implementation is incomplete as it only handles the unofficial
    // X-Forwarded-For header but not the official Forwarded one.
    let forwarded_req = http_client
        .request_from(new_url.as_str(), req.head())
        .no_decompress();
    let forwarded_req = match req.head().peer_addr {
        Some(addr) => forwarded_req.insert_header(("x-forwarded-for", format!("{}", addr.ip()))),
        None => forwarded_req,
    };

    let res = forwarded_req
        .send_stream(payload)
        .await
        .map_err(error::ErrorInternalServerError)?;

    let mut client_resp = HttpResponse::build(res.status());
    // Remove `Connection` as per
    // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Connection#Directives
    for (header_name, header_value) in res.headers().iter().filter(|(h, _)| *h != "connection") {
        client_resp.insert_header((header_name.clone(), header_value.clone()));
    }

    Ok(client_resp.streaming(res))
}

async fn resolve_uuid_from_host(host: &HeaderValue, srv: web::Data<Addr<server::StormGrokServer>>) -> Option<String> {
    let host = host.to_str().ok()?;
    let client_id = host.split(".").next()?;
    let id = Uuid::parse_str(client_id).ok()?;
    srv.send(server::ResolveClient { id: id }).await.ok()?
}

async fn default(
    req: HttpRequest,
    payload: web::Payload,
    http_client: web::Data<Client>,
    srv: web::Data<Addr<server::StormGrokServer>>,
) -> Result<HttpResponse, Error>  {
    info!("\nREQ: {req:?}");
    // info!("payload: {payload:?}");
    info!("srv: {srv:?}");
    if let Some(host) = req.headers().get("host") {
        if let Some(client_addr) = resolve_uuid_from_host(host, srv).await {
            forward(req, payload, client_addr, http_client).await
        } else {
            Ok(HttpResponse::NotFound().body("No connected client found\n"))
        }
    } else {
        Ok(HttpResponse::BadRequest().body("Your request needs a host header!\n"))
    }
}

#[actix_web::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt::init();
    let stop_handle = web::Data::new(StopHandle::default());
    let server_address = server::StormGrokServer::start(stop_handle.clone());

    info!("starting storm grok server at http://localhost:3000",);
    let srv = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(Client::default()))
            .app_data(web::Data::new(server_address.clone()))
            .default_service(web::route().to(default))
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
