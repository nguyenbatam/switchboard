//! The verdict: was it worth routing this workload through switchboard?
//!
//! The report is arranged so that it can say **no**. A benchmark that cannot lose is a
//! brochure. `docs/08 §4` draws the distinction this file tries to honour: *"34% hit rate,
//! 0.4% false-hit rate, p95 1.2s → 380ms" is a claim. "Has a semantic cache" is not.*
//!
//! Four questions, checked in this order, because a later answer is worthless if an
//! earlier one is bad:
//!
//! 1. **Do the books agree?** If the client's arithmetic and the gateway's ledger disagree,
//!    every number below is suspect and nothing else may be concluded.
//! 2. **Was there enough capacity?** A workload that mostly got refused has not been
//!    priced; it has been rationed.
//! 3. **Did it save money** against calling the provider directly?
//! 4. **What did the saving cost** in latency?

use std::collections::BTreeMap;

use super::ledger::{Answer, Tallies, percentile_sorted};
use super::money::{NANO_PER_USD, Nano, basis_points, pct, usd_short};
use super::prices::PriceTable;
use crate::contract::usage::{UsageRecord, UsageSnapshot};

/// Refusals above this share of offered load mean the run measured capacity, not price.
const CAPACITY_SHORT_BP: i64 = 2_000; // 20%

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The client's books and the gateway's ledger disagree. Stop.
    BillingDisagreement {
        delta_nano: Nano,
    },
    /// Most of the offered load never became an answer.
    CapacityShort {
        refused_bp: i64,
    },
    /// It ran, it was billed correctly, and it saved nothing.
    NotWorthIt {
        reason: String,
    },
    WorthIt {
        saved_nano: Nano,
        saved_bp: i64,
    },
}

#[derive(Debug, Clone)]
pub struct ModelLine {
    pub model: String,
    pub answers: u64,
    pub cost_nano: Nano,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

pub struct Report {
    /// The ceiling the gateway enforces. `None` means unlimited — not zero.
    pub funded_nano: Option<Nano>,
    /// This run's spend: the gateway's all-time total, after minus before.
    pub spent_nano: Nano,
    /// Read from the gateway, not derived. The tenant may have spent money before this
    /// run, and a report that assumes otherwise will overstate what is left.
    pub remaining_nano: Option<Nano>,

    pub answers: u64,
    pub cache_hits: u64,
    pub tallies: Tallies,

    pub prompt_tokens: u64,
    pub completion_tokens: u64,

    /// What the client's own arithmetic says the run cost.
    pub client_expected_nano: Nano,
    /// `spent - client_expected`. Must be zero.
    pub reconciliation_delta_nano: Nano,

    /// The same tokens, every request, priced at the provider's list price with no cache.
    pub baseline_direct_nano: Nano,
    pub saved_nano: Nano,
    pub saved_bp: i64,

    pub e2e_p50: u64,
    pub e2e_p95: u64,
    pub e2e_p99: u64,
    pub gateway_p50: u64,
    pub gateway_p95: u64,
    pub gateway_p99: u64,
    pub queue_p95: u64,

    pub by_model: Vec<ModelLine>,

    /// How long the run took, wall clock.
    pub duration_secs: f64,
    /// The most requests this fleet ever had in flight against the gateway at once. The
    /// answer to "how many replies can the bot handle concurrently".
    pub peak_inflight: usize,
    /// Answers per second, sustained over the run.
    pub replies_per_sec: f64,

