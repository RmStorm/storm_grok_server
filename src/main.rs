use actix::Addr;
use actix_web::{
    dev::ServerHandle, error, guard::GuardContext, http::header, web,
    App, Error, HttpMessage, HttpRequest, HttpResponse, HttpServer,
};
use parking_lot::Mutex;
use rustls::ServerConfig;
use uuid::Uuid;

use awc::Client;
use tracing::{debug, info};

use url::Url;

mod server;
mod session;
mod settings;

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

async fn uuid_forwarder(
    req: HttpRequest,
    payload: web::Payload,
    http_client: web::Data<Client>,
    srv: web::Data<Addr<server::StormGrokServer>>,
) -> Result<HttpResponse, Error> {
    let id = req.extensions().get::<Uuid>().unwrap().clone();
    if let Some(client_addr) = srv.send(server::ResolveClient { id: id }).await.unwrap() {
        forward(req, payload, client_addr, http_client).await
    } else {
        Ok(HttpResponse::NotFound().body("No connected client found\n"))
    }
}

async fn index(_req: HttpRequest, _body: web::Bytes) -> HttpResponse {
    // info!("\nREQ: {req:?}");
    // info!("body: {body:?}");
    HttpResponse::Ok().body("hit index\n")
}

async fn print_clients_in_log(srv: web::Data<Addr<server::StormGrokServer>>) -> HttpResponse {
    srv.send(server::LogAllClients {}).await.unwrap();
    HttpResponse::Ok().body("check your logs!\n")
}

fn resolve_uuid_from_host(host: &str) -> Option<Uuid> {
    let client_id = host.split(".").next()?;
    let id = Uuid::parse_str(client_id).ok();
    id
}

fn uuid_guard(g_ctx: &GuardContext) -> bool {
    if let Some(host_header) = g_ctx.head().headers().get(header::HOST) {
        if let Ok(host) = host_header.to_str() {
            if let Some(uuid) = resolve_uuid_from_host(host) {
                g_ctx.req_data_mut().insert(uuid);
                return true;
            }
        }
    }
    debug!("Did not find uuid in host header '{:?}', try to fallback to uri.", g_ctx.head().headers());
    if let Some(host) = g_ctx.head().uri.host() {
        if let Some(uuid) = resolve_uuid_from_host(host) {
            g_ctx.req_data_mut().insert(uuid);
            return true;
        }
    }
    false
}

#[actix_web::main]
async fn main() -> Result<(), std::io::Error> {
    let config = settings::Settings::new();
    let stop_handle = web::Data::new(StopHandle::default());
    let server_address = server::StormGrokServer::start(stop_handle.clone(), &config);

    let srv = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(Client::default()))
            .app_data(web::Data::new(server_address.clone()))
            .service(
                web::resource("{tail:.*}")
                    .guard(uuid_guard)
                    .to(uuid_forwarder),
            )
            .service(web::resource("/print_clients").to(print_clients_in_log))
            .service(web::resource("/").to(index))
    });

    info!(
        "starting storm grok server at {}:{:?}",
        config.server.host, config.server.http_port
    );
    let srv = if config.env == settings::ENV::Prod {
        let (certs, key) = config.get_certs_and_key();
        srv.bind_rustls(
            (config.server.host, config.server.http_port),
            ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .expect("bad certificate/key"),
        )?
    } else {
        srv.bind((config.server.host, config.server.http_port))?
    }
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
