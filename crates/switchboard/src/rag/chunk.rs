//! Structural chunking: headings → paragraphs → sentences, sized in tokens.
//!
//! Three rules decide whether retrieval works at all:
//!
//! 1. **Every chunk carries its heading path.** A chunk reading "rotate it every 90
//!    days" is neither retrievable nor usable; `# Guide > ## Auth > ### Rotation`
//!    in front of it is both.
//! 2. **A table row never leaves its header.** `| EU | 30 |` means nothing without
//!    `| region | days |`, so a split table repeats the header in every piece.
//! 3. **Sizing is in tokens.** A character budget silently halves the real chunk
//!    size for CJK and inflates it for code, so one config yields different chunks
//!    per language.
//!
//! Sizing uses `cl100k_base` as a yardstick. It is not the embedding model's
//! tokenizer — it only has to be a stable measure that keeps chunks inside the
//! embedder's context — and it is never used for billing.

use std::sync::OnceLock;

const DEFAULT_CHUNK_TOKENS: usize = 512;
const DEFAULT_OVERLAP_TOKENS: usize = 64;
const CHARS_PER_TOKEN_FALLBACK: f64 = 3.6;

fn measure() -> Option<&'static tiktoken_rs::CoreBPE> {
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

fn count_tokens(text: &str) -> usize {
    match measure() {
        Some(bpe) => bpe.encode_ordinary(text).len(),
        None => ((text.chars().count() as f64) / CHARS_PER_TOKEN_FALLBACK).ceil() as usize,
    }
}

pub struct Chunker {
    pub chunk_tokens: usize,
    pub overlap_tokens: usize,
}

impl Default for Chunker {
    fn default() -> Self {
        Self {
            chunk_tokens: DEFAULT_CHUNK_TOKENS,
            overlap_tokens: DEFAULT_OVERLAP_TOKENS,
        }
    }
}

impl Chunker {
    pub fn split(&self, title: &str, body: &str) -> Vec<String> {
        let segments = parse(body);
        let mut chunks = Vec::new();

        // A chunk never spans two heading paths: its prefix could name only one of
        // them, and the other half would be mislabelled.
        for group in group_by_path(segments) {
            let prefix = build_prefix(title, &group.path);
            let mut packer = Packer::new(prefix, self.chunk_tokens, self.overlap_tokens);

            for seg in group.segments {
                match seg {
                    SegBody::Para(text) => self.pack_paragraph(&mut packer, &text),
                    SegBody::Code(text) => self.pack_code(&mut packer, &text),
                    SegBody::Table { header, rows } => {
                        packer.begin_table(header);
                        for row in rows {
                            packer.push_row(row);
                        }
                        packer.end_table();
                    }
                }
            }
            packer.finish(&mut chunks);
        }

        if chunks.is_empty() && !body.trim().is_empty() {
            chunks.push(format!("{}{}", build_prefix(title, &[]), body.trim()));
        }
        chunks
    }

    fn pack_paragraph(&self, packer: &mut Packer, text: &str) {
        if packer.fits_alone(text) {
            packer.push_block(text.to_string());
            return;
        }
        for sentence in split_sentences(text) {
            if packer.fits_alone(&sentence) {
                packer.push_block(sentence);
            } else {
                // A single sentence larger than the whole budget. Nothing structural
                // is left to split on, so cut on character boundaries.
                for piece in hard_split(&sentence, packer.budget()) {
                    packer.push_block(piece);
                }
            }
        }
    }

    fn pack_code(&self, packer: &mut Packer, text: &str) {
        if packer.fits_alone(text) {
            packer.push_block(text.to_string());
            return;
        }
        // Too large to keep whole. Split by lines and re-fence each piece, so no
        // chunk holds a code block whose boundaries the model cannot see.
        let lines: Vec<&str> = text.lines().collect();
        let fence = lines.first().copied().unwrap_or("```");
        let end = lines.len().saturating_sub(1).max(1);
        let inner = &lines[1.min(lines.len())..end];

        let mut buf: Vec<&str> = Vec::new();
        let mut tokens = 0usize;
        let line_budget = packer.budget().saturating_sub(8).max(8);

        for line in inner {
            let line_tokens = count_tokens(line) + 1;
            if tokens + line_tokens > line_budget && !buf.is_empty() {
                packer.push_block(format!("{fence}\n{}\n```", buf.join("\n")));
                buf.clear();
                tokens = 0;
            }
            buf.push(line);
            tokens += line_tokens;
        }
        if !buf.is_empty() {
            packer.push_block(format!("{fence}\n{}\n```", buf.join("\n")));
        }
    }
}

// ── Prefix ───────────────────────────────────────────────────────────────────

