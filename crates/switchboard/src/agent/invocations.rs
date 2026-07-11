use sqlx::PgPool;
use uuid::Uuid;

use crate::agent::taint::Trust;

#[derive(Debug, Clone)]
pub struct ToolInvocationRecord {
    pub request_id: String,
    pub tenant_id: Option<Uuid>,
    pub iteration: u32,
    pub tool_name: String,
    pub arguments: String,
    pub result: Option<String>,
    pub trust: Trust,
    pub latency_ms: i32,
    pub status: String,
    pub error: Option<String>,
}

pub struct InvocationRecorder {
    pool: Option<PgPool>,
}

impl InvocationRecorder {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self { pool }
    }

    pub fn record(&self, rec: ToolInvocationRecord) {
        let Some(pool) = &self.pool else {
            return;
        };
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = insert(&pool, rec).await {
                tracing::warn!(error = %e, "failed to record tool invocation");
            }
        });
    }

    /// What the agent actually did during one request, scoped to the tenant that
    /// made it.
    ///
    /// This is the only way, from outside the process, to answer "did the model
    /// call that tool?" — the agent loop resolves tool calls internally, so they
    /// never appear in the chat response. Prompt-injection tests need this;
    /// without it they can only inspect the prompt they sent, which always passes.
    pub async fn for_request(
        &self,
        tenant_id: Uuid,
        request_id: &str,
    ) -> Result<Vec<InvocationSummary>, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        sqlx::query_as::<_, InvocationSummary>(
            r#"
            SELECT iteration, tool_name, arguments, trust, status, latency_ms
            FROM tool_invocations
            WHERE tenant_id = $1 AND request_id = $2
            ORDER BY iteration, tool_name
            "#,
        )
        .bind(tenant_id)
        .bind(request_id)
        .fetch_all(pool)
        .await
    }
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct InvocationSummary {
    pub iteration: i16,
    pub tool_name: String,
    /// The arguments the model chose. This is the caller's own request data, and
    /// it is the only way to answer "did it call the right tool *the right way*"
    /// — a calculator invoked with the wrong expression is a wrong answer that a
    /// name-only check scores as correct.
    pub arguments: serde_json::Value,
    pub trust: String,
    pub status: String,
    pub latency_ms: i32,
}

async fn insert(pool: &PgPool, r: ToolInvocationRecord) -> Result<(), sqlx::Error> {
    let tenant_id = match r.tenant_id {
        Some(id) => id,
        None => return Ok(()),
    };

    let args: serde_json::Value =
        serde_json::from_str(&r.arguments).unwrap_or(serde_json::Value::Null);
    let result = r.result.as_ref().map(|s| {
        let truncated = if s.len() > 8192 {
            format!("{}…", &s[..8189])
        } else {
            s.clone()
        };
        serde_json::from_str::<serde_json::Value>(&truncated)
            .unwrap_or(serde_json::Value::String(truncated))
    });

    sqlx::query(
        r#"
        INSERT INTO tool_invocations (
            id, request_id, tenant_id, iteration, tool_name, arguments,
            result, trust, latency_ms, status, error
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(r.request_id)
    .bind(tenant_id)
    .bind(r.iteration as i16)
    .bind(r.tool_name)
    .bind(args)
    .bind(result)
    .bind(r.trust.as_str())
    .bind(r.latency_ms)
    .bind(r.status)
    .bind(r.error)
    .execute(pool)
    .await?;
    Ok(())
}
