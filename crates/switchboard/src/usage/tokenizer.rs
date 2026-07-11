//! Token counting.
//!
//! One encoder per model alias, built once at boot. `chars / 3.6` is the last
//! resort, not the default, and any record it produces is flagged `estimated`
//! so the drift counter can tell a guess from a measurement.

use crate::config::models::{ModelRegistry, TokenizerSpec};
use crate::domain::chat::{ChatRequest, ContentPart, MessageContent, Role};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

const CHARS_PER_TOKEN: f64 = 3.6;

/// `<|start|>{role}<|message|>{content}<|end|>` framing, per message.
const PER_MESSAGE_OVERHEAD: u32 = 3;
/// The assistant turn the model is primed to write.
const REPLY_PRIMING_OVERHEAD: u32 = 3;
/// Preamble the provider wraps around the tool schema block.
const TOOL_BLOCK_OVERHEAD: u32 = 8;

/// A `detail: low` image is a single 512px tile.
const IMAGE_BASE_TOKENS: u32 = 85;
const IMAGE_TILE_TOKENS: u32 = 170;
/// `detail: high`/`auto` tiles depend on pixel dimensions we do not have without
/// fetching the image, so we assume a 2x2 grid and mark the count estimated.
const ASSUMED_HIGH_DETAIL_TILES: u32 = 4;

/// Bounds on the prompt size. `expected` is what we bill against; `upper` is what
/// we reserve against the token bucket.
#[derive(Debug, Default, Clone)]
pub struct TokenEstimate {
    pub lower: u32,
    pub expected: u32,
    pub upper: u32,
    /// True when any part of the count was guessed rather than encoded.
    pub estimated: bool,
}

enum Encoder {
    Tiktoken(tiktoken_rs::CoreBPE),
    Hf(Box<tokenizers::Tokenizer>),
    Estimate,
}

impl Encoder {
    /// Returns `(tokens, exact)`.
    fn count(&self, text: &str) -> (u32, bool) {
        match self {
            Self::Tiktoken(bpe) => (bpe.encode_ordinary(text).len() as u32, true),
            Self::Hf(tk) => match tk.encode(text, false) {
                Ok(enc) => (enc.len() as u32, true),
                Err(_) => (estimate_chars(text), false),
            },
            Self::Estimate => (estimate_chars(text), false),
        }
    }
}

fn estimate_chars(text: &str) -> u32 {
    ((text.chars().count() as f64) / CHARS_PER_TOKEN).ceil() as u32
}

/// Where vendored HuggingFace `tokenizer.json` files live.
fn hf_tokenizer_path(repo: &str) -> PathBuf {
    PathBuf::from("config/tokenizers").join(format!("{}.json", repo.replace('/', "_")))
}

pub struct TokenCounter {
    by_alias: HashMap<String, Arc<Encoder>>,
    fallback: Arc<Encoder>,
}

impl TokenCounter {
    /// Builds one encoder per alias, sharing a single `CoreBPE` across models that
    /// name the same tiktoken encoding.
    pub fn new(registry: &ModelRegistry) -> Self {
        let mut tiktoken_cache: HashMap<String, Arc<Encoder>> = HashMap::new();
        let mut by_alias = HashMap::new();

        for spec in registry.list() {
            let encoder = match &spec.tokenizer {
                TokenizerSpec::Tiktoken { encoding } => {
                    if let Some(cached) = tiktoken_cache.get(encoding) {
                        cached.clone()
                    } else {
                        let built = match load_tiktoken(encoding) {
                            Some(bpe) => Arc::new(Encoder::Tiktoken(bpe)),
                            None => {
                                tracing::warn!(
                                    encoding,
                                    alias = %spec.alias,
                                    "unknown tiktoken encoding, token counts will be estimated"
                                );
                                Arc::new(Encoder::Estimate)
                            }
                        };
                        tiktoken_cache.insert(encoding.clone(), built.clone());
                        built
                    }
                }
                TokenizerSpec::Hf { repo } => {
                    let path = hf_tokenizer_path(repo);
                    match tokenizers::Tokenizer::from_file(&path) {
                        Ok(tk) => Arc::new(Encoder::Hf(Box::new(tk))),
                        Err(e) => {
                            tracing::warn!(
                                repo,
                                path = %path.display(),
                                error = %e,
                                "no vendored tokenizer.json, token counts will be estimated"
                            );
                            Arc::new(Encoder::Estimate)
                        }
                    }
                }
                TokenizerSpec::Estimate => Arc::new(Encoder::Estimate),
            };
            by_alias.insert(spec.alias.clone(), encoder);
        }

        Self {
            by_alias,
            fallback: Arc::new(Encoder::Estimate),
        }
    }

