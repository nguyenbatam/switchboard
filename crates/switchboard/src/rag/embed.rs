//! nomic-embed-text task prefixes — applied here, never by callers.

pub const DOC_PREFIX: &str = "search_document: ";
pub const QUERY_PREFIX: &str = "search_query: ";

pub fn prefix_document(text: &str) -> String {
    format!("{DOC_PREFIX}{text}")
}

pub fn prefix_query(text: &str) -> String {
    format!("{QUERY_PREFIX}{text}")
}

pub fn estimate_tokens(text: &str) -> u16 {
    (text.len() as u32 / 4).max(1).min(u16::MAX as u32) as u16
}
