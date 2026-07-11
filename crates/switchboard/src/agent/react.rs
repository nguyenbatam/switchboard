use crate::agent::budget::{Budget, BudgetError};
use crate::agent::invocations::{InvocationRecorder, ToolInvocationRecord};
use crate::agent::registry::{ToolCallCtx, ToolRegistry};
use crate::agent::taint::{TaintState, Trust};
use crate::domain::chat::{
    ChatRequest, ChatResponse, FinishReason, Message, MessageContent, Role, ToolCall,
};
use crate::domain::usage::Usage;
use crate::providers::registry::ProviderRegistry;
use crate::providers::{CallCx, ProviderError};
use serde_json::json;
use std::time::Duration;
use thiserror::Error;
use tokio::task::JoinSet;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationReason {
    MaxIterations,
    MaxTokens,
    Deadline,
}

#[derive(Debug)]
pub struct AgentOutcome {
    pub response: ChatResponse,
    pub iterations: u32,
    pub truncation: Option<TruncationReason>,
    /// Time until the first provider turn came back. An agent run's user-visible
    /// latency is dominated by tool calls, so its TTFT is not its total latency.
    pub ttft_ms: Option<i32>,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("fatal tool error: {0}")]
    FatalTool(String),
    #[error("cancelled")]
    Cancelled,
}

pub struct AgentLoop {
    pub max_iterations: u32,
    pub max_total_tokens: u32,
    pub request_timeout: Duration,
    pub tools: ToolRegistry,
}

impl AgentLoop {
    pub fn new(
        max_iterations: u32,
        max_total_tokens: u32,
        request_timeout: Duration,
        tools: ToolRegistry,
    ) -> Self {
        Self {
            max_iterations,
            max_total_tokens,
            request_timeout,
            tools,
        }
    }

    pub async fn run(
        &self,
        registry: &ProviderRegistry,
        model_alias: &str,
        mut req: ChatRequest,
        cx: &CallCx,
        recorder: Option<&InvocationRecorder>,
        tenant_id: Option<Uuid>,
    ) -> Result<AgentOutcome, AgentError> {
        req.tools = Some(self.tools.openai_tools());
        req.stream = false;

        let mut budget = Budget::new(
            self.max_iterations,
            self.max_total_tokens,
            self.request_timeout,
        );
        // Retrieved context is untrusted the moment it enters `messages`. Starting
        // the loop tainted is what stops a poisoned document from reaching a tool
        // that can exfiltrate or mutate.
        let mut taint = TaintState::default();
        if req.untrusted_context {
            taint.mark_untrusted();
        }
        let mut total_usage = Usage::default();
        let mut last_response: Option<ChatResponse> = None;
        let mut truncation = None;
        let loop_started = std::time::Instant::now();
        let mut ttft_ms: Option<i32> = None;

        loop {
            if cx.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if let Err(e) = budget.tick() {
                truncation = Some(match e {
                    BudgetError::MaxIterations { .. } => TruncationReason::MaxIterations,
                    BudgetError::MaxTokens { .. } => TruncationReason::MaxTokens,
                    BudgetError::Deadline => TruncationReason::Deadline,
                });
                break;
            }

            let resp = registry.chat(model_alias, &req, cx).await?;
            ttft_ms.get_or_insert_with(|| loop_started.elapsed().as_millis() as i32);
            accumulate_usage(&mut total_usage, &resp.usage);
            if let Err(e) = budget.add_tokens(resp.usage.total()) {
                truncation = Some(match e {
                    BudgetError::MaxTokens { .. } => TruncationReason::MaxTokens,
                    _ => TruncationReason::MaxIterations,
                });
                last_response = Some(resp);
                break;
            }

            let choice = resp
                .choices
                .first()
                .ok_or_else(|| AgentError::FatalTool("empty provider response".into()))?;

            if choice.finish_reason == Some(FinishReason::ToolCalls) {
                let tool_calls = choice.message.tool_calls.clone().unwrap_or_default();
                if tool_calls.is_empty() {
                    last_response = Some(resp);
                    break;
                }

                let iteration = budget.iterations();
                let mut batch = ToolBatch {
                    tool_cx: ToolCallCtx { tenant_id },
                    registry: &self.tools,
                    taint: &mut taint,
                    cx,
                    audit: InvocationAudit {
                        recorder,
                        tenant_id,
                        request_id: &cx.request_id,
                        iteration,
                    },
                };
                let results = batch.run(&tool_calls).await?;

                req.messages.push(choice.message.clone());
                for (call, result) in tool_calls.iter().zip(results.iter()) {
                    req.messages.push(tool_message(call, result));
                }
                last_response = None;
                continue;
            }

            last_response = Some(resp);
            break;
        }

        let mut response = last_response
            .ok_or_else(|| AgentError::FatalTool("agent loop produced no response".into()))?;

        if let Some(reason) = truncation {
            response = truncate_response(response, reason);
        }

        response.usage = total_usage.with_total();
        Ok(AgentOutcome {
            iterations: budget.iterations(),
            truncation,
            ttft_ms,
            response,
        })
    }
}

