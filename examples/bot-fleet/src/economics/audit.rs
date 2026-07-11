//! Per-request audit: was every message charged correctly, and only once?
//!
//! Aggregates can agree while every individual row is wrong. Two answers overcharged by a
//! nanodollar and two undercharged by a nanodollar reconcile perfectly in total, and the
//! customer who reads their invoice is still right to complain.
//!
//! So this checks each request the bot sent, by `x-request-id`, against the row the gateway
//! wrote for it. Five questions:
//!
//! 1. **Was it recorded at all?** An answer with no row is revenue the gateway lost.
//! 2. **Was it recorded once?** Two rows for one `Idempotency-Key` means the retry bought a
//!    second completion. This is the failure that idempotency exists to prevent, it costs
//!    real money, and it is invisible in totals.
//! 3. **Was the amount right?** `cost_nano` must equal the rate card, to the nanodollar.
//! 4. **Was the reason right?** A cache hit must be `free_cache_hit`, not `free_zero_cost`;
//!    a local answer must be `charged`, not `free_error`. A zero the customer has to explain
//!    to themselves is a support ticket.
//! 5. **Was anything charged that should not have been?** A `503`, a `429`, or a request the
//!    bot abandoned never reached a provider. It must have no row, and certainly no cost.
//!
//! There is no *refund* to check, because there is nothing to reverse: switchboard only
//! bills for tokens a provider generated. What a bot calls "a refund" is really "a request
//! that must never have been charged", and questions 4 and 5 are how that is verified.

use std::collections::HashMap;

use super::ledger::{Answer, Refusal};
use super::money::{Nano, usd};
use super::prices::PriceTable;
use crate::contract::usage::UsageRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub request_id: String,
    pub problem: String,
}

/// Returns every discrepancy. An empty vector is the only acceptable result.
pub fn audit(
    answers: &[Answer],
    refusals: &[Refusal],
    history: &[UsageRecord],
    prices: &PriceTable,
) -> Vec<Finding> {
    let mut rows: HashMap<&str, Vec<&UsageRecord>> = HashMap::new();
    for r in history {
        rows.entry(r.request_id.as_str()).or_default().push(r);
    }

    let mut findings = Vec::new();

    for a in answers {
        let id = a.request_id.as_str();
        let Some(matched) = rows.get(id) else {
            findings.push(Finding {
                request_id: id.to_string(),
                problem: "the bot holds an answer the gateway never recorded — unbilled".into(),
            });
            continue;
        };

        // (2) Exactly one. A retry that reuses the Idempotency-Key must have been replayed,
        // not re-generated. Two rows means two completions were bought and one was thrown
        // away — the canonical at-least-once bug, with an LLM's price tag attached.
        if matched.len() > 1 {
            let total: Nano = matched.iter().map(|r| r.cost_nano).sum();
            findings.push(Finding {
                request_id: id.to_string(),
                problem: format!(
                    "charged {} times for one message (attempts: {}), total ${} — \
                     the Idempotency-Key did not replay",
                    matched.len(),
                    a.attempts,
                    usd(total)
                ),
            });
            continue;
        }

        let row = matched[0];

        // (2b) A replay this bot never asked for.
        //
        // The gateway replays a stored answer when an `Idempotency-Key` is reused with an
        // identical body. On attempt 1 there was nothing to retry, so the key must have
        // been minted by a *different* request. That is a collision, and the answer this
        // user just read was generated for somebody else's question.
        if a.replay && a.attempts == 1 {
            findings.push(Finding {
                request_id: id.to_string(),
                problem: "the gateway replayed a stored answer for a request that was never \
                          retried — the Idempotency-Key collided with another request, and \
                          this user was shown an answer generated for a different one"
                    .into(),
            });
        }

        // (3) The amount, to the nanodollar.
        //
        // Skipped for a replay. The row belongs to this message's *first* attempt, and this
        // bot never saw that response — it does not know whether the original was served
        // from cache, or by a fallback at a different rate. Re-deriving a price from the
        // replayed body would compare a guess against a fact and report the guess as a bug.
        // What matters about a replay is that there is exactly **one** row, and (2) above
        // already proved it.
        if a.replay {
            continue;
        }

        match prices.expected_charge(
            &a.served_by,
            a.cache_hit,
            a.prompt_tokens,
            a.completion_tokens,
        ) {
            Some(expected) if expected != row.cost_nano => findings.push(Finding {
                request_id: id.to_string(),
                problem: format!(
                    "expected ${} for {} ({}p/{}c tokens on {}), gateway charged ${}",
                    usd(expected),
                    if a.cache_hit {
                        "a cache hit"
                    } else {
                        "an answer"
                    },
                    a.prompt_tokens,
                    a.completion_tokens,
                    a.served_by,
                    usd(row.cost_nano)
                ),
            }),
            None => findings.push(Finding {
                request_id: id.to_string(),
                problem: format!(
                    "no rate-card entry for '{}' — the client cannot verify this charge",
                    a.served_by
                ),
            }),
            Some(_) => {}
        }

        // (4) The reason, in words.
        let expected_billing = if a.cache_hit {
            "free_cache_hit"
        } else if row.cost_nano > 0 {
            "charged"
        } else {
            "free_zero_cost"
        };
        if row.billing != expected_billing {
            findings.push(Finding {
                request_id: id.to_string(),
                problem: format!(
                    "billing says '{}' but the bot observed {} — a zero the customer must \
                     explain to themselves is a support ticket",
                    row.billing,
                    if a.cache_hit {
                        "a cache hit"
                    } else {
                        "a generated answer"
                    }
                ),
            });
        }

        // The model that answered must be the one the bot was told answered, or the whole
        // rate-card comparison is priced against the wrong row.
        if row.model_served != a.served_by {
            findings.push(Finding {
                request_id: id.to_string(),
                problem: format!(
                    "response.model said '{}' but the ledger says '{}'",
                    a.served_by, row.model_served
                ),
            });
        }
    }

    // (5) Nothing that never reached a provider may carry a charge.
    for r in refusals {
        if let Some(matched) = rows.get(r.request_id.as_str()) {
            let charged: Nano = matched.iter().map(|x| x.cost_nano).sum();
            if charged > 0 {
                findings.push(Finding {
                    request_id: r.request_id.clone(),
                    problem: format!(
                        "the request was refused ({}) but the gateway charged ${}",
                        r.kind,
                        usd(charged)
                    ),
                });
            }
        }
    }

    findings
}

