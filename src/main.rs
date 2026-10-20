use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};

use heed::{Database, Env, EnvOpenOptions, types::Bytes};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

mod config;
mod identity;
mod names;
mod password_work;
mod rate_limit;
mod registration;
mod socket;
mod spa;
mod storage;
mod token;
pub mod types;

struct ServerState {
    environment: Env,
    database: Database<Bytes, Bytes>,
    config: config::Config,
    limits: rate_limit::RateLimiter,
}

type AppState = Arc<ServerState>;

fn status(code: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (
        code,
        Json(serde_json::json!({ "fakywaky": message.into().to_lowercase() })),
    )
        .into_response()
}

pub fn format_uuid(uuid: &Uuid) -> String {
    let hex = uuid.simple().to_string();
    format!(
        "{}:{}:{}:{}:{}:{}",
        &hex[..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..24],
        &hex[24..28],
        &hex[28..32],
    )
}

pub fn generate_uuid_v7() -> String {
    format_uuid(&Uuid::now_v7())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::load()?;
    let registration_body_limit = config.registration_body_limit();
    let database_path = PathBuf::from(
        std::env::var("SLIMEPROTO_DATA_PATH")
            .unwrap_or_else(|_| "data/slimeproto-identities-v1".into()),
    );
    std::fs::create_dir_all(&database_path)?;
    let environment = unsafe {
        EnvOpenOptions::new()
            .max_dbs(1)
            .map_size(config.database_map_size_bytes)
            .open(&database_path)?
    };
    let mut transaction = environment.write_txn()?;
    let database: Database<Bytes, Bytes> =
        environment.create_database(&mut transaction, Some("slimeproto"))?;
    transaction.commit()?;

    identity::initialize_schema(&environment, database)?;

    spa::migrate_collection_index(&environment, database)?;
    spa::migrate_document_indexes(&environment, database)?;
    spa::migrate_reference_index(&environment, database)?;
    storage::migrate(&environment, database)?;
    token::initialize_sessions(&environment, database)
        .map_err(|error| format!("session initialization failed: {error:?}"))?;

    std::sync::LazyLock::force(&socket::PUBSUB_POOL);

    let state = Arc::new(ServerState {
        environment,
        database,
        config,
        limits: rate_limit::RateLimiter::default(),
    });
    spa::start_document_writer(state.clone())?;
    token::start_cleanup(state.clone());
    let maintenance_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            maintenance_state.limits.prune();
        }
    });

    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route(
            "/register",
            post(registration::register_handler)
                .layer(DefaultBodyLimit::max(registration_body_limit)),
        )
        .route("/ws", get(socket::ws_handler))
        .route(
            "/identity/export",
            post(identity::export_handler).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/identity/import",
            post(identity::import_handler).layer(DefaultBodyLimit::max(registration_body_limit)),
        )
        .route(
            "/spa",
            post(spa::spa_handler)
                .get(spa::spa_get_handler)
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/token",
            post(token::token_handler).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::middleware,
        ))
        .layer(CorsLayer::permissive().expose_headers([axum::http::header::RETRY_AFTER]))
        .with_state(state);

    let bind_address =
        std::env::var("SLIMEPROTO_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:3000".to_owned());
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
