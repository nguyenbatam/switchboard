use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub model: String,
    pub input: EmbedInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    /// The caller's own identifier for the end user. See `ChatRequest::user`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// OpenAI accepts four shapes here and clients use all four. The pre-tokenized
/// variants are the ones gateways forget: a client that already ran the tokenizer
/// sends token ids, and a gateway modelling only `string | string[]` answers it
/// with a deserialization error.
///
/// Order matters — `serde(untagged)` tries variants top to bottom.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbedInput {
    Single(String),
    Batch(Vec<String>),
    Tokens(Vec<u32>),
    TokenBatch(Vec<Vec<u32>>),
}

impl EmbedInput {
    /// How many embeddings this request should produce.
    pub fn len(&self) -> usize {
        match self {
            Self::Single(_) | Self::Tokens(_) => 1,
            Self::Batch(v) => v.len(),
            Self::TokenBatch(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The text inputs, if this request carries text rather than token ids.
    pub fn as_texts(&self) -> Option<Vec<String>> {
        match self {
            Self::Single(s) => Some(vec![s.clone()]),
            Self::Batch(v) => Some(v.clone()),
            Self::Tokens(_) | Self::TokenBatch(_) => None,
        }
    }
}

/// `float` (the default) or `base64`. Base64 roughly halves the JSON payload of a
/// large batch, which is why clients ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingFormat {
    Float,
    Base64,
}

impl EncodingFormat {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None | Some("float") => Ok(Self::Float),
            Some("base64") => Ok(Self::Base64),
            Some(other) => Err(format!(
                "invalid encoding_format '{other}': expected 'float' or 'base64'"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedResponse {
    pub model: String,
    pub data: Vec<EmbeddingData>,
    pub usage: super::usage::Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub index: u32,
    pub embedding: Vec<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> EmbedInput {
        serde_json::from_str(json).expect("input parses")
    }

    #[test]
    fn all_four_openai_input_shapes_deserialize() {
        assert!(matches!(parse(r#""hello""#), EmbedInput::Single(_)));
        assert!(matches!(parse(r#"["a","b"]"#), EmbedInput::Batch(_)));
        assert!(matches!(parse("[1,2,3]"), EmbedInput::Tokens(_)));
        assert!(matches!(parse("[[1,2],[3,4]]"), EmbedInput::TokenBatch(_)));
    }

    #[test]
    fn len_counts_the_embeddings_that_will_come_back() {
        assert_eq!(parse(r#""hi""#).len(), 1);
        assert_eq!(parse(r#"["a","b","c"]"#).len(), 3);
        assert_eq!(parse("[1,2,3,4,5]").len(), 1, "one pre-tokenized document");
        assert_eq!(parse("[[1,2],[3,4]]").len(), 2);
    }

    #[test]
    fn token_inputs_have_no_text() {
        assert!(parse("[1,2,3]").as_texts().is_none());
        assert_eq!(parse(r#"["a"]"#).as_texts().unwrap(), vec!["a".to_string()]);
    }

    #[test]
    fn encoding_format_defaults_to_float_and_rejects_nonsense() {
        assert_eq!(EncodingFormat::parse(None).unwrap(), EncodingFormat::Float);
        assert_eq!(
            EncodingFormat::parse(Some("base64")).unwrap(),
            EncodingFormat::Base64
        );
        assert!(EncodingFormat::parse(Some("float16")).is_err());
    }
}
