use axum::body::Body;
use hyper::client::HttpConnector;
use hyper_rustls::HttpsConnector;
use rustls::ServerConfig;

use axum::response::Response;

use axum::http::status::StatusCode;
use axum::{http::Request, routing::any};
use axum_server::tls_rustls::RustlsConfig;
use hyper::Uri;
use tower::util::ServiceExt;

use axum::Extension;
use tracing::info;
use uuid::Uuid;

use axum::{extract::Host, Router};
use jsonwebtoken::DecodingKey;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

mod google_key_store;
mod server;
mod session;
mod settings;

type KeyMap = Arc<RwLock<HashMap<String, DecodingKey>>>;
type ClientMap = Arc<RwLock<HashMap<Uuid, String>>>;
type HttpClient = hyper::client::Client<HttpConnector, Body>;
type HttpsClient = hyper::client::Client<HttpsConnector<HttpConnector>, Body>;

async fn forwarder(
    Extension(client): Extension<HttpClient>,
    Extension(client_map): Extension<ClientMap>,
    host: Host,
    mut req: Request<Body>,
) -> Response<Body> {
    let path_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(req.uri().path());

    let uuid = resolve_uuid_from_host(&host.0).unwrap();

    let uri = match client_map.read().get(&uuid) {
        Some(addr) => format!("http://{}{}", addr, path_query),
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("No active client found\n"))
                .unwrap();
        }
    };

    *req.uri_mut() = Uri::try_from(uri).unwrap();
    client.request(req).await.unwrap()
}

async fn handler(Extension(client_map): Extension<ClientMap>, host: Host) -> &'static str {
    println!("{:?}", host);
    println!("{:?}", client_map);
    "Hello, world!\n"
}

fn resolve_uuid_from_host(host: &str) -> Option<Uuid> {
    let client_id = host.split(".").next()?;
    let id = Uuid::parse_str(client_id).ok();
    id
}

#[tokio::main]
async fn main() {
    let config = settings::Settings::new();
    let key_store: KeyMap = Arc::new(RwLock::new(HashMap::new()));
    let client_map: ClientMap = Arc::new(RwLock::new(HashMap::new()));
    let sg_server = server::start_storm_grok_server(&config, client_map.clone(), key_store.clone());

    let client: HttpClient = hyper::Client::new();

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_only()
        .enable_http1()
        .build();
    let https_client: HttpsClient = hyper::Client::builder().build(https);

    let forwarder_router = Router::new().route("/*path", any(forwarder));
    let default_router = Router::new().route("/*path", any(handler));

    let app = Router::new()
        .route(
            "/*path",
            any(|Host(hostname): Host, request: Request<Body>| async move {
                match resolve_uuid_from_host(hostname.as_str()) {
                    Some(_uuid) => forwarder_router.oneshot(request).await,
                    None => default_router.oneshot(request).await,
                }
            }),
        )
        .layer(Extension(client_map))
        .layer(Extension(client));

    let addr = format!("{}:{}", config.server.host, config.server.http_port);
    info!("starting storm grok server at {}", addr);
    let addr: SocketAddr = addr.parse().unwrap();

    if config.env == settings::ENV::Prod {
        let (certs, key) = config.get_certs_and_key();
        let tls_config = RustlsConfig::from_config(Arc::new(
            ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .expect("bad certificate/key"),
        ));
        let http_serve = axum_server::bind_rustls(addr, tls_config).serve(app.into_make_service());
        tokio::select!(
            _ = http_serve => {},
            _ = sg_server => {},
            _ = google_key_store::refresh_loop(key_store, https_client) => {},
        );
    } else {
        let http_serve = axum_server::bind(addr).serve(app.into_make_service());
        tokio::select!(
            _ = http_serve => {},
            _ = sg_server => {},
            _ = google_key_store::refresh_loop(key_store, https_client) => {},
        );
    };
}
