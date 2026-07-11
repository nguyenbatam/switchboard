//! One process: a bot fleet, a consumer pool, a real gateway, and a report.
//!
//! This is what `run_local_test.sh` runs. It answers the question a third party actually
//! has — *"I put $A in. What did I get, and would I have been better off calling OpenAI
//! directly?"* — and it is arranged so that the answer can be **no**.
//!
//! It ends by reconciling three independently-kept sets of books:
//!
//! 1. the client's own arithmetic, from `response.model`, `usage`, and the rate card;
//! 2. the gateway's rollup, from `GET /v1/usage`;
//! 3. the gateway's fact table, from `GET /v1/usage/records`.
//!
//! All three must agree to the nanodollar. Any disagreement fails the run, because a
//! latency number from a system that cannot count money is not worth reading.
//!
//! ```bash
//! SWITCHBOARD_API_KEY=sk-sb-... cargo run --bin simulate -- \
//!   --bots 20000 --per-bot-per-day 10 --seconds 45 --workers 16
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bot_fleet::contract::{AdaptiveLimiter, Config, GatewayClient};
use bot_fleet::economics::audit;
use bot_fleet::economics::report::{fact_vs_rollup_delta, render_history};
use bot_fleet::economics::{Ledger, PriceTable, Report, Verdict};
use bot_fleet::fleet::{Broker, Coalescer, ConsumerPool, Job, Publish, Solvency};
use bot_fleet::platform::MockPlatform;
use rand::Rng;
use uuid::Uuid;

/// How many history rows we are willing to page through. Beyond this we decline to assert
/// the fact-vs-rollup invariant rather than assert it on a truncated set.
const HISTORY_CAP: usize = 2_000;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let base_url =
        std::env::var("SWITCHBOARD_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let api_key = std::env::var("SWITCHBOARD_API_KEY")
        .context("SWITCHBOARD_API_KEY is required; mint one with `sbctl create-key`")?;
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let model = std::env::var("SWITCHBOARD_MODEL").unwrap_or_else(|_| "qwen3:4b".into());

    let client = Arc::new(GatewayClient::new(Config {
        base_url,
        api_key,
        model: model.clone(),
        request_timeout: Duration::from_secs(25),
        pool_max_idle_per_host: args.workers,
        ..Config::default()
    })?);

    // Before spending anything: does this client still know what the gateway charges?
    //
    // The price table in `economics::prices` is a client-side copy, and a copy goes stale
    // silently. Without this check the first symptom is a reconciliation failure at the end
    // of a run that has already bought the tokens. `GET /v1/models` costs one request.
    match client.models().await {
        Ok(cards) => {
            let drift = bot_fleet::economics::prices::PriceTable::published().drift_against(&cards);
            for d in &drift {
                tracing::warn!(%d, "the local price table disagrees with the gateway's rate card");
            }
            if !drift.is_empty() {
                tracing::warn!(
                    "the per-request audit below will report every charge it cannot verify; \
                     update economics::prices, or find out why the gateway's prices moved"
                );
            }
        }
        // An older gateway without a rate card is not a reason to refuse to run.
        Err(e) => tracing::warn!(error = %e, "could not read the gateway's rate card"),
    }

    // The books, before anything happens. Everything below is a delta against this.
    let before = client
        .usage_settled(Duration::from_millis(500), Duration::from_secs(10))
        .await
        .context("reading the gateway's ledger before the run")?;

    match before.monthly_budget_nano {
        Some(limit) => tracing::info!(
            model = %model,
            funded_usd = %bot_fleet::economics::money::usd_short(limit),
            remaining_usd = %bot_fleet::economics::money::usd_short(before.remaining_nano.unwrap_or(0)),
            "starting"
        ),
        None => tracing::warn!(
            "this tenant has no monthly budget: the quota path will never fire. \
             Set one with `sbctl set-budget <tenant> 0.002`."
        ),
    }

    let offered_rps = args.bots as f64 * args.per_bot_per_day as f64 / 86_400.0;
    tracing::info!(
        bots = args.bots,
        offered_rps = format!("{offered_rps:.2}"),
        workers = args.workers,
        "offered load — compare against provider capacity (docs/14 §2) before believing any verdict"
    );

    // A stream per run. Two simulations sharing a stream would each drain the other's work
    // and both would report nonsense.
    let stream = format!("botfleet:sim:{}", Uuid::now_v7());
    let broker =
        Arc::new(Broker::connect(&redis_url, &stream, "botfleet:workers", args.max_backlog).await?);

    let ledger = Arc::new(Ledger::new());
    let solvency = Arc::new(Solvency::default());
    let limiter = AdaptiveLimiter::new(args.workers.min(16), 1, args.workers.max(16));
    let platform: Arc<dyn bot_fleet::platform::Platform> = Arc::new(MockPlatform::new());

    let pool = ConsumerPool {
        workers: args.workers,
        broker: Arc::clone(&broker),
        client: Arc::clone(&client),
        limiter: Arc::clone(&limiter),
        coalescer: Coalescer::new(),
        platform: Arc::clone(&platform),
        ledger: Arc::clone(&ledger),
        solvency: Arc::clone(&solvency),
    };
    let run_started = std::time::Instant::now();
    let consumers = tokio::spawn(pool.run());

    produce(&broker, &ledger, &solvency, &args, offered_rps).await?;
    drain(&broker, &solvency, Duration::from_secs(args.drain_secs)).await?;

    // The workers are parked on a blocking XREADGROUP; there is nothing left for them.
    consumers.abort();

    // The usage row is written by a task the request path spawned and did not await, so the
    // ledger is still catching up. Reading it now would under-report spend — an error in
    // the flattering direction, which is the one to be most suspicious of.
    let after = client
        .usage_settled(Duration::from_secs(1), Duration::from_secs(20))
        .await
        .context("reading the gateway's ledger after the run")?;

    let answers = ledger.answers();
    let tallies = ledger.tallies();
    let prices = PriceTable::published();
    let report = Report::build(
        before,
        after,
        &answers,
        tallies,
        &prices,
        run_started.elapsed().as_secs_f64(),
        limiter.peak_inflight(),
    );

    print!("{}", report.render());

    let history = client.usage_history(HISTORY_CAP).await.unwrap_or_default();
    if !history.is_empty() {
        print!("{}", render_history(&history));
    }

    // The per-request audit. Totals can agree while every row is wrong, so this is the
    // check that actually protects a customer: each message charged once, at the rate
    // card, and everything that never reached a provider charged nothing.
    let refusals = ledger.refusals();
    let findings = audit::audit(&answers, &refusals, &history, &prices);
    print!(
        "{}",
        audit::render(&findings, answers.len() + refusals.len())
    );

    println!();
    let mut failed = !findings.is_empty();

    // Reconciliation 1: the client's arithmetic against the gateway's rollup.
    if report.reconciliation_delta_nano != 0 {
        eprintln!(
            "✗ client vs gateway rollup: ${}",
            bot_fleet::economics::money::usd_short(report.reconciliation_delta_nano)
        );
        failed = true;
    } else {
        println!("✓ client's arithmetic matches the gateway's rollup");
    }

    // Reconciliation 2: the gateway's fact table against its own rollup. They are written
    // in one transaction; if they disagree, every aggregate it has ever served is suspect.
    if history.len() >= HISTORY_CAP {
        println!("… fact-vs-rollup check skipped: history exceeds {HISTORY_CAP} rows");
    } else if !history.is_empty() {
        let delta = fact_vs_rollup_delta(&history, &after);
        if delta != 0 {
            eprintln!(
                "✗ gateway fact table vs its own rollup: ${}",
                bot_fleet::economics::money::usd_short(delta)
            );
            failed = true;
        } else {
            println!("✓ the gateway's fact table sums to its own rollup");
        }
    }

    // Reconciliation 3: every answer the client holds is a row in the gateway's history.
    // A missing row is an answer somebody was never billed for.
    let billable_rows = history
        .iter()
        .filter(|r| r.status != "error")
        .count()
        .saturating_sub(before.total_requests as usize);
    if !history.is_empty() && billable_rows < answers.len() {
        eprintln!(
            "✗ the client holds {} answers but the gateway recorded {billable_rows} non-error calls",
            answers.len()
        );
        failed = true;
    } else if !history.is_empty() {
        println!("✓ every answer the client holds appears in the gateway's history");
    }

    if solvency.is_broke() {
        println!(
            "\nThe fleet halted because the account ran out of money — which is the point of \
             the exercise, and the only failure in this list that is not a bug."
        );
    }

    if failed {
        eprintln!("\nThe books do not agree. Do not read the latency numbers above.");
        std::process::exit(1);
    }
    if matches!(report.verdict, Verdict::BillingDisagreement { .. }) {
        std::process::exit(1);
    }
    Ok(())
}

