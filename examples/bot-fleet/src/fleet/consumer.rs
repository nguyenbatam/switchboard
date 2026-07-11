//! The bounded consumer pool — `N` tasks, not twenty thousand.
//!
//! ```text
//! 20k bots ──(publish, fire-and-forget)──► broker ──► consumer pool (N ≈ 16)
//!                                            │              │
//!                                     bounded stream        ▼
//!                                            │        gateway /v1/chat  (synchronous)
//!                                     full → "busy"         │
//!                                                    Ollama(2) · OpenAI(10)
//!                                                           │
//!                                        consumer sends the reply via the bot's own API
//! ```
//!
//! `N` is slightly above total provider concurrency — 16 against 12 slots — so the pipes
//! stay full without queueing upstream. The gateway never sees more than `N` concurrent
//! requests, so it can stay synchronous, and no job table exists inside it.
//!
//! One caution from `docs/14 §6`, and it is the one that bites: **a per-process limit is
//! not a limit.** Three consumer pods each holding `N = 16` send 48 concurrent requests.
//! Run one consumer deployment, `replicas: 1`, or divide `N` by the replica count.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::contract::{AdaptiveLimiter, GatewayClient, Outcome, idempotency_key};
use crate::economics::Ledger;
use crate::economics::ledger::{Answer as LedgerAnswer, ms};
use crate::fleet::broker::Broker;
use crate::fleet::coalesce::Coalescer;
use crate::platform::Platform;

/// Set when the gateway says the account is out of money.
///
/// This is the one condition that stops the fleet rather than slowing it. Every other
/// failure is transient and deserves a retry, a backoff, or a smaller concurrency limit.
/// An empty account deserves none of those: no amount of patience produces funds, and a
/// fleet that keeps trying will do so until the billing month turns over.
#[derive(Default)]
pub struct Solvency(AtomicBool);

impl Solvency {
    pub fn is_broke(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn declare_broke(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub struct ConsumerPool {
    pub workers: usize,
    pub broker: Arc<Broker>,
    pub client: Arc<GatewayClient>,
    pub limiter: Arc<AdaptiveLimiter>,
    pub coalescer: Arc<Coalescer>,
    pub platform: Arc<dyn Platform>,
    pub ledger: Arc<Ledger>,
    pub solvency: Arc<Solvency>,
}

impl ConsumerPool {
    pub async fn run(self) -> Result<()> {
        let mut set = tokio::task::JoinSet::new();

        for i in 0..self.workers {
            set.spawn(worker(Worker {
                name: format!("consumer-{i}"),
                broker: Arc::clone(&self.broker),
                client: Arc::clone(&self.client),
                limiter: Arc::clone(&self.limiter),
                coalescer: Arc::clone(&self.coalescer),
                platform: Arc::clone(&self.platform),
                ledger: Arc::clone(&self.ledger),
                solvency: Arc::clone(&self.solvency),
            }));
        }

        // A worker returns either because the account is empty — in which case they all
        // will, and that is orderly — or because it died, which is not.
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                tracing::error!(error = %e, "a consumer panicked");
                set.abort_all();
                anyhow::bail!("consumer pool degraded; restarting is the correct response");
            }
        }
        Ok(())
    }
}

struct Worker {
    name: String,
    broker: Arc<Broker>,
    client: Arc<GatewayClient>,
    limiter: Arc<AdaptiveLimiter>,
    coalescer: Arc<Coalescer>,
    platform: Arc<dyn Platform>,
    ledger: Arc<Ledger>,
    solvency: Arc<Solvency>,
}

async fn worker(w: Worker) {
    while !w.solvency.is_broke() {
        let deliveries = match w.broker.consume(&w.name, 1, 2_000).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, "broker read failed");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        for d in deliveries {
            handle(&w, &d.job).await;
            if let Err(e) = w.broker.ack(&d.id).await {
                tracing::warn!(error = %e, "ack failed; job will be redelivered");
            }
        }
    }
}