struct InvocationAudit<'a> {
    recorder: Option<&'a InvocationRecorder>,
    tenant_id: Option<Uuid>,
    request_id: &'a str,
    iteration: u32,
}

impl InvocationAudit<'_> {
    fn record(&self, call: &ToolCall, entry: InvocationEntry) {
        let Some(rec) = self.recorder else {
            return;
        };
        rec.record(ToolInvocationRecord {
            request_id: self.request_id.into(),
            tenant_id: self.tenant_id,
            iteration: self.iteration,
            tool_name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
            result: entry.result,
            trust: entry.trust,
            latency_ms: entry.latency_ms,
            status: entry.status,
            error: entry.error,
        });
    }
}

struct InvocationEntry {
    result: Option<String>,
    trust: Trust,
    latency_ms: i32,
    status: String,
    error: Option<String>,
}

struct ToolBatch<'a> {
    registry: &'a ToolRegistry,
    tool_cx: ToolCallCtx,
    taint: &'a mut TaintState,
    cx: &'a CallCx,
    audit: InvocationAudit<'a>,
}

impl ToolBatch<'_> {
    async fn run(&mut self, calls: &[ToolCall]) -> Result<Vec<String>, AgentError> {
        let registry = self.registry.clone();
        let mut set = JoinSet::new();
        let mut precomputed: Vec<(usize, Option<ToolCallOutcome>)> =
            Vec::with_capacity(calls.len());

        for (idx, call) in calls.iter().enumerate() {
            if let Some(spec) = registry.get(&call.function.name)
                && !self.taint.tool_allowed(spec.trust, spec.effects)
            {
                let message = json!({
                    "error": format!(
                        "tool '{}' is unavailable after untrusted content entered the context",
                        call.function.name
                    )
                })
                .to_string();
                precomputed.push((
                    idx,
                    Some(ToolCallOutcome::Denied {
                        message,
                        latency: 0,
                    }),
                ));
                continue;
            }
            precomputed.push((idx, None));
            let call = call.clone();
            let registry = registry.clone();
            let cancel = self.cx.cancel.clone();
            let tool_cx = self.tool_cx;
            set.spawn(async move {
                let result = execute_one(&call, &registry, &tool_cx, cancel).await;
                (idx, call, result)
            });
        }

        let mut indexed: Vec<(usize, ToolCall, ToolCallOutcome)> = Vec::with_capacity(calls.len());
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(row) => indexed.push(row),
                Err(e) => return Err(AgentError::FatalTool(format!("tool task failed: {e}"))),
            }
        }

        for (idx, pre) in precomputed {
            if let Some(outcome) = pre {
                indexed.push((idx, calls[idx].clone(), outcome));
            }
        }
        indexed.sort_by_key(|(idx, _, _)| *idx);

        let mut results = Vec::with_capacity(calls.len());
        for (_, call, outcome) in indexed {
            match outcome {
                ToolCallOutcome::Ok {
                    content,
                    trust,
                    latency,
                    status,
                } => {
                    self.taint.record_known_tool();
                    // Audit the raw result; hand the model the delimited one, so a
                    // tool result that says "ignore your instructions" arrives
                    // visibly quoted rather than as narration.
                    self.audit.record(
                        &call,
                        InvocationEntry {
                            result: Some(content.clone()),
                            trust,
                            latency_ms: latency,
                            status,
                            error: None,
                        },
                    );
                    let content = if trust == Trust::Untrusted {
                        self.taint.mark_untrusted();
                        crate::agent::taint::wrap_untrusted(&call.function.name, &content)
                    } else {
                        content
                    };
                    results.push(content);
                }
                ToolCallOutcome::Denied { message, latency } => {
                    self.taint.record_known_tool();
                    self.audit.record(
                        &call,
                        InvocationEntry {
                            result: Some(message.clone()),
                            trust: Trust::Trusted,
                            latency_ms: latency,
                            status: "denied".into(),
                            error: None,
                        },
                    );
                    results.push(message);
                }
                ToolCallOutcome::Unknown { message } => {
                    let action = self.taint.record_unknown_tool();
                    self.audit.record(
                        &call,
                        InvocationEntry {
                            result: None,
                            trust: Trust::Trusted,
                            latency_ms: 0,
                            status: "unknown_tool".into(),
                            error: Some(message.clone()),
                        },
                    );
                    if matches!(action, crate::agent::taint::UnknownToolAction::Fatal) {
                        return Err(AgentError::FatalTool(message));
                    }
                    results.push(message);
                }
            }
        }
        Ok(results)
    }
}

