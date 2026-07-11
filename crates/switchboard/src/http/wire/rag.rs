use crate::rag::search::{SearchHit, SearchMode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct DocumentUploadRequest {
    pub content: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub chunk_size: Option<usize>,
    #[serde(default)]
    pub chunk_overlap: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct DocumentUploadResponse {
    pub id: Uuid,
    pub chunks: usize,
    pub deduplicated: bool,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_k")]
    pub k: u32,
    #[serde(default)]
    pub mode: SearchMode,
}

fn default_k() -> u32 {
    5
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHitWire>,
}

#[derive(Debug, Serialize)]
pub struct SearchHitWire {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub ord: i32,
    pub content: String,
    pub score: f32,
    pub citation: String,
}

impl From<SearchHit> for SearchHitWire {
    fn from(h: SearchHit) -> Self {
        Self {
            chunk_id: h.chunk_id,
            document_id: h.document_id,
            ord: h.ord,
            content: h.content,
            score: h.score,
            citation: h.citation,
        }
    }
}