async fn handle(w: &Worker, job: &crate::fleet::Job) {
    let deadline = job.deadline();
    let ledger = &w.ledger;

    // 1. Has the user already left? Answer nothing; charge nobody. This counter is the
    //    one that says you are accepting work you cannot do.
    if deadline.expired() {
        ledger.tally(|t| t.expired_before_attempted += 1);
        tracing::debug!(job = %job.job_id, waited = ?job.queue_wait(), "expired before attempted");
        return;
    }

    // 2. One in-flight request per conversation. Coalesce, do not enqueue: the user is
    //    going to read one reply, so buy one answer.
    let Some(_guard) = w.coalescer.try_begin(&job.conversation_id) else {
        ledger.tally(|t| t.coalesced += 1);
        return;
    };

    // 3. Wait for the fleet's adaptive concurrency budget. This is what a `503` taught us.
    let _permit = w.limiter.acquire().await;

    let queue_wait = job.queue_wait();
    let started = Instant::now();
    let key = idempotency_key(&job.conversation_id, &job.message_id);
    let request_id = job.job_id.to_string();

    let limiter_for_shed = Arc::clone(&w.limiter);
    // The conversation is the end user, as far as the gateway's ledger is concerned. It
    // lands on `usage_records.end_user_id`, which is how this fleet's operator answers
    // "which of my users spent this" without keeping a second ledger of their own.
    let outcome = w
        .client
        .complete(
            &job.text,
            &key,
            &request_id,
            &deadline,
            Some(&job.conversation_id),
            move || {
                limiter_for_shed.on_shed();
            },
        )
        .await;

    match outcome {
        Outcome::Answered(answer) => {
            w.limiter.on_success();
            ledger.record(LedgerAnswer {
                request_id: request_id.clone(),
                queue_wait_ms: ms(queue_wait),
                gateway_ms: ms(answer.latency),
                end_to_end_ms: ms(queue_wait) + ms(started.elapsed()),
                prompt_tokens: answer.prompt_tokens,
                completion_tokens: answer.completion_tokens,
                requested_model: w.client.config().model.clone(),
                served_by: answer.served_by.clone(),
                cache_hit: answer.cache_hit,
                replay: answer.replay,
                attempts: answer.attempts,
            });
            if let Some(f) = &answer.fallback {
                tracing::info!(job = %job.job_id, fallback = %f, "served by a fallback model");
            }
            w.platform
                .send(&job.bot_id, &job.conversation_id, &answer.text);
        }
        Outcome::QuotaExhausted { message } => {
            // Out of money. Stop the fleet. Every further request would be refused, and
            // twenty thousand refusals per minute is a denial of service against a
            // gateway that has done nothing wrong.
            ledger.tally(|t| t.quota_exhausted += 1);
            ledger.refuse(&request_id, "quota_429");
            w.solvency.declare_broke();
            tracing::error!(job = %job.job_id, "{message}: halting the fleet");
            w.platform.send_busy(&job.bot_id, &job.conversation_id);
        }
        Outcome::Busy { .. } => {
            // `on_shed` already fired inside `complete`, once per 503 observed.
            ledger.tally(|t| t.shed_by_gateway += 1);
            ledger.refuse(&request_id, "shed_503");
            w.platform.send_busy(&job.bot_id, &job.conversation_id);
        }
        Outcome::Throttled { .. } => {
            // Our own rate limit, not the gateway's capacity. Do not shrink concurrency.
            ledger.tally(|t| t.throttled += 1);
            ledger.refuse(&request_id, "throttled_429");
            w.limiter.on_neutral();
            w.platform.send_busy(&job.bot_id, &job.conversation_id);
        }
        Outcome::DeadlineExceeded => {
            ledger.tally(|t| t.expired_before_attempted += 1);
            ledger.refuse(&request_id, "deadline");
            w.limiter.on_neutral();
            w.platform.send_busy(&job.bot_id, &job.conversation_id);
        }
        Outcome::Fatal(e) => {
            // A 400/401/409. Retrying cannot help, and a busy reply would be a lie.
            ledger.tally(|t| t.failed += 1);
            ledger.refuse(&request_id, "failed");
            tracing::error!(job = %job.job_id, error = %e, "fatal, not retryable");
        }
        Outcome::Transport(e) => {
            // Not recorded as a refusal: the request may well have reached a provider and
            // generated tokens before the socket died. A charge for it is correct, and
            // asserting otherwise would fail the audit for the gateway being right.
            ledger.tally(|t| t.failed += 1);
            w.limiter.on_neutral();
            tracing::warn!(job = %job.job_id, error = %e, "transport failure");
            w.platform.send_busy(&job.bot_id, &job.conversation_id);
        }
    }
}
