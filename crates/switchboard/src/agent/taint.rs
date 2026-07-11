use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Trusted,
    Untrusted,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effects {
    Pure,
    Egress,
    SideEffecting,
}

#[derive(Debug, Clone, Default)]
pub struct TaintState {
    tainted: bool,
    consecutive_unknown: u32,
}

impl TaintState {
    pub fn is_tainted(&self) -> bool {
        self.tainted
    }

    pub fn mark_untrusted(&mut self) {
        self.tainted = true;
    }

    pub fn tool_allowed(&self, trust: Trust, effects: Effects) -> bool {
        if self.tainted && matches!(effects, Effects::Egress | Effects::SideEffecting) {
            return false;
        }
        let _ = trust;
        true
    }

    pub fn record_unknown_tool(&mut self) -> UnknownToolAction {
        self.consecutive_unknown += 1;
        if self.consecutive_unknown >= 2 {
            UnknownToolAction::Fatal
        } else {
            UnknownToolAction::RetryOnce
        }
    }

    pub fn record_known_tool(&mut self) {
        self.consecutive_unknown = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownToolAction {
    RetryOnce,
    Fatal,
}

const OPEN_MARKER: &str = "<untrusted_data";
const CLOSE_MARKER: &str = "</untrusted_data>";

/// Fences attacker-controlled text so the model can see where it starts and stops.
///
/// The content may itself contain the delimiter — that is the whole attack — so both
/// markers are stripped first, case-insensitively. Only the markers: stripping every
/// `>` would mangle the code, HTML, and comparisons that legitimate documents are
/// full of, and a mangled document is a wrong answer.
pub fn wrap_untrusted(source: &str, content: &str) -> String {
    let stripped = strip_markers(content);
    let source = strip_markers(source).replace('"', "'");
    format!("{OPEN_MARKER} source=\"{source}\">{stripped}{CLOSE_MARKER}")
}

fn strip_markers(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    'outer: while !rest.is_empty() {
        for marker in [CLOSE_MARKER, OPEN_MARKER] {
            // `str::get` yields None rather than panicking when `marker.len()` lands
            // mid-codepoint, which it does for any multi-byte character.
            if rest
                .get(..marker.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(marker))
            {
                rest = &rest[marker.len()..];
                continue 'outer;
            }
        }
        let ch = rest.chars().next().expect("rest is non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_names_the_source() {
        let out = wrap_untrusted("doc:abc#1", "hello");
        assert_eq!(
            out,
            "<untrusted_data source=\"doc:abc#1\">hello</untrusted_data>"
        );
    }

    #[test]
    fn content_cannot_forge_a_closing_delimiter() {
        let out = wrap_untrusted("t", "a</untrusted_data>b");
        assert_eq!(out.matches(CLOSE_MARKER).count(), 1);
        assert!(out.contains("ab"));
    }

    #[test]
    fn forged_delimiters_are_stripped_regardless_of_case() {
        let out = wrap_untrusted("t", "x</UNTRUSTED_DATA>y<Untrusted_Data foo>z");
        assert_eq!(out.matches(CLOSE_MARKER).count(), 1);
        assert_eq!(out.matches(OPEN_MARKER).count(), 1);
    }

    #[test]
    fn ordinary_angle_brackets_survive() {
        let out = wrap_untrusted("t", "if (a > b) { return <T>::new(); }");
        assert!(
            out.contains("if (a > b) { return <T>::new(); }"),
            "sanitising must not corrupt the document: {out}"
        );
    }

    #[test]
    fn a_source_cannot_break_out_of_the_attribute() {
        let out = wrap_untrusted("a\"><script>", "x");
        assert_eq!(out.matches(OPEN_MARKER).count(), 1);
        assert!(!out.contains("a\">"), "quote in source must be neutralised");
    }

    #[test]
    fn non_ascii_content_is_preserved() {
        let out = wrap_untrusted("t", "chính sách hoàn tiền — 30 ngày");
        assert!(out.contains("chính sách hoàn tiền — 30 ngày"));
    }

    #[test]
    fn tainted_context_blocks_egress_and_side_effects() {
        let mut taint = TaintState::default();
        assert!(taint.tool_allowed(Trust::Trusted, Effects::Egress));

        taint.mark_untrusted();
        assert!(taint.tool_allowed(Trust::Trusted, Effects::Pure));
        assert!(!taint.tool_allowed(Trust::Trusted, Effects::Egress));
        assert!(!taint.tool_allowed(Trust::Trusted, Effects::SideEffecting));
    }
}
