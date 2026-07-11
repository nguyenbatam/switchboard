//! What the client *believes* things cost.
//!
//! This table is the third party's own, built from published list prices. It is not read
//! from the gateway, and that is the point: the whole question this crate exists to answer
//! is *"would I be better off calling OpenAI directly?"*, and answering it with the
//! gateway's own numbers would be assuming the conclusion.
//!
//! Two different sums are computed from this table, and they must not be confused:
//!
//! * **What the run actually cost** — each answer priced at the model that *actually*
//!   answered (`response.model`), with cache hits at zero. This should reconcile exactly
//!   against the gateway's `/v1/usage`. If it does not, one of us has a billing bug.
//! * **What it would have cost direct** — every answer priced at the model you *asked
//!   for*, with no cache, because going direct means no cache and no local model. This is
//!   the baseline the verdict is measured against.
//!
//! The difference between those two numbers is switchboard's entire commercial claim.

use std::collections::HashMap;

use super::money::Nano;

/// USD per million tokens, held as micros so the arithmetic stays in integers.
///
/// `0.15 USD/Mtok` is stored as `150_000` micros/Mtok. The gateway's own
/// `config/pricing.rs` does exactly this, for exactly this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    input_micros_per_mtok: i64,
    output_micros_per_mtok: i64,
}

impl Price {
    /// Takes the published decimal as a string, so no `f64` ever touches a price.
    pub fn per_mtok(input_usd: &str, output_usd: &str) -> Self {
        Self {
            input_micros_per_mtok: usd_to_micros(input_usd),
            output_micros_per_mtok: usd_to_micros(output_usd),
        }
    }

    pub const FREE: Price = Price {
        input_micros_per_mtok: 0,
        output_micros_per_mtok: 0,
    };

    pub fn is_free(&self) -> bool {
        self.input_micros_per_mtok == 0 && self.output_micros_per_mtok == 0
    }

    /// `tokens × micros_per_Mtok × 1e9 nano/USD ÷ (1e6 micros/USD × 1e6 tokens/Mtok)`.
    ///
    /// Widened to `i128` because the numerator overflows `i64` for realistic token counts.
    pub fn cost_nano(&self, prompt_tokens: u64, completion_tokens: u64) -> Nano {
        let f = |tokens: u64, micros: i64| -> i128 {
            tokens as i128 * micros as i128 * 1_000_000_000 / (1_000_000i128 * 1_000_000)
        };
        (f(prompt_tokens, self.input_micros_per_mtok)
            + f(completion_tokens, self.output_micros_per_mtok)) as Nano
    }
}

/// Exact decimal USD → micros. Panics on malformed input, because a price table is a
/// constant in the source, not user input.
fn usd_to_micros(s: &str) -> i64 {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    assert!(frac.len() <= 6, "{s}: more precision than a micro-dollar");
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().expect("price whole part must be digits")
    };
    let padded = format!("{frac:0<6}");
    let frac: i64 = padded.parse().expect("price fraction must be digits");
    whole * 1_000_000 + frac
}

#[derive(Debug, Clone)]
pub struct PriceTable {
    prices: HashMap<String, Price>,
    /// What a model costs when we have never heard of it. Zero would silently under-report
    /// spend and make switchboard look free; instead we refuse to guess.
    default: Option<Price>,
}

impl PriceTable {
    /// The rate card, as published — by OpenAI for its models, and by switchboard for the
    /// local capacity it resells.
    ///
    /// This is a *client-side copy* of somebody else's price list, which means it goes
    /// stale. That is not a flaw to be engineered away; it is the reason the report
    /// reconciles against `/v1/usage` instead of trusting this table. A drift between the
    /// two is either a stale copy here or a billing bug there, and both are worth an alert.
    pub fn published() -> Self {
        let mut prices = HashMap::new();
        // Switchboard's rate card. Every request is billed at the rate of the model that
        // actually answered. None of these is zero: a customer who pays nothing is not a
        // customer, and a saving measured against zero is not a measurement.
        prices.insert("qwen3:4b".to_string(), Price::per_mtok("0.05", "0.10"));
        prices.insert("gpt-4o-mini".to_string(), Price::per_mtok("0.20", "0.80"));
        prices.insert("nomic-embed-text".to_string(), Price::per_mtok("0.02", "0"));
        Self {
            prices,
            default: None,
        }
    }