    pub verdict: Verdict,
}

impl Report {
    pub fn build(
        before: UsageSnapshot,
        after: UsageSnapshot,
        answers: &[Answer],
        tallies: Tallies,
        prices: &PriceTable,
        duration_secs: f64,
        peak_inflight: usize,
    ) -> Self {
        let spent_nano = after.total_cost_nano - before.total_cost_nano;
        let funded_nano = after.monthly_budget_nano;
        let remaining_nano = after.remaining_nano;

        // What the client believes it spent: each answer at the price of the model that
        // actually answered, cache hits at zero. This is only computable because the
        // gateway tells the truth in `response.model` and `x-switchboard-cache`.
        let client_expected_nano: Nano = answers
            .iter()
            .filter(|a| !a.cache_hit)
            .map(|a| {
                prices
                    .get(&a.served_by)
                    .map(|p| p.cost_nano(a.prompt_tokens, a.completion_tokens))
                    .unwrap_or(0)
            })
            .sum();

        // What it would have cost with no gateway: no cache, no local model, every request
        // to the paid provider — at the provider's OWN list price, not switchboard's
        // resale rate. Comparing switchboard against switchboard would assume the
        // conclusion.
        let direct_table = PriceTable::provider_direct();
        let direct = direct_table
            .get(direct_table.direct_baseline_model())
            .expect("the baseline model must be priced");
        let baseline_direct_nano: Nano = answers
            .iter()
            .map(|a| direct.cost_nano(a.prompt_tokens, a.completion_tokens))
            .sum();

        let saved_nano = baseline_direct_nano - spent_nano;
        let saved_bp = basis_points(saved_nano, baseline_direct_nano);
        let reconciliation_delta_nano = spent_nano - client_expected_nano;

        let mut e2e: Vec<u64> = answers.iter().map(|a| a.end_to_end_ms).collect();
        let mut gw: Vec<u64> = answers.iter().map(|a| a.gateway_ms).collect();
        let mut qw: Vec<u64> = answers.iter().map(|a| a.queue_wait_ms).collect();
        e2e.sort_unstable();
        gw.sort_unstable();
        qw.sort_unstable();

        let by_model = Self::group_by_model(answers, prices);

        let refused = tallies.broker_full
            + tallies.expired_before_attempted
            + tallies.shed_by_gateway
            + tallies.quota_exhausted;
        let refused_bp = basis_points(refused as i64, tallies.offered as i64);

        let verdict = if reconciliation_delta_nano != 0 {
            Verdict::BillingDisagreement {
                delta_nano: reconciliation_delta_nano,
            }
        } else if refused_bp > CAPACITY_SHORT_BP {
            Verdict::CapacityShort { refused_bp }
        } else if answers.is_empty() {
            Verdict::NotWorthIt {
                reason: "no answers were produced; there is nothing to price".into(),
            }
        } else if saved_nano <= 0 {
            Verdict::NotWorthIt {
                reason: if baseline_direct_nano == 0 {
                    "the answers were served by a model with no direct-provider equivalent, \
                     so there is no baseline to save against"
                        .into()
                } else if saved_nano < 0 {
                    format!(
                        "this workload cost ${} MORE than calling the provider directly: it was \
                         served by a resold hosted model, and switchboard's markup is not free. \
                         You saved nothing and paid a hop for it",
                        usd_short(-saved_nano)
                    )
                } else {
                    "every answer was served by the paid model, with no cache hits: you paid \
                     the gateway's overhead and saved nothing"
                        .into()
                },
            }
        } else {
            Verdict::WorthIt {
                saved_nano,
                saved_bp,
            }
        };

        Self {
            funded_nano,
            spent_nano,
            remaining_nano,
            answers: answers.len() as u64,
            cache_hits: answers.iter().filter(|a| a.cache_hit).count() as u64,
            tallies,
            prompt_tokens: answers.iter().map(|a| a.prompt_tokens).sum(),
            completion_tokens: answers.iter().map(|a| a.completion_tokens).sum(),
            client_expected_nano,
            reconciliation_delta_nano,
            baseline_direct_nano,
            saved_nano,
            saved_bp,
            e2e_p50: percentile_sorted(&e2e, 5_000),
            e2e_p95: percentile_sorted(&e2e, 9_500),
            e2e_p99: percentile_sorted(&e2e, 9_900),
            gateway_p50: percentile_sorted(&gw, 5_000),
            gateway_p95: percentile_sorted(&gw, 9_500),
            gateway_p99: percentile_sorted(&gw, 9_900),
            queue_p95: percentile_sorted(&qw, 9_500),
            by_model,
            duration_secs,
            peak_inflight,
            replies_per_sec: if duration_secs > 0.0 {
                answers.len() as f64 / duration_secs
            } else {
                0.0
            },
            verdict,
        }
    }

