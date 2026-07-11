use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    #[default]
    Hybrid,
    Vector,
    Lexical,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub ord: i32,
    pub content: String,
    pub score: f32,
    pub citation: String,
}

impl SearchHit {
    pub fn citation_for(document_id: Uuid, ord: i32) -> String {
        let short = &document_id.to_string()[..8];
        format!("doc:{short}#{ord}")
    }
}
