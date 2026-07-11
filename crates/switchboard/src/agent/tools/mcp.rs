use crate::agent::registry::ToolSpec;
use crate::agent::taint::{Effects, Trust};
use crate::config::McpServerConfig;
use crate::domain::schema::ToolParameters;
use crate::domain::values::ParsedToolArguments;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

type McpClient = RunningService<RoleClient, ()>;

#[derive(Clone)]
struct McpToolHandle {
    client: Arc<Mutex<McpClient>>,
    tool_name: String,
    timeout: Duration,
}

pub async fn discover(servers: &[McpServerConfig]) -> Vec<ToolSpec> {
    let mut tools = Vec::new();

    for server in servers {
        if !server.enabled {
            continue;
        }
        let Ok(client) = connect(server).await else {
            tracing::warn!(name = %server.name, "mcp server unavailable at boot");
            continue;
        };
        let client = Arc::new(Mutex::new(client));

        let listed = {
            let guard = client.lock().await;
            guard.list_all_tools().await
        };
        let Ok(mcp_tools) = listed else {
            tracing::warn!(name = %server.name, "mcp list_tools failed");
            continue;
        };

        for tool in mcp_tools {
            let tool_name = tool.name.to_string();
            let openai_name = format!("mcp_{}_{}", server.name, tool_name);
            let description = tool
                .description
                .as_ref()
                .map(|d| d.to_string())
                .unwrap_or_else(|| format!("MCP tool {tool_name}"));
            let parameters = mcp_schema_to_parameters(&tool.schema_as_json_value());
            let trust = if server.trusted {
                Trust::Trusted
            } else {
                Trust::Untrusted
            };
            let timeout = Duration::from_millis(server.timeout_ms);
            let handle = McpToolHandle {
                client: client.clone(),
                tool_name: tool_name.clone(),
                timeout,
            };

            tools.push(ToolSpec::new(
                openai_name,
                description,
                parameters,
                trust,
                Effects::SideEffecting,
                timeout,
                move |_cx, args| handle.call(args),
            ));
        }
    }

    tools
}

impl McpToolHandle {
    fn call(&self, args: &ParsedToolArguments) -> Result<String, String> {
        let args_value: Value = serde_json::to_value(args).map_err(|e| e.to_string())?;
        let arguments = args_value.as_object().cloned();
        let handle = self.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let req = CallToolRequestParams {
                    meta: None,
                    name: handle.tool_name.into(),
                    arguments,
                    task: None,
                };
                let call = async {
                    let guard = handle.client.lock().await;
                    guard.call_tool(req).await
                };
                let result = tokio::time::timeout(handle.timeout, call)
                    .await
                    .map_err(|_| "mcp tool timeout".to_string())?
                    .map_err(|e| e.to_string())?;

                if result.is_error.unwrap_or(false) {
                    return Err(result
                        .content
                        .iter()
                        .filter_map(|c| c.as_text().map(|t| t.text.to_string()))
                        .collect::<Vec<_>>()
                        .join("\n"));
                }

                Ok(result
                    .content
                    .iter()
                    .filter_map(|c| c.as_text().map(|t| t.text.to_string()))
                    .collect::<Vec<_>>()
                    .join("\n"))
            })
        })
    }
}

async fn connect(server: &McpServerConfig) -> Result<McpClient, String> {
    let mut command = Command::new(&server.command);
    command.args(&server.args);
    if !server.env.is_empty() {
        command.envs(&server.env);
    }
    let transport = TokioChildProcess::new(command.configure(|_| {})).map_err(|e| e.to_string())?;
    ().serve(transport).await.map_err(|e| e.to_string())
}

fn mcp_schema_to_parameters(schema: &Value) -> ToolParameters {
    serde_json::from_value(schema.clone()).unwrap_or_else(|_| {
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        }))
        .expect("fallback schema")
    })
}