    fn group_by_model(answers: &[Answer], prices: &PriceTable) -> Vec<ModelLine> {
        let mut buckets: BTreeMap<String, Vec<&Answer>> = BTreeMap::new();
        for a in answers {
            let key = if a.cache_hit {
                format!("{} (cached)", a.served_by)
            } else {
                a.served_by.clone()
            };
            buckets.entry(key).or_default().push(a);
        }
        buckets
            .into_iter()
            .map(|(model, group)| {
                let mut lat: Vec<u64> = group.iter().map(|a| a.gateway_ms).collect();
                lat.sort_unstable();
                let cost_nano = group
                    .iter()
                    .filter(|a| !a.cache_hit)
                    .map(|a| {
                        prices
                            .get(&a.served_by)
                            .map(|p| p.cost_nano(a.prompt_tokens, a.completion_tokens))
                            .unwrap_or(0)
                    })
                    .sum();
                ModelLine {
                    model,
                    answers: group.len() as u64,
                    cost_nano,
                    p50_ms: percentile_sorted(&lat, 5_000),
                    p95_ms: percentile_sorted(&lat, 9_500),
                }
            })
            .collect()
    }

    /// Answers per dollar. `None` when the run cost nothing — which is a real outcome, not
    /// a division by zero to be papered over with a big number.
    pub fn answers_per_usd(&self) -> Option<u64> {
        (self.spent_nano > 0)
            .then(|| (self.answers as i128 * NANO_PER_USD as i128 / self.spent_nano as i128) as u64)
    }

    pub fn cost_per_answer_nano(&self) -> Nano {
        if self.answers == 0 {
            0
        } else {
            self.spent_nano / self.answers as i64
        }
    }