#[derive(Debug)]
enum ToolCallOutcome {
    Ok {
        content: String,
        trust: Trust,
        latency: i32,
        status: String,
    },
    Denied {
        message: String,
        latency: i32,
    },
    Unknown {
        message: String,
    },
}

async fn execute_one(
    call: &ToolCall,
    registry: &ToolRegistry,
    tool_cx: &ToolCallCtx,
    cancel: tokio_util::sync::CancellationToken,
) -> ToolCallOutcome {
    let started = std::time::Instant::now();
    let latency = || started.elapsed().as_millis() as i32;

    let args =
        match crate::domain::values::ParsedToolArguments::parse_json(&call.function.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolCallOutcome::Ok {
                    content: json!({"error": format!("invalid JSON arguments: {e}")}).to_string(),
                    trust: Trust::Trusted,
                    latency: latency(),
                    status: "invalid_args".into(),
                };
            }
        };

    let Some(spec) = registry.get(&call.function.name) else {
        let available = registry.names().join(", ");
        let message = json!({
            "error": format!("unknown tool '{}'. Available: [{available}]", call.function.name)
        })
        .to_string();
        return ToolCallOutcome::Unknown { message };
    };

    if cancel.is_cancelled() {
        return ToolCallOutcome::Ok {
            content: json!({"error":"cancelled"}).to_string(),
            trust: spec.trust,
            latency: latency(),
            status: "error".into(),
        };
    }

    if let Some(err) = spec.validate_args(&args) {
        return ToolCallOutcome::Ok {
            content: json!({"error": err}).to_string(),
            trust: spec.trust,
            latency: latency(),
            status: "invalid_args".into(),
        };
    }

    // Two bugs lived in the four lines this replaces.
    //
    // `tokio::time::timeout(t, async { spec.run_blocking(..) })` can never fire. The async
    // block reaches no `.await` before `run_blocking` returns, so the future is already
    // complete the first time the timer polls it. A tool that wedged ignored its deadline
    // entirely, and the whole agent loop hung behind it.
    //
    // And `run_blocking` ran on a runtime worker. Each parallel tool call is `set.spawn`ed,
    // so a fan-out of tool calls blocked one worker thread per call — the same failure as
    // an uncached argon2 verify, arriving by a different door.
    //
    // `spawn_blocking` fixes both: the work leaves the async workers, and the timeout now
    // races a real `JoinHandle` that actually yields. The blocking thread itself cannot be
    // interrupted — nothing in Rust can preempt a synchronous call — so a wedged tool still
    // occupies a blocking-pool thread after we stop waiting for it. What changes is that
    // the request no longer waits with it.
    let timeout = spec.timeout;
    let runner = spec.clone();
    let ctx = *tool_cx;
    let owned_args = args.clone();
    let handle = tokio::task::spawn_blocking(move || runner.run_blocking(&ctx, &owned_args));

    let result = tokio::select! {
        _ = cancel.cancelled() => {
            return ToolCallOutcome::Ok {
                content: json!({"error":"cancelled"}).to_string(),
                trust: spec.trust,
                latency: latency(),
                status: "error".into(),
            };
        }
        r = tokio::time::timeout(timeout, handle) => r,
    };

    match result {
        Ok(Ok(Ok(content))) => ToolCallOutcome::Ok {
            content,
            trust: spec.trust,
            latency: latency(),
            status: "ok".into(),
        },
        Ok(Ok(Err(e))) => ToolCallOutcome::Ok {
            content: json!({"error": e}).to_string(),
            trust: spec.trust,
            latency: latency(),
            status: "error".into(),
        },
        // The tool panicked. `CatchPanicLayer` is upstream of us and would turn this into
        // a dead request; a tool is allowed to fail without taking the turn with it.
        Ok(Err(join_err)) => ToolCallOutcome::Ok {
            content: json!({"error": format!("tool panicked: {join_err}")}).to_string(),
            trust: spec.trust,
            latency: latency(),
            status: "error".into(),
        },
        Err(_) => ToolCallOutcome::Ok {
            content: json!({"error":"tool timeout"}).to_string(),
            trust: spec.trust,
            latency: latency(),
            status: "timeout".into(),
        },
    }
}

fn tool_message(call: &ToolCall, content: &str) -> Message {
    Message {
        role: Role::Tool,
        content: Some(MessageContent::Text(content.into())),
        name: None,
        tool_calls: None,
        tool_call_id: Some(call.id.clone()),
    }
}

fn accumulate_usage(total: &mut Usage, u: &Usage) {
    total.prompt_tokens += u.prompt_tokens;
    total.completion_tokens += u.completion_tokens;
    total.cached_tokens += u.cached_tokens;
    total.reasoning_tokens += u.reasoning_tokens;
}

