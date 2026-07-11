use switchboard::app::AppState;
use switchboard::config::AppConfig;
use switchboard::http;
use switchboard::telemetry::{init_metrics, init_tracing};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = dotenvy::dotenv();
    let config = AppConfig::load()?;
    init_tracing(&config);
    init_metrics(config.telemetry.metrics_enabled);

    let state = AppState::boot(config.clone()).await?;
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "switchboard listening");

    let app = http::router(state);

    // `into_make_service_with_connect_info` is what puts the peer's `SocketAddr` into the
    // request extensions. Without it, `http::extract::client_ip` has no socket to trust,
    // every caller collapses into the `"unknown"` bucket, and the per-IP limiter guarding
    // the argon2 verify throttles the innocent along with the guilty.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
