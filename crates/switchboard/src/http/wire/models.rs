use serde::Serialize;

use crate::config::models::ModelSpec;

#[derive(Debug, Clone, Serialize)]
pub struct ModelListResponse {
    pub object: String,
    pub data: Vec<ModelObject>,
}

/// What a tenant may know about a model before spending money on it.
///
/// The four OpenAI fields come first, because every SDK deserialises them. Everything
/// after is switchboard's, and exists so a partner can price a call *before* making it
/// rather than reading the price off an invoice a month later.
#[derive(Debug, Clone, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,

    /// Maximum context, in tokens.
    pub context_window: u32,
    /// The rate card. Never the cost of goods — see [`Pricing`].
    pub pricing: Pricing,
    /// Aliases this model falls back to when its provider is unavailable. A caller who
    /// sees an `x-switchboard-fallback` header can look up here what it meant.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
}

/// What the tenant pays.
///
/// **`cost_in` / `cost_out` are absent, and that absence is the point of this type.**
/// Those are what *we* pay upstream, and the difference is the margin. Migration 007 goes
/// as far as revoking that column from the read-only role reachable by `sql_query`, so
/// that a prompt-injected tool call cannot read it either. A wire struct that *could*
/// serialise the margin is a wire struct that eventually will.
#[derive(Debug, Clone, Serialize)]
pub struct Pricing {
    /// Prompt tokens, USD per million. For humans.
    pub prompt_usd_per_million: f64,
    /// Completion tokens, USD per million. For humans.
    pub completion_usd_per_million: f64,
    /// The same two rates as integers, in nanodollars per single token.
    ///
    /// Money must not cross `f64` in a client's arithmetic any more than it does in ours
    /// (see `config/pricing.rs`). A client that multiplies these by the token counts in
    /// `usage` lands on exactly the integer we bill.
    pub prompt_nano_per_token: i64,
    pub completion_nano_per_token: i64,
}

impl Pricing {
    /// Nanodollars per token, from USD per million tokens.
    ///
    /// `1 USD = 1e9 nanodollars`, so `usd_per_million / 1e6 * 1e9 = usd_per_million * 1000`.
    fn nano_per_token(usd_per_million: f64) -> i64 {
        (usd_per_million * 1_000.0).round() as i64
    }

    fn from_spec(spec: &ModelSpec) -> Self {
        Self {
            prompt_usd_per_million: spec.price_in,
            completion_usd_per_million: spec.price_out,
            prompt_nano_per_token: Self::nano_per_token(spec.price_in),
            completion_nano_per_token: Self::nano_per_token(spec.price_out),
        }
    }
}

impl ModelObject {
    pub fn from_spec(spec: &ModelSpec) -> Self {
        Self {
            id: spec.alias.clone(),
            object: "model".into(),
            created: 1_686_935_002,
            owned_by: spec.provider.clone(),
            context_window: spec.context,
            pricing: Pricing::from_spec(spec),
            fallback: spec.fallback.clone(),
        }
    }
}

impl ModelListResponse {
    pub fn new(data: Vec<ModelObject>) -> Self {
        Self {
            object: "list".into(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tenant that can read the rate card can budget. One that can read the cost card can
    /// negotiate against the margin.
    #[test]
    fn the_wire_shape_cannot_express_the_cost_of_goods() {
        let json = serde_json::to_string(&Pricing {
            prompt_usd_per_million: 0.15,
            completion_usd_per_million: 0.60,
            prompt_nano_per_token: 150,
            completion_nano_per_token: 600,
        })
        .unwrap();

        assert!(!json.contains("cost"), "{json}");
        assert!(json.contains("prompt_usd_per_million"));
    }

    /// `$0.15` per million tokens is `150` nanodollars per token. A client pricing its own
    /// spend from these must land on the same integer we bill it.
    #[test]
    fn nanodollars_per_token_match_the_usd_rate_card() {
        assert_eq!(Pricing::nano_per_token(0.15), 150);
        assert_eq!(Pricing::nano_per_token(0.60), 600);
        assert_eq!(Pricing::nano_per_token(10.0), 10_000);
        assert_eq!(Pricing::nano_per_token(0.0), 0);
        // Sub-nanodollar rates round rather than truncate.
        assert_eq!(Pricing::nano_per_token(0.0004), 0);
        assert_eq!(Pricing::nano_per_token(0.0006), 1);
    }
}
