use super::chat::FinishReason;
use super::usage::Usage;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Start {
        id: String,
        model: String,
        created: i64,
    },
    Delta {
        content: String,
    },
    ReasoningDelta {
        content: String,
    },
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Finish {
        reason: FinishReason,
    },
    Usage(Usage),
}
