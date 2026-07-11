use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::domain::chat::ChatRequest;

/// Canonical JSON key for exact-match cache.
pub fn cache_key(tenant_id: uuid::Uuid, req: &ChatRequest) -> String {
    let canonical = canonical_request(req);
    let hash = Sha256::digest(canonical.as_bytes());
    format!(
        "{}:{}",
        tenant_id,
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Every field that can change the answer belongs in the key. A field left out is a
/// collision waiting to happen: two different requests, one cached response, and
/// the second caller silently receives the first caller's answer.
fn canonical_request(req: &ChatRequest) -> String {
    #[derive(Serialize)]
    struct Canon<'a> {
        model: &'a str,
        messages: &'a [crate::domain::chat::Message],
        tools: &'a Option<Vec<crate::domain::chat::Tool>>,
        temperature: Option<f32>,
        top_p: Option<f32>,
        top_k: Option<u32>,
        max_tokens: Option<u32>,
        seed: Option<i64>,
        stop: &'a Option<crate::domain::chat::StopSequences>,
        response_format: &'a Option<crate::domain::chat::ResponseFormat>,
    }

    let c = Canon {
        model: &req.model,
        messages: &req.messages,
        tools: &req.tools,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        max_tokens: req.max_tokens,
        seed: req.seed,
        stop: &req.stop,
        response_format: &req.response_format,
    };
    serde_json::to_string(&c).unwrap_or_default()
}

/// Cacheable when the client asked for a reproducible answer — greedy decoding or a
/// pinned seed — and no tool in the request can have side effects.
pub fn is_cacheable(req: &ChatRequest) -> bool {
    let deterministic = req.temperature.unwrap_or(1.0) == 0.0 || req.seed.is_some();
    let no_side_effect_tools = req.tools.as_ref().is_none_or(|t| t.is_empty());
    deterministic && no_side_effect_tools
}

/// Sorted-key canonicalisation for property testing.
pub fn canonical_sorted_json(value: &serde_json::Value) -> String {
    fn sort_value(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let sorted: BTreeMap<_, _> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), sort_value(v)))
                    .collect();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(sort_value).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort_value(value)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::chat::{ChatRequest, Message, Role, StopSequences};
    use proptest::prelude::*;

    fn request(content: &str) -> ChatRequest {
        ChatRequest {
            model: "qwen3:4b".into(),
            messages: vec![Message {
                role: Role::User,
                content: Some(crate::domain::chat::MessageContent::Text(content.into())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            tool_choice: None,
            temperature: Some(0.0),
            top_p: None,
            max_tokens: None,
            seed: None,
            stop: None,
            top_k: None,
            stream: false,
            stream_options: None,
            response_format: None,
            parallel_tool_calls: None,
            rag: None,
            user: None,
            legacy_completion: false,
            untrusted_context: false,
        }
    }

    fn digest(req: &ChatRequest) -> String {
        cache_key(uuid::Uuid::now_v7(), req)
            .split(':')
            .nth(1)
            .unwrap()
            .to_string()
    }

    proptest! {
        #[test]
        fn canonical_is_stable(s in ".*") {
            let req = request(&s);
            prop_assert_eq!(digest(&req), digest(&req));
        }
    }

    #[test]
    fn tenant_scopes_the_key() {
        let req = request("hi");
        let a = cache_key(uuid::Uuid::now_v7(), &req);
        let b = cache_key(uuid::Uuid::now_v7(), &req);
        assert_ne!(a, b, "one tenant must never read another tenant's cache");
    }

    #[test]
    fn seed_participates_in_the_key() {
        let mut a = request("hi");
        let mut b = request("hi");
        a.seed = Some(1);
        b.seed = Some(2);
        assert_ne!(
            digest(&a),
            digest(&b),
            "different seeds are different requests"
        );
    }

    #[test]
    fn stop_participates_in_the_key() {
        let mut a = request("hi");
        let mut b = request("hi");
        a.stop = Some(StopSequences::One("\n".into()));
        b.stop = Some(StopSequences::One("END".into()));
        assert_ne!(digest(&a), digest(&b));
    }

    #[test]
    fn top_k_participates_in_the_key() {
        let mut a = request("hi");
        let mut b = request("hi");
        a.top_k = Some(10);
        b.top_k = Some(40);
        assert_ne!(digest(&a), digest(&b));
    }

    #[test]
    fn a_pinned_seed_is_cacheable_even_when_sampling() {
        let mut req = request("hi");
        req.temperature = Some(0.7);
        assert!(!is_cacheable(&req), "sampling alone is not reproducible");

        req.seed = Some(42);
        assert!(is_cacheable(&req), "a pinned seed makes it reproducible");
    }

    #[test]
    fn requests_carrying_tools_are_not_cacheable() {
        let mut req = request("hi");
        req.tools = Some(vec![crate::domain::chat::Tool {
            r#type: "function".into(),
            function: crate::domain::chat::ToolFunction {
                name: "wallet_balance".into(),
                description: None,
                parameters: crate::domain::schema::ToolParameters::object(
                    Default::default(),
                    vec![],
                    false,
                ),
            },
        }]);
        assert!(!is_cacheable(&req));
    }
}
