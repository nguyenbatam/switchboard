use crate::domain::chat::{ChatRequest, ChatResponse};
use crate::domain::embed::{EmbedRequest, EmbedResponse};
use crate::domain::stream::StreamEvent;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Caps {
    pub tools: bool,
    pub parallel_tool_calls: bool,
    pub json_schema: bool,
    pub vision: bool,
    pub streaming_usage: bool,
    pub max_context: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            tools: true,
            parallel_tool_calls: true,
            json_schema: false,
            vision: false,
            streaming_usage: false,
            max_context: 32768,
        }
    }
}

/// The instant after which nobody wants the answer.
///
/// Held as a monotonic `Instant`, never a wall-clock time: the client sends a *relative*
/// millisecond budget precisely because our clocks disagree, and converting it to an
/// absolute timestamp here would reintroduce the skew the relative header exists to
/// avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn in_ms(ms: u64) -> Self {
        Self(Instant::now() + Duration::from_millis(ms))
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.0
    }

    /// What is left, or `None` if it has already run out.
    pub fn remaining(&self) -> Option<Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    }
}

#[derive(Debug, Clone)]
pub struct CallCx {
    pub request_id: String,
    pub cancel: CancellationToken,
    /// Checked immediately before a provider is called. A request whose deadline has
    /// expired must not reach the model: that is real tokens and real money spent on an
    /// answer nobody is still waiting for.
    pub deadline: Option<Deadline>,
}

impl CallCx {
    /// A context with no deadline, for internal callers with no client attached.
    pub fn detached(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            cancel: CancellationToken::new(),
            deadline: None,
        }
    }

    /// `Err(Cancelled)` if there is no longer any point starting the work.
    pub fn check_deadline(&self) -> Result<(), ProviderError> {
        match &self.deadline {
            Some(d) if d.expired() => Err(ProviderError::DeadlineExceeded),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    /// The upstream is **broken**: its circuit breaker is open, or it answered `503`/`529`.
    #[error("upstream overloaded")]
    Overloaded,
    /// The upstream is **healthy but full**: every one of its in-flight permits is taken.
    ///
    /// Kept distinct from [`Self::Overloaded`] because the two license different actions.
    /// Busy means try the fallback — that provider is fine, we are simply at its capacity.
    /// Broken means do not call it at all. Collapsing them into one variant is how a
    /// gateway ends up either hammering a dead provider or refusing a healthy one.
    #[error("provider at capacity")]
    Busy,
    #[error("timeout: {0:?}")]
    Timeout(TimeoutKind),
    #[error("upstream 5xx: {status}")]
    Upstream5xx { status: u16 },
    #[error("upstream auth error")]
    Auth,
    #[error("bad request")]
    BadRequest { param: Option<String> },
    #[error("context length {got} > {limit}")]
    ContextLength { limit: u32, got: u32 },
    #[error("content filtered")]
    ContentFilter,
    #[error("cancelled")]
    Cancelled,
    /// The client's deadline ran out before the work could start or finish. Distinct
    /// from [`Self::Timeout`], which is the *upstream* failing to answer in time: this
    /// one is not the provider's fault and must never open its circuit breaker.
    #[error("deadline exceeded")]
    DeadlineExceeded,
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy)]
pub enum TimeoutKind {
    Connect,
    Ttft,
    Stall,
    Total,
}

#[async_trait]
pub trait ModelProvider: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn capabilities(&self, model: &str) -> Caps;
    async fn chat(
        &self,
        model: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<ChatResponse, ProviderError>;
    async fn stream(
        &self,
        model: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
    async fn embed(
        &self,
        model: &str,
        req: &EmbedRequest,
        cx: &CallCx,
    ) -> Result<EmbedResponse, ProviderError>;
    async fn healthy(&self) -> bool;
}

pub mod ollama;
pub mod openai;
pub mod registry;
pub mod timeouts;
