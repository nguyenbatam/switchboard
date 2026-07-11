use crate::agent::taint::{Effects, Trust};
use crate::domain::chat::Tool;
use crate::domain::schema::ToolParameters;
use crate::domain::values::ParsedToolArguments;
use jsonschema::Validator;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

type ToolRunner =
    Arc<dyn Fn(&ToolCallCtx, &ParsedToolArguments) -> Result<String, String> + Send + Sync>;

/// Per-invocation context handed to every tool.
///
/// `tenant_id` is what lets a tool scope itself to the caller. `sql_query` sets it as
/// `app.tenant_id` so row-level security enforces isolation in the database, rather
/// than trusting a hand-written `WHERE` clause to be present in every query.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCallCtx {
    pub tenant_id: Option<uuid::Uuid>,
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: ToolParameters,
    pub trust: Trust,
    pub effects: Effects,
    pub timeout: Duration,
    run: ToolRunner,
    validator: Arc<Validator>,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: ToolParameters,
        trust: Trust,
        effects: Effects,
        timeout: Duration,
        run: impl Fn(&ToolCallCtx, &ParsedToolArguments) -> Result<String, String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let schema = serde_json::to_value(&parameters).expect("tool schema serialises");
        let validator = Validator::new(&schema).expect("valid tool schema");
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            trust,
            effects,
            timeout,
            run: Arc::new(run),
            validator: Arc::new(validator),
        }
    }

    pub fn openai_tool(&self) -> Tool {
        Tool {
            r#type: "function".into(),
            function: crate::domain::chat::ToolFunction {
                name: self.name.clone(),
                description: Some(self.description.clone()),
                parameters: self.parameters.clone(),
            },
        }
    }

    pub fn validate_args(&self, args: &ParsedToolArguments) -> Option<String> {
        let value: Value = serde_json::to_value(args).ok()?;
        let errors: Vec<String> = self
            .validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            None
        } else {
            let msg = errors.join("; ");
            Some(truncate(&msg, 512))
        }
    }

    pub fn run_blocking(
        &self,
        ctx: &ToolCallCtx,
        args: &ParsedToolArguments,
    ) -> Result<String, String> {
        (self.run)(ctx, args)
    }
}

#[derive(Clone, Default)]
pub struct ToolContext {
    pub tool_ro: Option<PgPool>,
    pub sql_allowed_tables: Vec<String>,
    pub wallet: Option<WalletToolConfig>,
}

#[derive(Clone)]
pub struct WalletToolConfig {
    pub rpc_urls: Vec<String>,
    pub max_block_drift: u64,
    pub timeout: Duration,
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolSpec>,
}

impl ToolRegistry {
    /// `tool_timeout` is a **ceiling**, not an override.
    ///
    /// Each tool already carries a deadline chosen for what it does: a second for the
    /// calculator, two for `sql_query`, whatever its config says for an MCP server. Those
    /// are better numbers than one global figure could be, so the global figure only
    /// clamps. It bounds how long *any* tool may hold an agent turn and leaves a tighter
    /// local choice tight.
    ///
    /// `agent.tool_timeout_ms` was declared in the config and read nowhere, while the
    /// tools silently borrowed `server.request_timeout_ms` instead. A config key that lies
    /// is worse than one that is missing.
    pub fn build(ctx: &ToolContext, extra: Vec<ToolSpec>, tool_timeout: Duration) -> Self {
        let mut tools = HashMap::new();

        let capped = |tools: &mut HashMap<String, ToolSpec>, mut spec: ToolSpec| {
            spec.timeout = spec.timeout.min(tool_timeout);
            register(tools, spec);
        };

        capped(&mut tools, crate::agent::tools::calculator::build());
        capped(&mut tools, crate::agent::tools::sql_query::build(ctx));
        if let Some(cfg) = &ctx.wallet {
            capped(&mut tools, crate::agent::tools::wallet_balance::build(cfg));
        }
        for spec in extra {
            capped(&mut tools, spec);
        }
        Self { tools }
    }

    /// A registry of exactly these tools, for tests that want one tool and no database.
    #[cfg(test)]
    pub fn build_from_specs(specs: Vec<ToolSpec>, tool_timeout: Duration) -> Self {
        let mut tools = HashMap::new();
        for mut spec in specs {
            spec.timeout = spec.timeout.min(tool_timeout);
            register(&mut tools, spec);
        }
        Self { tools }
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn openai_tools(&self) -> Vec<Tool> {
        let mut tools: Vec<_> = self.tools.values().map(ToolSpec::openai_tool).collect();
        tools.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        tools
    }
}

fn register(map: &mut HashMap<String, ToolSpec>, spec: ToolSpec) {
    map.insert(spec.name.clone(), spec);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(3)])
    }
}