    pub fn render(&self) -> String {
        let t = &self.tallies;
        let mut s = String::new();
        s.push_str("\n════════ third-party report: was switchboard worth it? ════════\n\n");

        s.push_str("money (the gateway's ledger is the authority)\n");
        match (self.funded_nano, self.remaining_nano) {
            (Some(funded), Some(remaining)) => s.push_str(&format!(
                "  funded            ${}\n  spent (this run)  ${}\n  remaining         ${}{}\n",
                usd_short(funded),
                usd_short(self.spent_nano),
                usd_short(remaining),
                if remaining < 0 {
                    "   (overshot: the budget check is a pre-check)"
                } else {
                    ""
                },
            )),
            _ => s.push_str(&format!(
                "  funded            unlimited (no monthly_budget_nano set)\n  \
                 spent (this run)  ${}\n",
                usd_short(self.spent_nano),
            )),
        }
        s.push_str(&format!(
            "  answers           {}\n  cost per answer   ${}\n",
            self.answers,
            usd_short(self.cost_per_answer_nano()),
        ));
        match self.answers_per_usd() {
            Some(n) => s.push_str(&format!("  answers per $1    {n}\n")),
            None => s.push_str("  answers per $1    ∞ (the run cost nothing)\n"),
        }

        s.push_str("\nreconciliation (client's books vs the gateway's)\n");
        s.push_str(&format!(
            "  client expected   ${}\n  gateway billed    ${}\n  delta             ${}{}\n",
            usd_short(self.client_expected_nano),
            usd_short(self.spent_nano),
            usd_short(self.reconciliation_delta_nano),
            if self.reconciliation_delta_nano == 0 {
                "   ✓ the books agree"
            } else {
                "   ✗ SOMEONE HAS A BILLING BUG"
            },
        ));

        s.push_str("\nvs. calling the provider directly (no gateway, no cache, no local model)\n");
        s.push_str(&format!(
            "  direct would cost ${}\n  actually cost     ${}\n  saved             ${}  ({})\n",
            usd_short(self.baseline_direct_nano),
            usd_short(self.spent_nano),
            usd_short(self.saved_nano),
            pct(self.saved_bp),
        ));

        s.push_str("\nwhat the saving cost, in latency\n");
        s.push_str("  model                        answers      cost       p50      p95\n");
        for m in &self.by_model {
            s.push_str(&format!(
                "  {:<26} {:>7}  {:>8}  {:>6}ms  {:>6}ms\n",
                m.model,
                m.answers,
                format!("${}", usd_short(m.cost_nano)),
                m.p50_ms,
                m.p95_ms,
            ));
        }

        s.push_str("\nthroughput and concurrency (the bot's side)\n");
        s.push_str(&format!(
            "  run duration      {:.1}s\n  replies delivered {} ({:.2}/s sustained)\n               peak concurrent   {} in flight against the gateway at once\n",
            self.duration_secs, self.answers, self.replies_per_sec, self.peak_inflight,
        ));
        s.push_str(
            "  Little's Law: concurrency = throughput x latency. If peak concurrent is far\n               below your AIMD ceiling, the bottleneck is upstream, not your pool size.\n",
        );

        s.push_str("\nlatency (what the user felt, not what the gateway measured)\n");
        s.push_str(&format!(
            "  end-to-end        p50 {:>6}ms   p95 {:>6}ms   p99 {:>6}ms\n",
            self.e2e_p50, self.e2e_p95, self.e2e_p99
        ));
        s.push_str(&format!(
            "  gateway call      p50 {:>6}ms   p95 {:>6}ms   p99 {:>6}ms\n",
            self.gateway_p50, self.gateway_p95, self.gateway_p99
        ));
        s.push_str(&format!(
            "  your own backlog                    p95 {:>6}ms   (not the gateway's fault)\n",
            self.queue_p95
        ));

        s.push_str("\nwhat did not become an answer\n");
        s.push_str(&format!(
            "  offered {}   coalesced {}   broker full {}   expired {}   shed(503) {}   throttled(429) {}   quota(429) {}   failed {}\n",
            t.offered, t.coalesced, t.broker_full, t.expired_before_attempted,
            t.shed_by_gateway, t.throttled, t.quota_exhausted, t.failed,
        ));
        if self.cache_hits > 0 {
            s.push_str(&format!(
                "  cache hits        {} of {} answers ({})\n",
                self.cache_hits,
                self.answers,
                pct(basis_points(self.cache_hits as i64, self.answers as i64))
            ));
        }

        s.push_str("\nverdict\n");
        s.push_str(&self.verdict_text());
        s.push('\n');
        s
    }

