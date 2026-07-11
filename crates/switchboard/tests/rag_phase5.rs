use switchboard::rag::chunk::Chunker;

#[test]
fn chunker_splits_long_text() {
    let text =
        "This is a longer paragraph with enough content to exceed chunk limits. ".repeat(200);
    let chunks = Chunker::default().split("Title", &text);
    assert!(chunks.len() > 1);
    assert!(chunks[0].starts_with("# Title"));
}

#[test]
fn mmr_dedupe_reduces_duplicates() {
    use switchboard::rag::context::mmr_dedupe;
    use switchboard::rag::search::SearchHit;
    use uuid::Uuid;

    let id = Uuid::now_v7();
    let hits = vec![
        SearchHit {
            chunk_id: Uuid::now_v7(),
            document_id: id,
            ord: 0,
            content: "hello world foo bar".into(),
            score: 1.0,
            citation: "doc:test#0".into(),
        },
        SearchHit {
            chunk_id: Uuid::now_v7(),
            document_id: id,
            ord: 1,
            content: "hello world foo bar baz".into(),
            score: 0.9,
            citation: "doc:test#1".into(),
        },
    ];
    let deduped = mmr_dedupe(hits, 0.7);
    assert_eq!(deduped.len(), 2);
}
