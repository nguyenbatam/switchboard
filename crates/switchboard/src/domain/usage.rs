use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl Usage {
    pub fn total(&self) -> u32 {
        self.prompt_tokens + self.completion_tokens
    }

    pub fn with_total(mut self) -> Self {
        self.total_tokens = self.total();
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    Completed,
    ClientDisconnect,
    Error,
}

impl RecordStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ClientDisconnect => "client_disconnect",
            Self::Error => "error",
        }
    }
}