/// Twenty thousand bots, each publishing and forgetting.
async fn produce(
    broker: &Broker,
    ledger: &Ledger,
    solvency: &Solvency,
    args: &Args,
    offered_rps: f64,
) -> Result<()> {
    let interval = if offered_rps > 0.0 {
        Duration::from_secs_f64(1.0 / offered_rps)
    } else {
        Duration::from_secs(1)
    };

    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(args.seconds) && !solvency.is_broke() {
        // A small conversation space, because real users send bursts and coalescing is only
        // visible when two messages land on one conversation.
        let conv = rand::rng().random_range(0..args.conversations);
        let job = Job::new(
            format!("bot-{}", conv % args.bots.max(1)),
            format!("conv-{conv}"),
            // A short, deterministic prompt: `temperature` is unset, so identical prompts
            // are cache-eligible and the hit rate becomes visible in the report.
            format!("Reply with one short sentence about topic {}.", conv % 8),
            Duration::from_secs(args.ttl_secs),
        );

        ledger.tally(|t| t.offered += 1);
        match broker.publish(&job).await? {
            Publish::Enqueued => {}
            // Degrade in the product, not in the infrastructure: the bot answers the user
            // now. It does not hold the message and hope.
            Publish::Full => ledger.tally(|t| t.broker_full += 1),
        }

        tokio::time::sleep(interval).await;
    }
    Ok(())
}

/// Wait for the backlog to clear, so the report counts answers rather than a snapshot of a
/// half-finished queue.
async fn drain(broker: &Broker, solvency: &Solvency, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if solvency.is_broke() {
            tracing::warn!("account exhausted; abandoning the backlog");
            return Ok(());
        }
        let backlog = broker.len().await.unwrap_or(0);
        if backlog == 0 {
            // Give the last in-flight requests a moment to land in the ledger.
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                backlog,
                "drain timed out; the report will under-count answers"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

struct Args {
    bots: usize,
    per_bot_per_day: usize,
    seconds: u64,
    drain_secs: u64,
    conversations: usize,
    max_backlog: usize,
    ttl_secs: u64,
    workers: usize,
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
            seconds: get("seconds", 30) as u64,
            drain_secs: get("drain-secs", 60) as u64,
            conversations: get("conversations", 40),
            max_backlog: get("max-backlog", 10_000),
            // 30s, per docs/14 §7: a person in Telegram is not a patient SDK.
            ttl_secs: get("ttl-secs", 30) as u64,
            workers: get("workers", 8),
        }
    }
}