    fn encoder(&self, model: &str) -> &Encoder {
        self.by_alias.get(model).unwrap_or(&self.fallback)
    }

    /// Counts a bare string. Used to tally streamed deltas locally, so a client
    /// that disconnects mid-stream is still billed for the tokens that crossed
    /// the wire — the upstream usage chunk never arrives in that case.
    ///
    /// Returns `(tokens, exact)`.
    pub fn count_text(&self, model: &str, text: &str) -> (u32, bool) {
        self.encoder(model).count(text)
    }

    /// Counts the prompt as the provider will see it: chat template framing, tool
    /// schemas, and image tiles included.
    pub fn estimate(&self, req: &ChatRequest, model: &str) -> TokenEstimate {
        let enc = self.encoder(model);
        let mut total = 0u32;
        let mut exact = true;

        let add = |text: &str, total: &mut u32, exact: &mut bool| {
            let (n, ok) = enc.count(text);
            *total += n;
            *exact &= ok;
        };

        for m in &req.messages {
            total += PER_MESSAGE_OVERHEAD;
            add(role_str(&m.role), &mut total, &mut exact);

            if let Some(name) = &m.name {
                add(name, &mut total, &mut exact);
                total += 1;
            }

            match &m.content {
                Some(MessageContent::Text(t)) => add(t, &mut total, &mut exact),
                Some(MessageContent::Parts(parts)) => {
                    for part in parts {
                        match part {
                            ContentPart::Text { text } => add(text, &mut total, &mut exact),
                            ContentPart::ImageUrl { image_url } => {
                                let low = image_url.detail.as_deref() == Some("low");
                                if low {
                                    total += IMAGE_BASE_TOKENS;
                                } else {
                                    // Tile count depends on pixel dimensions we would
                                    // have to fetch the image to learn.
                                    total += IMAGE_BASE_TOKENS
                                        + IMAGE_TILE_TOKENS * ASSUMED_HIGH_DETAIL_TILES;
                                    exact = false;
                                }
                            }
                        }
                    }
                }
                None => {}
            }

            if let Some(calls) = &m.tool_calls {
                for c in calls {
                    add(&c.function.name, &mut total, &mut exact);
                    add(&c.function.arguments, &mut total, &mut exact);
                    total += PER_MESSAGE_OVERHEAD;
                }
            }
            if let Some(id) = &m.tool_call_id {
                add(id, &mut total, &mut exact);
            }
        }

        total += REPLY_PRIMING_OVERHEAD;

        if let Some(tools) = &req.tools
            && !tools.is_empty()
            && let Ok(json) = serde_json::to_string(tools)
        {
            add(&json, &mut total, &mut exact);
            total += TOOL_BLOCK_OVERHEAD;
        }

        if let Some(rf) = &req.response_format
            && let Ok(json) = serde_json::to_string(rf)
        {
            add(&json, &mut total, &mut exact);
        }

        if exact {
            return TokenEstimate {
                lower: total,
                expected: total,
                upper: total,
                estimated: false,
            };
        }

        TokenEstimate {
            lower: (total as f64 * 0.8) as u32,
            expected: total,
            upper: (total as f64 * 1.3) as u32 + 64,
            estimated: true,
        }
    }
}