    /// What the provider would charge us directly, with no gateway in between.
    ///
    /// This is **not** switchboard's rate card. Switchboard resells `gpt-4o-mini` at a
    /// markup; going direct is cheaper for that one model, and the report has to be able
    /// to say so. The gateway earns its keep on the local model, the fallback, and one
    /// wire format — not by undercutting OpenAI on OpenAI's own inference.
    pub fn provider_direct() -> Self {
        let mut prices = HashMap::new();
        prices.insert("gpt-4o-mini".to_string(), Price::per_mtok("0.15", "0.60"));
        prices.insert("gpt-4o".to_string(), Price::per_mtok("2.50", "10.00"));
        Self {
            prices,
            default: None,
        }
    }

    pub fn with_price(mut self, model: &str, price: Price) -> Self {
        self.prices.insert(model.to_string(), price);
        self
    }

    pub fn get(&self, model: &str) -> Option<Price> {
        self.prices.get(model).copied().or(self.default)
    }

    /// The model a direct-to-provider caller would have had to use. There is no local
    /// model and no cache on that path — that is what "direct" means.
    pub fn direct_baseline_model(&self) -> &'static str {
        "gpt-4o-mini"
    }

    /// Nanodollars per single token, the unit `/v1/models` publishes.
    ///
    /// `micros_per_Mtok × 1e9 nano/USD ÷ (1e6 micros/USD × 1e6 tokens/Mtok)` = `÷ 1000`.
    fn nano_per_token(&self, model: &str) -> Option<(i64, i64)> {
        let p = self.get(model)?;
        Some((
            p.input_micros_per_mtok / 1_000,
            p.output_micros_per_mtok / 1_000,
        ))
    }

    /// Every model where this table and the gateway's published rate card disagree.
    ///
    /// A client-side price table goes stale, silently, and the first symptom is a
    /// reconciliation failure at the end of a run that has already spent the money. This
    /// check costs one `GET /v1/models` and runs before the first user is kept waiting.
    ///
    /// It compares only what the gateway is authoritative about — its own rate card. The
    /// direct-to-provider baseline stays this client's own belief, because a gateway
    /// supplying the number it is being measured against would be marking its own homework.
    ///
    /// Models the gateway prices and we do not are reported too: the run cannot verify a
    /// charge for a model it has never heard of.
    pub fn drift_against(&self, cards: &[crate::contract::models::ModelCard]) -> Vec<Drift> {
        let mut out = Vec::new();
        for card in cards {
            let Some(published) = card.pricing else { continue };
            let published = (
                published.prompt_nano_per_token,
                published.completion_nano_per_token,
            );
            match self.nano_per_token(&card.id) {
                Some(believed) if believed != published => out.push(Drift {
                    model: card.id.clone(),
                    believed,
                    published,
                }),
                Some(_) => {}
                None => out.push(Drift {
                    model: card.id.clone(),
                    believed: (-1, -1),
                    published,
                }),
            }
        }
        out.sort_by(|a, b| a.model.cmp(&b.model));
        out
    }

    /// Exactly what the gateway will bill for this answer. The audit compares this against
    /// `usage_records.cost_nano`, per request, and a mismatch of one nanodollar fails.
    pub fn expected_charge(
        &self,
        served_by: &str,
        cache_hit: bool,
        prompt: u64,
        completion: u64,
    ) -> Option<Nano> {
        if cache_hit {
            return Some(0);
        }
        Some(self.get(served_by)?.cost_nano(prompt, completion))
    }
}

