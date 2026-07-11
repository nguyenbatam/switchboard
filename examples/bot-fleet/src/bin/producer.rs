//! Simulates the fleet: N bots, each publishing at a given rate, fire-and-forget.
//!
//! Before running this, do the arithmetic in `docs/14 §2`. It decides which row you are
//! in, and nothing else you decide matters as much:
//!
//! ```text
//! slots needed = rps × seconds-per-reply
//! ```
//!
//! Ollama at 2 concurrent (~5s) plus OpenAI at 10 (~2s) is ~12 slots and ~5.4 rps. Twenty
//! thousand bots at 100 messages/day is 23 rps, which needs 69 slots. That is 5× short,
//! and **no architecture in this crate will save you** — only more capacity, less work,
//! cache, or accepting less.
//!
//! Usage:
//!   cargo run --bin producer -- --bots 20000 --per-bot-per-day 10 --seconds 60

use std::time::Duration;

use anyhow::Result;
use bot_fleet::fleet::{Broker, Job, Publish};
use rand::Rng;

const STREAM: &str = "botfleet:jobs";
const GROUP: &str = "botfleet:workers";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());

    let broker = Broker::connect(&redis_url, STREAM, GROUP, args.max_backlog).await?;

    let offered_rps = args.bots as f64 * args.per_bot_per_day as f64 / 86_400.0;
    tracing::info!(
        bots = args.bots,
        offered_rps = format!("{offered_rps:.2}"),
        max_backlog = args.max_backlog,
        "starting; compare offered_rps against your provider capacity (docs/14 §2)"
    );

    let interval = if offered_rps > 0.0 {
        Duration::from_secs_f64(1.0 / offered_rps)
    } else {
        Duration::from_secs(1)
    };

    let (mut enqueued, mut refused) = (0u64, 0u64);
    let started = tokio::time::Instant::now();

    while started.elapsed() < Duration::from_secs(args.seconds) {
        // Each message belongs to a conversation. Reusing a small conversation space is
        // what makes coalescing visible: real users send bursts.
        let conv = rand::rng().random_range(0..args.conversations);
        let job = Job::new(
            format!("bot-{}", conv % args.bots),
            format!("conv-{conv}"),
            "What is the refund policy for EU customers?",
            Duration::from_secs(args.ttl_secs),
        );

        match broker.publish(&job).await? {
            Publish::Enqueued => enqueued += 1,
            Publish::Full => {
                // Degrade in the product, not in the infrastructure. The bot answers the
                // user *now*. It does not hold the message and hope.
                refused += 1;
            }
        }

        tokio::time::sleep(interval).await;
    }

    tracing::info!(
        enqueued,
        refused,
        backlog = broker.len().await?,
        "done — a growing backlog means the arithmetic in docs/14 §2 was wrong"
    );
    Ok(())
}

struct Args {
    bots: usize,
    per_bot_per_day: usize,
    seconds: u64,
    conversations: usize,
    max_backlog: usize,
    ttl_secs: u64,
}

impl Args {
    fn parse() -> Self {
        let get = |name: &str, default: usize| -> usize {
            let mut it = std::env::args().skip_while(|a| a != &format!("--{name}"));
            it.next();
            it.next().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        Self {
            bots: get("bots", 20_000),
            per_bot_per_day: get("per-bot-per-day", 10),
            seconds: get("seconds", 60) as u64,
            conversations: get("conversations", 500),
            max_backlog: get("max-backlog", 10_000),
            // 30s, per docs/14 §7: a person in Telegram is not a patient SDK.
            ttl_secs: get("ttl-secs", 30) as u64,
        }
    }
}
