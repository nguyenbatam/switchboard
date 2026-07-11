//! The consumer pool, standing alone. `N` is a constant you choose, not twenty thousand.
//!
//! Use this with `producer` for the two-terminal demo. Use `simulate` when you want a
//! bounded run and a report.
//!
//! ```bash
//! SWITCHBOARD_API_KEY=sk-sb-... cargo run --bin consumer -- --workers 16
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bot_fleet::contract::{AdaptiveLimiter, Config, GatewayClient};
use bot_fleet::economics::Ledger;
use bot_fleet::fleet::{Broker, Coalescer, ConsumerPool, Solvency};
use bot_fleet::platform::MockPlatform;

const STREAM: &str = "botfleet:jobs";
const GROUP: &str = "botfleet:workers";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let workers = arg("workers", 16);
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let base_url =
        std::env::var("SWITCHBOARD_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let api_key = std::env::var("SWITCHBOARD_API_KEY")
        .context("SWITCHBOARD_API_KEY is required; mint one with `sbctl create-key`")?;

    let cfg = Config {
        base_url,
        api_key,
        model: std::env::var("SWITCHBOARD_MODEL").unwrap_or_else(|_| "qwen3:4b".into()),
        // A bot's attempt budget, not an SDK's. docs/14 §7.
        request_timeout: Duration::from_secs(25),
        // Size the pool at the in-flight cap, not above it. docs/15 §8.
        pool_max_idle_per_host: workers,
        ..Config::default()
    };

    let client = Arc::new(GatewayClient::new(cfg)?);
    let broker = Arc::new(Broker::connect(&redis_url, STREAM, GROUP, 10_000).await?);

    // Start slightly above total provider concurrency (12 slots) so the pipes stay full,
    // and let AIMD find the real number. Floor of 1: never stop entirely.
    let limiter = AdaptiveLimiter::new(workers.min(16), 1, workers.max(16));
    let ledger = Arc::new(Ledger::new());
    let solvency = Arc::new(Solvency::default());
    let platform = Arc::new(MockPlatform::new());

    {
        let ledger = Arc::clone(&ledger);
        let limiter = Arc::clone(&limiter);
        let platform = Arc::clone(&platform);
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                // Awaiting inside a `tracing` macro holds its `Arguments<'_>` across the
                // await point, and `Arguments` is not `Send`. Resolve every value first.
                let backlog = broker.len().await.unwrap_or(0);
                let t = ledger.tallies();
                let answered = ledger.answers().len();
                tracing::info!(
                    backlog,
                    answered,
                    inflight_limit = limiter.limit(),
                    expired = t.expired_before_attempted,
                    coalesced = t.coalesced,
                    shed = t.shed_by_gateway,
                    busy_replies = platform.busy(),
                    "the four numbers from docs/14 §9"
                );
            }
        });
    }

    tracing::info!(workers, "draining; ctrl-c to stop");

    let pool = ConsumerPool {
        workers,
        broker,
        client,
        limiter,
        coalescer: Coalescer::new(),
        platform,
        ledger,
        solvency: Arc::clone(&solvency),
    };

    tokio::select! {
        r = pool.run() => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }

    if solvency.is_broke() {
        tracing::error!("halted: the account is out of money");
        std::process::exit(2);
    }
    Ok(())
}

fn arg(name: &str, default: usize) -> usize {
    let mut it = std::env::args().skip_while(|a| a != &format!("--{name}"));
    it.next();
    it.next().and_then(|v| v.parse().ok()).unwrap_or(default)
}