fn build_prefix(title: &str, path: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !title.is_empty() {
        parts.push(format!("# {title}"));
    }
    parts.extend(path.iter().filter(|p| !p.is_empty()).cloned());
    if parts.is_empty() {
        return String::new();
    }
    format!("{}\n\n", parts.join(" > "))
}

// ── Parsing ──────────────────────────────────────────────────────────────────

enum SegBody {
    Para(String),
    Code(String),
    Table { header: String, rows: Vec<String> },
}

struct Segment {
    path: Vec<String>,
    body: SegBody,
}

struct Group {
    path: Vec<String>,
    segments: Vec<SegBody>,
}

fn group_by_path(segments: Vec<Segment>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for seg in segments {
        match groups.last_mut() {
            Some(g) if g.path == seg.path => g.segments.push(seg.body),
            _ => groups.push(Group {
                path: seg.path,
                segments: vec![seg.body],
            }),
        }
    }
    groups
}

fn heading_level(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    if line[hashes..].trim().is_empty() {
        return None;
    }
    Some((hashes, line.trim().to_string()))
}

/// A table separator row: `|---|:--:|`.
fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn is_table_row(line: &str) -> bool {
    line.trim().starts_with('|')
}

fn parse(body: &str) -> Vec<Segment> {
    let lines: Vec<&str> = body.lines().collect();
    let mut segments = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut i = 0;

    macro_rules! flush_para {
        () => {
            if !para.is_empty() {
                let text = para.join("\n").trim().to_string();
                if !text.is_empty() {
                    segments.push(Segment {
                        path: path.clone(),
                        body: SegBody::Para(text),
                    });
                }
                para.clear();
            }
        };
    }

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            flush_para!();
            let mut code = vec![line];
            i += 1;
            while i < lines.len() {
                code.push(lines[i]);
                let closing = lines[i].trim_start().starts_with("```");
                i += 1;
                if closing {
                    break;
                }
            }
            segments.push(Segment {
                path: path.clone(),
                body: SegBody::Code(code.join("\n")),
            });
            continue;
        }

        if let Some((level, text)) = heading_level(trimmed) {
            flush_para!();
            path.truncate(level.saturating_sub(1));
            while path.len() + 1 < level {
                path.push(String::new());
            }
            path.push(text);
            i += 1;
            continue;
        }

        // A header row followed by a separator row opens a table.
        if is_table_row(line) && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            flush_para!();
            let header = format!("{}\n{}", line.trim(), lines[i + 1].trim());
            i += 2;
            let mut rows = Vec::new();
            while i < lines.len() && is_table_row(lines[i]) {
                rows.push(lines[i].trim().to_string());
                i += 1;
            }
            segments.push(Segment {
                path: path.clone(),
                body: SegBody::Table { header, rows },
            });
            continue;
        }

        if line.trim().is_empty() {
            flush_para!();
        } else {
            para.push(line);
        }
        i += 1;
    }
    flush_para!();

    segments
}

// ── Sentence splitting ───────────────────────────────────────────────────────

/// Splits on sentence terminators, keeping each terminator with its sentence.
/// Walks `char_indices`, so multi-byte text is never cut mid-codepoint.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();

    for (idx, (byte_pos, ch)) in chars.iter().enumerate() {
        if !matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
            continue;
        }
        // A terminator only when whitespace or end of text follows it.
        let terminates = chars
            .get(idx + 1)
            .is_none_or(|(_, next)| next.is_whitespace());
        if !terminates {
            continue;
        }
        let end = byte_pos + ch.len_utf8();
        let sentence = text[start..end].trim();
        if !sentence.is_empty() {
            out.push(sentence.to_string());
        }
        start = end;
    }

    let tail = text[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    if out.is_empty() {
        out.push(text.trim().to_string());
    }
    out
}