pub use crate::contract::models::Drift;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::models::{ModelCard, Pricing};

    fn card(id: &str, prompt: i64, completion: i64) -> ModelCard {
        ModelCard {
            id: id.into(),
            owned_by: "test".into(),
            context_window: 0,
            pricing: Some(Pricing {
                prompt_nano_per_token: prompt,
                completion_nano_per_token: completion,
            }),
        }
    }

    /// The table in this file, against the rate card in `config/models.toml`. If somebody
    /// changes one and not the other, this is where they find out — not on an invoice.
    #[test]
    fn the_shipped_table_agrees_with_the_shipped_rate_card() {
        let cards = vec![
            card("qwen3:4b", 50, 100),
            card("gpt-4o-mini", 200, 800),
            card("nomic-embed-text", 20, 0),
        ];
        assert_eq!(PriceTable::published().drift_against(&cards), vec![]);
    }

    #[test]
    fn a_price_change_at_the_gateway_is_a_drift() {
        let cards = vec![card("gpt-4o-mini", 250, 900)];
        let drift = PriceTable::published().drift_against(&cards);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].believed, (200, 800));
        assert_eq!(drift[0].published, (250, 900));
    }

    /// A model this client has never heard of cannot have its charges verified, which is
    /// exactly as bad as a wrong price and must not be silent.
    #[test]
    fn a_model_we_cannot_price_is_a_drift_too() {
        let cards = vec![card("claude-opus-4-8", 15_000, 75_000)];
        let drift = PriceTable::published().drift_against(&cards);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].believed, (-1, -1), "we had no belief at all");
    }

    /// An older gateway that publishes no rate card is not drifting; it is silent. Treating
    /// silence as disagreement would make every drift report cry wolf.
    #[test]
    fn an_unpriced_card_is_not_a_drift() {
        let cards = vec![ModelCard {
            id: "qwen3:4b".into(),
            owned_by: "ollama".into(),
            context_window: 0,
            pricing: None,
        }];
        assert_eq!(PriceTable::published().drift_against(&cards), vec![]);
    }

    #[test]
    fn published_prices_parse_exactly() {
        let p = Price::per_mtok("0.15", "0.60");
        assert_eq!(p.input_micros_per_mtok, 150_000);
        assert_eq!(p.output_micros_per_mtok, 600_000);
    }

    #[test]
    fn cost_matches_the_gateways_worked_example() {
        // crates/switchboard/src/usage/mod.rs: 1000 prompt @ $1/M + 1000 completion @ $2/M
        // = 3_000_000 nano = $0.003.
        let p = Price::per_mtok("1", "2");
        assert_eq!(p.cost_nano(1_000, 1_000), 3_000_000);
    }

    #[test]
    fn the_direct_table_prices_a_realistic_request() {
        let p = PriceTable::provider_direct().get("gpt-4o-mini").unwrap();
        // 1000 in, 200 out: 0.15*1000/1e6 + 0.60*200/1e6 USD = 0.00027 USD = 270_000 nano.
        assert_eq!(p.cost_nano(1_000, 200), 270_000);
    }

    #[test]
    fn no_model_on_the_rate_card_is_free() {
        // A free model makes every bill zero and every saving look like 100%, which is not
        // a measurement. Every request the bot sends is charged.
        let t = PriceTable::published();
        for alias in ["qwen3:4b", "gpt-4o-mini"] {
            assert!(!t.get(alias).unwrap().is_free(), "{alias} must be billed");
        }
    }

    #[test]
    fn local_capacity_undercuts_the_hosted_model_on_the_rate_card() {
        // The commercial claim, as an assertion. If this inverts, switchboard's price
        // routing has nothing to sell.
        let t = PriceTable::published();
        let (prompt, completion) = (50, 200);
        assert!(
            t.get("qwen3:4b").unwrap().cost_nano(prompt, completion)
                < t.get("gpt-4o-mini").unwrap().cost_nano(prompt, completion)
        );
    }

    #[test]
    fn reselling_the_hosted_model_costs_more_than_going_direct() {
        // The honest half of the pitch: switchboard marks gpt-4o-mini up. A caller who
        // only ever uses that model should go direct, and the verdict must be able to
        // reach that conclusion from these two tables.
        let (prompt, completion) = (50, 200);
        let via_gateway = PriceTable::published().get("gpt-4o-mini").unwrap();
        let direct = PriceTable::provider_direct().get("gpt-4o-mini").unwrap();
        assert!(via_gateway.cost_nano(prompt, completion) > direct.cost_nano(prompt, completion));
    }

    #[test]
    fn a_cache_hit_is_expected_to_be_free_whatever_the_model_costs() {
        let t = PriceTable::published();
        assert_eq!(t.expected_charge("gpt-4o-mini", true, 50, 200), Some(0));
        assert_eq!(t.expected_charge("qwen3:4b", false, 50, 200), Some(22_500));
    }

    #[test]
    fn large_token_counts_do_not_overflow() {
        // Ten million tokens at $10/M output. In i64 the intermediate
        // `10e6 * 10_000_000 * 1e9` overflows; in i128 it does not.
        let p = Price::per_mtok("0", "10");
        assert_eq!(
            p.cost_nano(0, 10_000_000),
            100 * super::super::money::NANO_PER_USD
        );
    }

    #[test]
    fn an_unknown_model_has_no_price_rather_than_a_free_one() {
        // Defaulting to zero would make every unpriced model look like a saving, which is
        // the most flattering possible bug for a report whose job is to be sceptical.
        assert!(PriceTable::published().get("some-new-model").is_none());
    }
}
