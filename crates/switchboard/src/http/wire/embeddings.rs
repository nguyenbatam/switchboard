use crate::domain::embed::{EmbeddingData, EncodingFormat};
use crate::domain::usage::Usage;
use base64::Engine;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingListResponse {
    pub object: String,
    pub data: Vec<EmbeddingDataWire>,
    pub model: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingDataWire {
    pub object: String,
    pub index: u32,
    pub embedding: EmbeddingVector,
}

/// Either a JSON array of floats, or those floats as little-endian `f32` bytes in
/// base64 — the encoding OpenAI's own Python client requests by default.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Float(Vec<f32>),
    Base64(String),
}

fn encode_base64(v: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for f in v {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl EmbeddingListResponse {
    pub fn new(
        model: String,
        data: Vec<EmbeddingData>,
        usage: Usage,
        format: EncodingFormat,
    ) -> Self {
        let data = data
            .into_iter()
            .map(|d| EmbeddingDataWire {
                object: d.object.unwrap_or_else(|| "embedding".into()),
                index: d.index,
                embedding: match format {
                    EncodingFormat::Float => EmbeddingVector::Float(d.embedding),
                    EncodingFormat::Base64 => EmbeddingVector::Base64(encode_base64(&d.embedding)),
                },
            })
            .collect();

        Self {
            object: "list".into(),
            data,
            model,
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_to_the_same_floats() {
        let original = vec![0.5f32, -1.25, 3.0e-8];
        let encoded = encode_base64(&original);

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(bytes.len(), original.len() * 4);

        let decoded: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(
            decoded, original,
            "a client must get its vector back intact"
        );
    }

    #[test]
    fn float_format_emits_a_json_array() {
        let resp = EmbeddingListResponse::new(
            "m".into(),
            vec![EmbeddingData {
                index: 0,
                embedding: vec![1.0, 2.0],
                object: None,
            }],
            Usage::default(),
            EncodingFormat::Float,
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["data"][0]["embedding"].is_array());
        assert_eq!(json["data"][0]["object"], "embedding");
    }

    #[test]
    fn base64_format_emits_a_string() {
        let resp = EmbeddingListResponse::new(
            "m".into(),
            vec![EmbeddingData {
                index: 0,
                embedding: vec![1.0, 2.0],
                object: None,
            }],
            Usage::default(),
            EncodingFormat::Base64,
        );
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["data"][0]["embedding"].is_string());
    }
}
