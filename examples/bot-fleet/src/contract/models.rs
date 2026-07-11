//! `GET /v1/models` — the gateway's rate card, as published.
//!
//! [`crate::economics::prices`] holds a *client-side copy* of this, and says why: the
//! question this crate answers is "would I be better off calling OpenAI directly", and
//! answering it with the gateway's own numbers would assume the conclusion. That reasoning
//! is right, and it leaves one problem behind — a client-side copy goes stale, silently,
//! and the first symptom is a reconciliation failure at the end of a run that has already
//! spent the money.
//!
//! This module closes that gap without reopening the first one. The **direct** baseline
//! stays the client's own belief about OpenAI's list prices, because that is the number the
//! gateway has no business supplying. But the **gateway's** rate card is a fact the gateway
//! is authoritative about, and comparing it against what we believed is a check that costs
//! one request and can be run before the first user is kept waiting.
//!
//! A drift is not automatically a bug. It is either a stale table here or a price change
//! there, and both are worth knowing about at boot rather than at invoice.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::client::GatewayClient;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelCard {
    pub id: String,
    #[serde(default)]
    pub owned_by: String,
    #[serde(default)]
    pub context_window: u32,
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

/// What the tenant pays. The gateway's cost of goods is deliberately absent from the wire,
/// so there is nothing here to deserialize it into.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct Pricing {
    /// Nanodollars per single prompt token. Integers, because money must not cross an
    /// `f64` in our arithmetic any more than it does in the gateway's.
    pub prompt_nano_per_token: i64,
    pub completion_nano_per_token: i64,
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelCard>,
}

impl GatewayClient {
    /// The catalogue and its rate card.
    pub async fn models(&self) -> Result<Vec<ModelCard>> {
        let resp = self
            .http()
            .get(format!("{}/v1/models", self.config().base_url))
            .bearer_auth(&self.config().api_key)
            .send()
            .await
            .context("GET /v1/models")?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET /v1/models returned {status}");
        }
        Ok(resp
            .json::<ModelListResponse>()
            .await
            .context("decoding /v1/models")?
            .data)
    }
}

/// One model where the client's price table and the gateway's rate card disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub model: String,
    /// `(prompt, completion)` nanodollars per token, as this client believed.
    pub believed: (i64, i64),
    /// …and as the gateway publishes.
    pub published: (i64, i64),
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: believed {}/{} nano-per-token, gateway publishes {}/{}",
            self.model, self.believed.0, self.believed.1, self.published.0, self.published.1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_shape_has_nowhere_to_put_a_cost_of_goods() {
        // A gateway that started leaking its margin would not be able to smuggle it through
        // this type, and a reviewer adding a `cost_in` field here would have to say why.
        let json = r#"{"id":"gpt-4o-mini","object":"model","created":1,"owned_by":"openai",
            "context_window":128000,
            "pricing":{"prompt_usd_per_million":0.2,"completion_usd_per_million":0.8,
                       "prompt_nano_per_token":200,"completion_nano_per_token":800}}"#;
        let card: ModelCard = serde_json::from_str(json).expect("decodes");
        let p = card.pricing.expect("priced");
        assert_eq!(p.prompt_nano_per_token, 200);
        assert_eq!(p.completion_nano_per_token, 800);
    }

    /// An older gateway serves `/v1/models` without a rate card. That is a missing feature,
    /// not a malformed response, and it must not stop a fleet from starting.
    #[test]
    fn a_gateway_without_a_rate_card_still_decodes() {
        let json = r#"{"id":"qwen3:4b","object":"model","created":1,"owned_by":"ollama"}"#;
        let card: ModelCard = serde_json::from_str(json).expect("decodes");
        assert!(card.pricing.is_none());
    }

    #[test]
    fn a_drift_reads_as_a_sentence() {
        let d = Drift {
            model: "gpt-4o-mini".into(),
            believed: (200, 800),
            published: (250, 900),
        };
        assert_eq!(
            d.to_string(),
            "gpt-4o-mini: believed 200/800 nano-per-token, gateway publishes 250/900"
        );
    }
}
