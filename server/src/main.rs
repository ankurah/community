use ankurah::{policy::DEFAULT_CONTEXT as c, Node, PermissiveAgent};
use ankurah_websocket_server::WebsocketServer;
use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use community_model::{Room, RoomView};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt as _;
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use tracing::{info, Level};

#[cfg(all(feature = "sled", not(feature = "postgres")))]
use ankurah_storage_sled::SledStorageEngine;
#[cfg(feature = "postgres")]
use ankurah_storage_postgres::Postgres;

// Storage engine selected at generate time (the crate's default feature).
// dev.sh reads that choice back to decide whether to run a Postgres container.
#[cfg(all(feature = "sled", not(feature = "postgres")))]
type Storage = SledStorageEngine;
#[cfg(feature = "postgres")]
type Storage = Postgres;

#[cfg(all(feature = "sled", not(feature = "postgres")))]
async fn make_storage() -> Result<Storage> {
    Ok(SledStorageEngine::with_homedir_folder(".community")?)
}

#[cfg(feature = "postgres")]
async fn make_storage() -> Result<Storage> {
    // DATABASE_URL is provided by dev.sh (dev: randomized-port container) or by
    // Cloud Run (prod: Cloud SQL socket). The fallback is only for direct runs.
    let uri = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:ankurah@localhost:5432/community".to_string());
    Ok(Postgres::open(&uri).await?)
}

/// Shared HTTP state: where the built SPA (`trunk build`) lives on disk.
#[derive(Clone)]
struct AppState {
    static_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    // Initialize storage engine (Sled or Postgres — see this crate's features).
    let storage = make_storage().await?;
    let node = Node::new_durable(Arc::new(storage), PermissiveAgent::new());

    node.system.wait_loaded().await;
    if node.system.root().is_none() {
        node.system.create().await?;
    }

    // Seed the default community rooms (idempotent).
    ensure_default_rooms(&node).await?;

    // Where the built SPA lives. In prod the container copies `trunk build`
    // output here; in dev the dir is absent and trunk serves the SPA itself
    // (only /ws is proxied to this server), so the SPA fallback simply 404s.
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "static".to_string());
    let state = AppState { static_dir: PathBuf::from(static_dir) };

    // One axum app serves the Ankurah /ws endpoint AND the static SPA on a
    // single port, so the browser client is same-origin (mirrors idp.to). The
    // ws upgrade handler comes straight from ankurah-websocket-server.
    let ws_server = WebsocketServer::new(node);
    let app = Router::new()
        .route("/ws", get(ws_server.route_handler()))
        .route("/health", get(health))
        .fallback(spa_fallback)
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    // Cloud Run injects PORT; dev.sh sets SERVER_PORT; default 8080.
    let port = std::env::var("PORT")
        .or_else(|_| std::env::var("SERVER_PORT"))
        .unwrap_or_else(|_| "8080".to_string());
    let bind_addr: SocketAddr = format!("0.0.0.0:{port}").parse().context("invalid bind address")?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind community-server on {bind_addr}"))?;
    info!("community-server listening on {}", listener.local_addr()?);

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

/// Serve the SPA: real files from `static_dir`, with `index.html` as the
/// client-side-routing fallback. Backend paths never fall through to the SPA.
async fn spa_fallback(State(state): State<AppState>, request: Request<Body>) -> Response {
    let path = request.uri().path();
    if is_backend_path(path) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let index = state.static_dir.join("index.html");
    let service = ServeDir::new(&state.static_dir).fallback(ServeFile::new(index));

    service.oneshot(request).await.map(IntoResponse::into_response).unwrap_or_else(|error| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to serve assets: {error}")).into_response()
    })
}

fn is_backend_path(path: &str) -> bool {
    path == "/ws" || path.starts_with("/ws/") || path == "/health"
}

/// Seed the default rooms for the community. Idempotent — only creates rooms
/// that don't already exist, so it's safe to run on every boot.
async fn ensure_default_rooms(node: &Node<Storage, PermissiveAgent>) -> Result<()> {
    const DEFAULT_ROOMS: &[&str] = &["general", "support", "announcements", "introductions"];

    let context = node.context_async(c).await;

    for name in DEFAULT_ROOMS {
        let query = format!("name = '{name}'");
        let existing = context.fetch::<RoomView>(query.as_str()).await?;
        if existing.is_empty() {
            info!("Creating '{name}' room");
            let trx = context.begin();
            trx.create(&Room { name: name.to_string() }).await?;
            trx.commit().await?;
        }
    }

    Ok(())
}