fn role_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn load_tiktoken(encoding: &str) -> Option<tiktoken_rs::CoreBPE> {
    match encoding {
        "o200k_base" => tiktoken_rs::o200k_base().ok(),
        "cl100k_base" => tiktoken_rs::cl100k_base().ok(),
        "p50k_base" => tiktoken_rs::p50k_base().ok(),
        "p50k_edit" => tiktoken_rs::p50k_edit().ok(),
        "r50k_base" => tiktoken_rs::r50k_base().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::chat::Message;

    fn text_request(model: &str, content: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                role: Role::User,
                content: Some(MessageContent::Text(content.into())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            tool_choice: None,
            temperature: None,
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

    fn counter_with(alias: &str, spec: TokenizerSpec) -> TokenCounter {
        let registry = ModelRegistry::for_test(alias, spec);
        TokenCounter::new(&registry)
    }

    #[test]
    fn tiktoken_counts_are_exact_and_not_flagged() {
        let counter = counter_with(
            "gpt-4o-mini",
            TokenizerSpec::Tiktoken {
                encoding: "o200k_base".into(),
            },
        );
        let est = counter.estimate(&text_request("gpt-4o-mini", "hello world"), "gpt-4o-mini");

        assert!(!est.estimated);
        assert_eq!(est.lower, est.upper, "an exact count has no spread");
        // 2 content tokens + 1 role token + 3 framing + 3 priming.
        assert_eq!(est.expected, 9);
    }

    #[test]
    fn estimate_arm_is_flagged_and_bounded() {
        let counter = counter_with("qwen3:4b", TokenizerSpec::Estimate);
        let est = counter.estimate(&text_request("qwen3:4b", &"a".repeat(360)), "qwen3:4b");

        assert!(est.estimated, "a guess must announce itself");
        assert!(est.lower < est.expected && est.expected < est.upper);
        assert!(est.expected >= 100, "360 chars / 3.6 = 100 content tokens");
    }

    #[test]
    fn unknown_model_falls_back_to_estimating() {
        let counter = counter_with("qwen3:4b", TokenizerSpec::Estimate);
        let est = counter.estimate(&text_request("never-configured", "hi"), "never-configured");
        assert!(est.estimated);
        assert!(est.expected > 0);
    }

    #[test]
    fn tool_schemas_are_counted() {
        let counter = counter_with(
            "gpt-4o-mini",
            TokenizerSpec::Tiktoken {
                encoding: "o200k_base".into(),
            },
        );
        let bare = counter.estimate(&text_request("gpt-4o-mini", "hi"), "gpt-4o-mini");

        let mut with_tools = text_request("gpt-4o-mini", "hi");
        with_tools.tools = Some(vec![crate::domain::chat::Tool {
            r#type: "function".into(),
            function: crate::domain::chat::ToolFunction {
                name: "calculator".into(),
                description: Some("Evaluate an arithmetic expression".into()),
                parameters: crate::domain::schema::ToolParameters::object(
                    Default::default(),
                    vec![],
                    false,
                ),
            },
        }]);
        let tooled = counter.estimate(&with_tools, "gpt-4o-mini");

        assert!(
            tooled.expected > bare.expected + TOOL_BLOCK_OVERHEAD,
            "tool schemas occupy context and must be billed"
        );
    }

    #[test]
    fn images_force_an_estimate() {
        let counter = counter_with(
            "gpt-4o-mini",
            TokenizerSpec::Tiktoken {
                encoding: "o200k_base".into(),
            },
        );
        let mut req = text_request("gpt-4o-mini", "");
        req.messages[0].content = Some(MessageContent::Parts(vec![ContentPart::ImageUrl {
            image_url: crate::domain::chat::ImageUrl {
                url: "https://example.test/a.png".into(),
                detail: None,
            },
        }]));

        let est = counter.estimate(&req, "gpt-4o-mini");
        assert!(est.estimated, "tile count is unknown without the image");
        assert!(est.expected >= IMAGE_BASE_TOKENS);
    }

    #[test]
    fn low_detail_image_is_a_single_tile_and_exact() {
        let counter = counter_with(
            "gpt-4o-mini",
            TokenizerSpec::Tiktoken {
                encoding: "o200k_base".into(),
            },
        );
        let mut req = text_request("gpt-4o-mini", "");
        req.messages[0].content = Some(MessageContent::Parts(vec![ContentPart::ImageUrl {
            image_url: crate::domain::chat::ImageUrl {
                url: "https://example.test/a.png".into(),
                detail: Some("low".into()),
            },
        }]));

        let est = counter.estimate(&req, "gpt-4o-mini");
        assert!(!est.estimated);
        assert_eq!(est.expected, IMAGE_BASE_TOKENS + 1 + 3 + 3);
    }
}