fn truncate_response(mut resp: ChatResponse, reason: TruncationReason) -> ChatResponse {
    if let Some(choice) = resp.choices.first_mut() {
        choice.finish_reason = Some(FinishReason::Length);
        if choice.message.content.is_none() {
            let msg = match reason {
                TruncationReason::MaxIterations => {
                    "Response truncated: maximum agent iterations reached."
                }
                TruncationReason::MaxTokens => "Response truncated: maximum total tokens reached.",
                TruncationReason::Deadline => "Response truncated: request deadline exceeded.",
            };
            choice.message.content = Some(MessageContent::Text(msg.into()));
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::registry::{ToolCallCtx, ToolRegistry, ToolSpec};
    use crate::agent::taint::Effects;
    use crate::domain::chat::FunctionCall;
    use crate::domain::schema::ToolParameters;
    use std::collections::BTreeMap;
    use tokio_util::sync::CancellationToken;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    fn registry_of(spec: ToolSpec) -> ToolRegistry {
        // A ceiling far above the spec's own deadline, so the spec's deadline is what runs.
        ToolRegistry::build_from_specs(vec![spec], Duration::from_secs(60))
    }

    fn spec(
        name: &'static str,
        timeout: Duration,
        run: impl Fn(
            &ToolCallCtx,
            &crate::domain::values::ParsedToolArguments,
        ) -> Result<String, String>
        + Send
        + Sync
        + 'static,
    ) -> ToolSpec {
        ToolSpec::new(
            name,
            "test",
            ToolParameters::object(BTreeMap::new(), vec![], true),
            Trust::Trusted,
            Effects::Pure,
            timeout,
            run,
        )
    }

    /// The bug this rewrite exists to fix.
    ///
    /// `tokio::time::timeout(t, async { spec.run_blocking(..) })` can never fire: the async
    /// block reaches no `.await` before the synchronous call returns, so the future is
    /// already complete the first time the timer polls it. A wedged tool ignored its
    /// deadline and hung the agent loop behind it.
    ///
    /// This test fails, by hanging for ten seconds, against that implementation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wedged_tool_actually_times_out() {
        // The sleep outlives the timeout by an order of magnitude and cannot be interrupted —
        // nothing in Rust preempts a synchronous call. Two seconds is enough to prove the
        // timeout fired without making every CI run wait for the thread to finish.
        let registry = registry_of(spec("wedged", Duration::from_millis(100), |_, _| {
            std::thread::sleep(Duration::from_secs(2));
            Ok("never".into())
        }));

        let started = std::time::Instant::now();
        let outcome = execute_one(
            &call("wedged"),
            &registry,
            &ToolCallCtx::default(),
            CancellationToken::new(),
        )
        .await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the timeout never fired: waited {:?}",
            started.elapsed()
        );
        match outcome {
            ToolCallOutcome::Ok { status, .. } => assert_eq!(status, "timeout"),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// `sql_query`, `mcp` and `wallet_balance` all reach an async runtime from inside a
    /// synchronous tool body: `block_in_place` + `Handle::current().block_on(..)`.
    ///
    /// Moving `run_blocking` onto `spawn_blocking` moved that construct off a runtime worker
    /// thread and onto a blocking-pool thread, where `block_in_place` becomes a passthrough
    /// rather than a panic. That is a property of tokio's internals, not of our code, and it
    /// is load-bearing for three tools. Pin it here, so a tokio upgrade that changes it fails
    /// in CI rather than the first time somebody runs an allowlisted query in production.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_may_re_enter_the_runtime_from_the_blocking_pool() {
        let registry = registry_of(spec("reentrant", Duration::from_secs(5), |_, _| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    Ok::<_, String>("awaited".to_string())
                })
            })
        }));

        let outcome = execute_one(
            &call("reentrant"),
            &registry,
            &ToolCallCtx::default(),
            CancellationToken::new(),
        )
        .await;

        match outcome {
            ToolCallOutcome::Ok {
                content, status, ..
            } => {
                assert_eq!(status, "ok");
                assert_eq!(content, "awaited");
            }
            other => panic!("the blocking pool could not drive a future: {other:?}"),
        }
    }

    /// A tool that panics must fail its own call, not the request. `CatchPanicLayer` sits
    /// above us and would turn an escaped panic into a dead response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_tool_fails_only_itself() {
        let registry = registry_of(spec("boom", Duration::from_secs(5), |_, _| {
            panic!("the tool exploded")
        }));

        let outcome = execute_one(
            &call("boom"),
            &registry,
            &ToolCallCtx::default(),
            CancellationToken::new(),
        )
        .await;

        match outcome {
            ToolCallOutcome::Ok {
                status, content, ..
            } => {
                assert_eq!(status, "error");
                assert!(content.contains("panicked"), "{content}");
            }
            other => panic!("expected a contained failure, got {other:?}"),
        }
    }
}