/// Last resort: cut on character boundaries, never inside a codepoint.
fn hard_split(text: &str, budget_tokens: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();

    for ch in text.chars() {
        buf.push(ch);
        // Re-tokenising on every character would be quadratic; the byte estimate
        // only decides *when* it is worth measuring.
        if buf.len() >= budget_tokens * 2 && count_tokens(&buf) >= budget_tokens {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

// ── Packing ──────────────────────────────────────────────────────────────────

struct Piece {
    text: String,
    /// Table rows join with a single newline; prose joins with a blank line.
    tight: bool,
}

struct Packer {
    prefix: String,
    prefix_tokens: usize,
    chunk_tokens: usize,
    overlap_tokens: usize,
    pieces: Vec<Piece>,
    tokens: usize,
    out: Vec<String>,
    /// Repeated at the top of every chunk a table spills into.
    table_header: Option<String>,
}

impl Packer {
    fn new(prefix: String, chunk_tokens: usize, overlap_tokens: usize) -> Self {
        let prefix_tokens = count_tokens(&prefix);
        Self {
            prefix,
            prefix_tokens,
            chunk_tokens,
            overlap_tokens,
            pieces: Vec::new(),
            tokens: 0,
            out: Vec::new(),
            table_header: None,
        }
    }

    /// Tokens left for content once the heading prefix has taken its share.
    fn budget(&self) -> usize {
        self.chunk_tokens.saturating_sub(self.prefix_tokens).max(32)
    }

    fn fits_alone(&self, text: &str) -> bool {
        count_tokens(text) <= self.budget()
    }

    fn push_block(&mut self, text: String) {
        self.push(Piece { text, tight: false });
    }

    fn push_row(&mut self, text: String) {
        self.push(Piece { text, tight: true });
    }

    fn begin_table(&mut self, header: String) {
        self.table_header = Some(header.clone());
        self.push(Piece {
            text: header,
            tight: false,
        });
    }

    fn end_table(&mut self) {
        self.table_header = None;
    }

    fn push(&mut self, piece: Piece) {
        let cost = count_tokens(&piece.text);
        if self.tokens + cost > self.budget() && !self.pieces.is_empty() {
            self.flush();
        }
        self.tokens += cost;
        self.pieces.push(piece);
    }

    fn render(&self) -> String {
        let mut s = self.prefix.clone();
        for (i, p) in self.pieces.iter().enumerate() {
            if i > 0 {
                s.push_str(if p.tight { "\n" } else { "\n\n" });
            }
            s.push_str(&p.text);
        }
        s
    }

    fn flush(&mut self) {
        if self.pieces.is_empty() {
            return;
        }
        self.out.push(self.render());

        // Carry this chunk's tail into the next, so a fact that straddles the
        // boundary is wholly present in at least one of them.
        let mut carried: Vec<Piece> = Vec::new();
        let mut carried_tokens = 0usize;
        if self.overlap_tokens > 0 && self.table_header.is_none() {
            for p in self.pieces.iter().rev() {
                if p.tight {
                    continue; // never carry a naked table row
                }
                let cost = count_tokens(&p.text);
                if carried_tokens + cost > self.overlap_tokens {
                    break;
                }
                carried_tokens += cost;
                carried.insert(
                    0,
                    Piece {
                        text: p.text.clone(),
                        tight: false,
                    },
                );
            }
        }

        self.pieces.clear();
        self.tokens = 0;

        // A table that spills over reopens with its header, not with prose.
        if let Some(header) = self.table_header.clone() {
            self.tokens = count_tokens(&header);
            self.pieces.push(Piece {
                text: header,
                tight: false,
            });
            return;
        }

        for p in carried {
            self.tokens += count_tokens(&p.text);
            self.pieces.push(p);
        }
    }

    fn finish(mut self, chunks: &mut Vec<String>) {
        // A trailing buffer holding only a repeated table header carries no content.
        let only_header = self.pieces.len() == 1
            && self
                .table_header
                .as_ref()
                .is_some_and(|h| *h == self.pieces[0].text);

        chunks.append(&mut self.out);
        if !self.pieces.is_empty() && !only_header {
            chunks.push(self.render());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Chunker {
        Chunker {
            chunk_tokens: 60,
            overlap_tokens: 12,
        }
    }

    #[test]
    fn preserves_code_fence() {
        let body = "intro\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\noutro";
        let chunks = Chunker::default().split("Doc", body);
        let code = chunks
            .iter()
            .find(|c| c.contains("fn main()"))
            .expect("code survives");
        assert_eq!(code.matches("```").count() % 2, 0, "fences stay balanced");
    }

    #[test]
    fn every_chunk_carries_the_document_title() {
        let chunks = Chunker::default().split("Guide", "Hello world");
        assert!(chunks[0].starts_with("# Guide"));
    }

    #[test]
    fn a_chunk_carries_its_full_heading_path() {
        let body = "## Auth\n\n### Rotation\n\nRotate the key every 90 days.\n";
        let chunks = Chunker::default().split("Guide", body);
        let chunk = chunks.iter().find(|c| c.contains("90 days")).unwrap();

        assert!(
            chunk.starts_with("# Guide > ## Auth > ### Rotation"),
            "without the path, 'rotate the key every 90 days' names nothing: {chunk}"
        );
    }

    #[test]
    fn separate_headings_do_not_share_a_chunk() {
        let body = "## Refunds\n\nThirty days.\n\n## Shipping\n\nFive days.\n";
        let chunks = Chunker::default().split("Policy", body);

        assert!(
            !chunks
                .iter()
                .any(|c| c.contains("Thirty") && c.contains("Five")),
            "a chunk's prefix can only name one heading"
        );
        assert!(
            chunks
                .iter()
                .any(|c| c.contains("Refunds") && c.contains("Thirty"))
        );
        assert!(
            chunks
                .iter()
                .any(|c| c.contains("Shipping") && c.contains("Five"))
        );
    }

    #[test]
    fn a_split_table_repeats_its_header_in_every_chunk() {
        let mut body = String::from("| region | days |\n|---|---|\n");
        for i in 0..80 {
            body.push_str(&format!("| region-{i} | {i} |\n"));
        }

        let chunks = small().split("Policy", &body);
        assert!(chunks.len() > 1, "the table must actually have split");

        for chunk in &chunks {
            assert!(
                chunk.contains("| region | days |"),
                "`| EU | 30 |` means nothing without its header:\n{chunk}"
            );
        }
    }

    #[test]
    fn a_long_paragraph_is_split_on_sentence_boundaries() {
        let body = "The refund window for European customers is thirty calendar days. ".repeat(20);
        let chunks = small().split("Policy", &body);

        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                chunk.trim_end().ends_with('.'),
                "chunks should end at a sentence, not mid-clause: {chunk:?}"
            );
        }
    }

    /// The last sentence a chunk contains, as the packer laid it out.
    fn tail_sentence(chunk: &str) -> &str {
        chunk.trim().rsplit("\n\n").next().unwrap().trim()
    }

    #[test]
    fn overlap_carries_the_tail_of_the_previous_chunk() {
        let body = (0..20)
            .map(|i| format!("Fact number {i} is written here plainly."))
            .collect::<Vec<_>>()
            .join(" ");

        let chunks = small().split("Facts", &body);
        assert!(chunks.len() > 1);

        let tail = tail_sentence(&chunks[0]);
        assert!(
            chunks[1].contains(tail),
            "the boundary sentence must appear in both chunks:\ntail = {tail:?}\nsecond = {:?}",
            chunks[1]
        );
    }

    #[test]
    fn overlap_is_configurable_and_zero_means_zero() {
        let body = (0..20)
            .map(|i| format!("Fact number {i} is written here plainly."))
            .collect::<Vec<_>>()
            .join(" ");

        let none = Chunker {
            chunk_tokens: 60,
            overlap_tokens: 0,
        }
        .split("Facts", &body);
        let some = small().split("Facts", &body);

        let total = |v: &Vec<String>| v.iter().map(|c| c.len()).sum::<usize>();
        assert!(
            total(&some) > total(&none),
            "overlap_tokens was ignored: the config had no effect"
        );
    }

    #[test]
    fn non_ascii_text_keeps_its_overlap_and_is_never_cut_mid_character() {
        let body = (0..20)
            .map(|i| format!("Chính sách hoàn tiền số {i} áp dụng cho khách hàng châu Âu."))
            .collect::<Vec<_>>()
            .join(" ");

        // A Vietnamese sentence costs far more tokens than its English twin, so the
        // budget is scaled to hold one. Slicing by byte index, as the old code did,
        // landed mid-codepoint and dropped the overlap entirely — silently, and only
        // for non-ASCII documents.
        let chunker = Chunker {
            chunk_tokens: 140,
            overlap_tokens: 40,
        };
        let chunks = chunker.split("Chinh sach", &body);
        assert!(chunks.len() > 1);

        let tail = tail_sentence(&chunks[0]);
        assert!(
            tail.contains("Chính sách hoàn tiền"),
            "multi-byte text survived intact: {tail:?}"
        );
        assert!(chunks[1].contains(tail), "the overlap carried");
    }

    #[test]
    fn an_overlap_too_small_for_the_trailing_sentence_carries_nothing() {
        let body = (0..12)
            .map(|i| format!("Fact number {i} is written here at some length, plainly."))
            .collect::<Vec<_>>()
            .join(" ");

        let chunks = Chunker {
            chunk_tokens: 60,
            overlap_tokens: 2,
        }
        .split("Facts", &body);

        assert!(chunks.len() > 1);
        let tail = tail_sentence(&chunks[0]);
        assert!(
            !chunks[1].contains(tail),
            "half a sentence is worse than no overlap: it retrieves as a fragment"
        );
    }

    #[test]
    fn a_single_sentence_larger_than_the_budget_is_still_emitted_whole() {
        let body = "x".repeat(4000);
        let chunks = small().split("Doc", &body);
        assert!(chunks.len() > 1);
        assert_eq!(
            chunks.iter().map(|c| c.matches('x').count()).sum::<usize>(),
            4000,
            "nothing may be dropped"
        );
    }

    #[test]
    fn an_empty_body_yields_no_chunks() {
        assert!(Chunker::default().split("Doc", "   \n\n ").is_empty());
    }
}