    fn verdict_text(&self) -> String {
        match &self.verdict {
            Verdict::BillingDisagreement { delta_nano } => format!(
                "  ✗ INCONCLUSIVE — the books disagree by ${}.\n    \
                 Nothing above can be trusted until this is zero. Either the client's price\n    \
                 table is stale, or the gateway billed for something it did not serve.",
                usd_short(*delta_nano)
            ),
            Verdict::CapacityShort { refused_bp } => format!(
                "  ✗ INCONCLUSIVE — {} of offered load never became an answer.\n    \
                 This run measured capacity, not price. Per docs/14 §2, no architecture\n    \
                 fixes a capacity shortfall: raise the provider limit, do less work per\n    \
                 request, serve from cache, or accept less. Re-run inside capacity, then\n    \
                 ask about money.",
                pct(*refused_bp)
            ),
            Verdict::NotWorthIt { reason } => format!(
                "  ✗ GO DIRECT — {reason}.\n    \
                 Switchboard adds a hop, a queue, and an argon2 verify for no saving on this\n    \
                 workload. Keep it only for what it does besides price: fallback, budgets,\n    \
                 one wire format, and one place where usage is recorded."
            ),
            Verdict::WorthIt {
                saved_nano,
                saved_bp,
            } => {
                let mut s = format!(
                    "  ✓ WORTH IT — ${} saved ({} of the direct bill) across {} answers.\n",
                    usd_short(*saved_nano),
                    pct(*saved_bp),
                    self.answers
                );
                s.push_str(
                    "    The saving is real, and it is not free. Read the per-model table:\n    \
                     the cheap answers are the slow ones. Decide with docs/15 §1 — a bot has\n    \
                     a 25s budget and a person watching a cursor does not.\n",
                );
                s.push_str(
                    "    Caveat this report cannot resolve: the local model is priced at zero\n    \
                     because the gateway prices it at zero. Electricity, GPUs and the pager\n    \
                     are real, and they are not in this table. It also says nothing about\n    \
                     answer quality — that is what docs/09's eval suite is for.",
                );
                s
            }
        }
    }
}

/// The gateway's own per-call history: which request was charged, and which was not.
///
/// `/v1/usage` says what the bill is. Only this says *why* — and it is the difference
/// between a customer who trusts an invoice and a customer who opens a support ticket.
///
/// It is also a second, independent reconciliation. The rows come from `usage_records`,
/// the fact table; `/v1/usage` is summed from `usage_hourly`, the rollup. They are written
/// in one transaction, so they must agree. If they ever do not, the rollup has drifted and
/// every aggregate the gateway has ever reported is suspect.
pub fn render_history(records: &[UsageRecord]) -> String {
    let mut charged = 0u64;
    let mut free_cache = 0u64;
    let mut free_error = 0u64;
    let mut free_zero = 0u64;
    let mut disconnects = 0u64;
    let mut total_nano: Nano = 0;

    for r in records {
        total_nano += r.cost_nano;
        match r.billing.as_str() {
            "charged" => charged += 1,
            "free_cache_hit" => free_cache += 1,
            "free_error" => free_error += 1,
            _ => free_zero += 1,
        }
        if r.status == "client_disconnect" {
            disconnects += 1;
        }
    }

    let mut s = String::new();
    s.push_str("\ncall history (GET /v1/usage/records — the gateway's own books)\n");
    s.push_str(&format!(
        "  {} rows   charged {}   free: cache {} · error {} · zero-cost {}\n",
        records.len(),
        charged,
        free_cache,
        free_error,
        free_zero
    ));
    s.push_str(&format!(
        "  sum of cost_nano across every row: ${}\n",
        usd_short(total_nano)
    ));
    if disconnects > 0 {
        s.push_str(&format!(
            "  {disconnects} client_disconnect rows — billed for tokens generated before the socket died\n"
        ));
    }

    s.push_str("\n  most recent calls\n");
    s.push_str("    status              billing          served_by        cost      latency\n");
    for r in records.iter().take(6) {
        s.push_str(&format!(
            "    {:<18}  {:<15}  {:<15}  {:>8}  {:>6}ms\n",
            r.error_kind.as_deref().unwrap_or(&r.status),
            r.billing,
            r.model_served,
            format!("${}", usd_short(r.cost_nano)),
            r.latency_ms,
        ));
    }
    s
}

/// The fact table must sum to the rollup. Returns the discrepancy, which must be zero.
pub fn fact_vs_rollup_delta(records: &[UsageRecord], rollup: &UsageSnapshot) -> Nano {
    let fact: Nano = records.iter().map(|r| r.cost_nano).sum();
    fact - rollup.total_cost_nano
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(total_cost_nano: i64, budget: Option<i64>) -> UsageSnapshot {
        UsageSnapshot {
            total_requests: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_cost_nano,
            monthly_budget_nano: budget,
            month_spend_nano: total_cost_nano,
            remaining_nano: budget.map(|b| b - total_cost_nano),
        }
    }

    fn build(
        before: UsageSnapshot,
        after: UsageSnapshot,
        answers: &[Answer],
        t: Tallies,
    ) -> Report {
        Report::build(before, after, answers, t, &PriceTable::published(), 10.0, 4)
    }

    fn answer(served_by: &str, cache_hit: bool) -> Answer {
        Answer {
            replay: false,
            request_id: "r".into(),
            queue_wait_ms: 10,
            gateway_ms: 100,
            end_to_end_ms: 110,
            prompt_tokens: 50,
            completion_tokens: 200,
            requested_model: "qwen3:4b".into(),
            served_by: served_by.into(),
            cache_hit,
            attempts: 1,
        }
    }

    fn offered(n: u64) -> Tallies {
        Tallies {
            offered: n,
            ..Default::default()
        }
    }

    /// 50 prompt + 200 completion tokens.
    /// Switchboard's rate card for the local model: 0.05/M in, 0.10/M out.
    const LOCAL_PER_ANSWER: Nano = 22_500;
    /// Switchboard's resale rate for the hosted model: 0.20/M in, 0.80/M out.
    const HOSTED_PER_ANSWER: Nano = 170_000;
    /// What OpenAI would have charged directly: 0.15/M in, 0.60/M out.
    const DIRECT_PER_ANSWER: Nano = 127_500;

    #[test]
    fn local_capacity_undercuts_the_provider_and_the_verdict_says_so() {
        let answers: Vec<_> = (0..10).map(|_| answer("qwen3:4b", false)).collect();
        let spent = LOCAL_PER_ANSWER * 10;
        let r = build(
            snapshot(0, Some(1_000_000)),
            snapshot(spent, Some(1_000_000)),
            &answers,
            offered(10),
        );

        assert_eq!(r.reconciliation_delta_nano, 0, "the books must agree");
        assert_eq!(r.baseline_direct_nano, DIRECT_PER_ANSWER * 10);
        assert_eq!(r.saved_nano, DIRECT_PER_ANSWER * 10 - spent);
        assert!(matches!(r.verdict, Verdict::WorthIt { .. }));
        // $0.0225 through the gateway against $0.1275 direct: 82.35% cheaper. Note this is
        // the *customer's* saving, not switchboard's margin — the two are different numbers
        // and only `sbctl stats` knows the second.
        assert_eq!(
            r.saved_bp, 8_235,
            "82.35% cheaper than calling OpenAI directly"
        );
    }

    #[test]
    fn reselling_the_hosted_model_costs_more_than_going_direct_and_it_says_so() {
        // Everything served by gpt-4o-mini, which switchboard resells at a markup. The
        // gateway added a hop, a queue and an argon2 verify, and charged more than OpenAI
        // would have. The report must be able to reach that conclusion about its own
        // product, or it is a brochure.
        let answers: Vec<_> = (0..10).map(|_| answer("gpt-4o-mini", false)).collect();
        let spent = HOSTED_PER_ANSWER * 10;
        let r = build(
            snapshot(0, None),
            snapshot(spent, None),
            &answers,
            offered(10),
        );

        assert_eq!(r.reconciliation_delta_nano, 0, "the books still agree");
        assert_eq!(r.baseline_direct_nano, DIRECT_PER_ANSWER * 10);
        assert_eq!(r.saved_nano, DIRECT_PER_ANSWER * 10 - spent);
        assert!(
            r.saved_nano < 0,
            "the markup must surface as a negative saving"
        );
        match r.verdict {
            Verdict::NotWorthIt { ref reason } => {
                assert!(
                    reason.contains("MORE than calling the provider directly"),
                    "{reason}"
                )
            }
            ref v => panic!("expected NotWorthIt, got {v:?}"),
        }
    }

    #[test]
    fn cache_hits_are_free_to_the_client_and_free_on_the_gateways_ledger() {
        // Five real answers, five replayed from cache. The gateway bills five.
        let mut answers: Vec<_> = (0..5).map(|_| answer("qwen3:4b", false)).collect();
        answers.extend((0..5).map(|_| answer("qwen3:4b", true)));
        let spent = LOCAL_PER_ANSWER * 5;

        let r = build(
            snapshot(0, None),
            snapshot(spent, None),
            &answers,
            offered(10),
        );

        assert_eq!(r.cache_hits, 5);
        assert_eq!(
            r.reconciliation_delta_nano,
            0,
            "a client that billed itself for cache hits would drift by {}",
            LOCAL_PER_ANSWER * 5
        );
        // The baseline still charges for all ten: going direct means no cache.
        assert_eq!(r.baseline_direct_nano, DIRECT_PER_ANSWER * 10);
        assert!(matches!(r.verdict, Verdict::WorthIt { .. }));
    }

    #[test]
    fn a_client_that_cannot_see_cache_hits_reports_a_billing_disagreement() {
        // This is what the report looked like before the gateway sent
        // `x-switchboard-cache`. The client prices the replayed tokens, the gateway does
        // not, and the delta is exactly the cache hits. The verdict refuses to conclude.
        let answers: Vec<_> = (0..5)
            .map(|_| answer("qwen3:4b", false)) // cache hits mislabelled as misses
            .collect();
        let spent = 0; // the gateway charged nothing: they were all hits

        let r = build(
            snapshot(0, None),
            snapshot(spent, None),
            &answers,
            offered(5),
        );

        assert_eq!(r.reconciliation_delta_nano, -LOCAL_PER_ANSWER * 5);
        assert!(matches!(r.verdict, Verdict::BillingDisagreement { .. }));
    }

    #[test]
    fn a_run_that_was_mostly_refused_measured_capacity_not_price() {
        let answers = vec![answer("qwen3:4b", false)];
        let tallies = Tallies {
            offered: 100,
            shed_by_gateway: 30,
            expired_before_attempted: 10,
            ..Default::default()
        };
        let r = build(
            snapshot(0, None),
            snapshot(LOCAL_PER_ANSWER, None),
            &answers,
            tallies,
        );
        match r.verdict {
            Verdict::CapacityShort { refused_bp } => assert_eq!(refused_bp, 4_000),
            ref v => panic!("expected CapacityShort, got {v:?}"),
        }
    }

    #[test]
    fn a_disagreement_outranks_a_capacity_shortfall() {
        // Both conditions hold. The books being wrong must win, because a capacity claim
        // computed from numbers that do not add up is not a claim.
        let answers = vec![answer("qwen3:4b", false)];
        let tallies = Tallies {
            offered: 100,
            shed_by_gateway: 90,
            ..Default::default()
        };
        let r = build(
            snapshot(0, None),
            snapshot(999_999, None), // gateway billed something the client cannot explain
            &answers,
            tallies,
        );
        assert!(matches!(r.verdict, Verdict::BillingDisagreement { .. }));
    }

    #[test]
    fn remaining_comes_from_the_gateway_and_not_from_subtraction() {
        // The tenant spent money before this run. A report that computed
        // `funded - spent_this_run` would overstate the balance by the prior spend.
        let answers = vec![answer("qwen3:4b", false)];
        let before = snapshot(500_000, Some(1_000_000));
        let after = snapshot(511_000, Some(1_000_000));

        let r = build(before, after, &answers, offered(1));

        assert_eq!(r.spent_nano, 11_000, "this run's spend is a delta");
        assert_eq!(
            r.remaining_nano,
            Some(489_000),
            "remaining must account for the prior 500_000 of spend"
        );
    }

    #[test]
    fn answers_per_usd_is_none_rather_than_infinite_when_the_run_was_free() {
        let answers = vec![answer("qwen3:4b", false)];
        let r = build(snapshot(0, None), snapshot(0, None), &answers, offered(1));
        assert_eq!(r.answers_per_usd(), None);
        assert_eq!(r.cost_per_answer_nano(), 0);
    }

    #[test]
    fn no_answers_means_nothing_to_price() {
        let r = build(snapshot(0, None), snapshot(0, None), &[], offered(0));
        assert!(matches!(r.verdict, Verdict::NotWorthIt { .. }));
        assert_eq!(r.answers, 0);
    }

    #[test]
    fn the_report_renders_without_panicking_in_every_verdict() {
        for (spent, tallies, answers) in [
            (0i64, offered(1), vec![answer("qwen3:4b", false)]),
            (
                HOSTED_PER_ANSWER,
                offered(1),
                vec![answer("gpt-4o-mini", false)],
            ),
            (999i64, offered(1), vec![answer("qwen3:4b", false)]),
            (0, offered(0), vec![]),
        ] {
            let r = build(
                snapshot(0, Some(1_000_000)),
                snapshot(spent, Some(1_000_000)),
                &answers,
                tallies,
            );
            assert!(!r.render().is_empty());
        }
    }
}