pub fn render(findings: &[Finding], audited: usize) -> String {
    let mut s = String::new();
    s.push_str("\nper-request audit (every message the bot sent, against the gateway's row)\n");
    if findings.is_empty() {
        s.push_str(&format!(
            "  ✓ {audited} requests: each recorded exactly once, charged the rate-card amount,\n    \
             and every free answer free for a reason the customer can read.\n"
        ));
        return s;
    }
    s.push_str(&format!(
        "  ✗ {} discrepancies across {audited} requests\n",
        findings.len()
    ));
    for f in findings.iter().take(10) {
        s.push_str(&format!(
            "    {}  {}\n",
            &f.request_id[..8.min(f.request_id.len())],
            f.problem
        ));
    }
    if findings.len() > 10 {
        s.push_str(&format!("    … and {} more\n", findings.len() - 10));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replayed(id: &str, attempts: u32) -> Answer {
        Answer {
            replay: true,
            ..answer(id, "qwen3:4b", false, attempts)
        }
    }

    fn answer(id: &str, served: &str, cache_hit: bool, attempts: u32) -> Answer {
        Answer {
            replay: false,
            request_id: id.into(),
            queue_wait_ms: 1,
            gateway_ms: 10,
            end_to_end_ms: 11,
            prompt_tokens: 50,
            completion_tokens: 200,
            requested_model: "qwen3:4b".into(),
            served_by: served.into(),
            cache_hit,
            attempts,
        }
    }

    fn row(id: &str, cost: Nano, billing: &str, served: &str) -> UsageRecord {
        UsageRecord {
            user: None,
            id: "row".into(),
            request_id: id.into(),
            model_requested: "qwen3:4b".into(),
            model_served: served.into(),
            prompt_tokens: 50,
            completion_tokens: 200,
            cost_nano: cost,
            billing: billing.into(),
            status: "completed".into(),
            error_kind: None,
            latency_ms: 10,
        }
    }

    /// 50 prompt + 200 completion on qwen3:4b at 0.05/0.10 per Mtok.
    const QWEN_CHARGE: Nano = 22_500;

    #[test]
    fn a_clean_run_produces_no_findings() {
        let answers = vec![answer("r1", "qwen3:4b", false, 1)];
        let history = vec![row("r1", QWEN_CHARGE, "charged", "qwen3:4b")];
        assert!(audit(&answers, &[], &history, &PriceTable::published()).is_empty());
    }

    #[test]
    fn an_answer_with_no_row_is_unbilled_revenue() {
        let answers = vec![answer("r1", "qwen3:4b", false, 1)];
        let f = audit(&answers, &[], &[], &PriceTable::published());
        assert_eq!(f.len(), 1);
        assert!(f[0].problem.contains("never recorded"));
    }

    #[test]
    fn a_retry_charged_twice_is_the_bug_idempotency_exists_to_prevent() {
        let answers = vec![answer("r1", "qwen3:4b", false, 2)];
        let history = vec![
            row("r1", QWEN_CHARGE, "charged", "qwen3:4b"),
            row("r1", QWEN_CHARGE, "charged", "qwen3:4b"),
        ];
        let f = audit(&answers, &[], &history, &PriceTable::published());
        assert_eq!(f.len(), 1);
        assert!(f[0].problem.contains("charged 2 times"), "{}", f[0].problem);
        assert!(f[0].problem.contains("did not replay"));
    }

    #[test]
    fn an_overcharge_of_one_nanodollar_is_a_finding() {
        // Totals would hide this against a matching undercharge elsewhere. Per-request
        // does not.
        let answers = vec![answer("r1", "qwen3:4b", false, 1)];
        let history = vec![row("r1", QWEN_CHARGE + 1, "charged", "qwen3:4b")];
        let f = audit(&answers, &[], &history, &PriceTable::published());
        assert_eq!(f.len(), 1);
        assert!(f[0].problem.contains("gateway charged"));
    }

    #[test]
    fn a_cache_hit_must_be_free_and_labelled_as_such() {
        let answers = vec![answer("r1", "qwen3:4b", true, 1)];
        let ok = vec![row("r1", 0, "free_cache_hit", "qwen3:4b")];
        assert!(audit(&answers, &[], &ok, &PriceTable::published()).is_empty());

        // Free, but for the wrong stated reason: the customer sees a zero they cannot
        // explain, and cannot measure their own cache hit rate.
        let mislabelled = vec![row("r1", 0, "free_zero_cost", "qwen3:4b")];
        let f = audit(&answers, &[], &mislabelled, &PriceTable::published());
        assert_eq!(f.len(), 1);
        assert!(f[0].problem.contains("billing says"));
    }

    #[test]
    fn a_cache_hit_that_was_billed_is_a_finding() {
        let answers = vec![answer("r1", "qwen3:4b", true, 1)];
        let history = vec![row("r1", QWEN_CHARGE, "charged", "qwen3:4b")];
        let f = audit(&answers, &[], &history, &PriceTable::published());
        assert!(!f.is_empty());
        assert!(f.iter().any(|x| x.problem.contains("expected $0")));
    }

    #[test]
    fn a_refused_request_that_was_charged_is_a_finding() {
        // A 503 never reached a provider. Charging for it is theft, not a rounding error.
        let refusals = vec![Refusal {
            request_id: "r9".into(),
            kind: "shed_503",
        }];
        let history = vec![row("r9", QWEN_CHARGE, "charged", "qwen3:4b")];
        let f = audit(&[], &refusals, &history, &PriceTable::published());
        assert_eq!(f.len(), 1);
        assert!(f[0].problem.contains("refused"));
    }

    #[test]
    fn a_refused_request_with_no_row_is_correct() {
        let refusals = vec![Refusal {
            request_id: "r9".into(),
            kind: "shed_503",
        }];
        assert!(audit(&[], &refusals, &[], &PriceTable::published()).is_empty());
    }

    #[test]
    fn a_ledger_that_disagrees_about_which_model_answered_is_a_finding() {
        let answers = vec![answer("r1", "qwen3:4b", false, 1)];
        let history = vec![row("r1", QWEN_CHARGE, "charged", "gpt-4o-mini")];
        let f = audit(&answers, &[], &history, &PriceTable::published());
        assert!(f.iter().any(|x| x.problem.contains("response.model said")));
    }

    #[test]
    fn an_unpriced_model_cannot_be_verified_and_says_so() {
        let answers = vec![answer("r1", "some-new-model", false, 1)];
        let history = vec![row("r1", 5, "charged", "some-new-model")];
        let f = audit(&answers, &[], &history, &PriceTable::published());
        assert!(f.iter().any(|x| x.problem.contains("no rate-card entry")));
    }

    /// A retry that was replayed is the `Idempotency-Key` working. One row, one charge, one
    /// answer — and no finding.
    #[test]
    fn a_replay_after_a_retry_is_the_system_behaving() {
        let answers = vec![replayed("r1", 2)];
        let history = vec![row("r1", 7650, "charged", "qwen3:4b")];
        let findings = audit(&answers, &[], &history, &PriceTable::published());
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// A replay with no retry behind it means somebody else's key matched ours. The user
    /// just read an answer generated for a different question, and no amount of correct
    /// billing makes that acceptable.
    #[test]
    fn a_replay_without_a_retry_is_a_key_collision() {
        let answers = vec![replayed("r1", 1)];
        let history = vec![row("r1", 7650, "charged", "qwen3:4b")];
        let findings = audit(&answers, &[], &history, &PriceTable::published());
        assert_eq!(findings.len(), 1);
        assert!(findings[0].problem.contains("collided"), "{findings:?}");
    }

    /// The amount check is skipped for a replay, because the row describes an attempt this
    /// bot never saw. Re-deriving a price from the replayed body and reporting the mismatch
    /// would be reporting our own ignorance as the gateway's bug.
    #[test]
    fn a_replay_of_a_cache_hit_is_not_a_billing_finding() {
        // The original attempt was served from cache and charged zero. The replay's body
        // carries real token counts, so a naive price check would expect 7650 and find 0.
        let answers = vec![replayed("r1", 2)];
        let history = vec![row("r1", 0, "free_cache_hit", "qwen3:4b")];
        let findings = audit(&answers, &[], &history, &PriceTable::published());
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The one thing a replay must still prove: it was charged exactly once. Two rows means
    /// the key did not replay and two completions were bought.
    #[test]
    fn a_replay_charged_twice_is_still_the_at_least_once_bug() {
        let answers = vec![replayed("r1", 2)];
        let history = vec![
            row("r1", 7650, "charged", "qwen3:4b"),
            row("r1", 7650, "charged", "qwen3:4b"),
        ];
        let findings = audit(&answers, &[], &history, &PriceTable::published());
        assert_eq!(findings.len(), 1);
        assert!(findings[0].problem.contains("did not replay"), "{findings:?}");
    }
}
