use switchboard::agent::registry::{ToolCallCtx, ToolContext, ToolRegistry};
use switchboard::agent::taint::{Effects, Trust};
use switchboard::agent::tools::mcp;
use switchboard::config::McpServerConfig;
use switchboard::domain::values::ParsedToolArguments;

/// Well above every tool's own deadline, so the ceiling never masks what is under test.
const TEST_TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn npx_available() -> bool {
    std::process::Command::new("npx")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn everything_server() -> McpServerConfig {
    McpServerConfig {
        name: "everything".into(),
        command: "npx".into(),
        args: vec![
            "-y".into(),
            "@modelcontextprotocol/server-everything".into(),
        ],
        env: Default::default(),
        enabled: true,
        trusted: false,
        timeout_ms: 30_000,
    }
}

#[tokio::test]
async fn mcp_discovers_tools_from_everything_server() {
    if !npx_available() {
        eprintln!("skip mcp_discovers_tools_from_everything_server: npx not found");
        return;
    }

    let tools = mcp::discover(&[everything_server()]).await;
    assert!(
        !tools.is_empty(),
        "expected MCP server-everything to expose at least one tool"
    );
    let echo = tools
        .iter()
        .find(|t| t.name == "mcp_everything_echo")
        .expect("echo tool should be registered as mcp_everything_echo");
    assert_eq!(echo.trust, Trust::Untrusted);
    assert_eq!(echo.effects, Effects::SideEffecting);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_echo_tool_roundtrip() {
    if !npx_available() {
        eprintln!("skip mcp_echo_tool_roundtrip: npx not found");
        return;
    }

    let tools = mcp::discover(&[everything_server()]).await;
    let registry = ToolRegistry::build(&ToolContext::default(), tools, TEST_TOOL_TIMEOUT);
    let spec = registry
        .get("mcp_everything_echo")
        .expect("mcp_everything_echo registered");
    let args = ParsedToolArguments::parse_json(r#"{"message":"switchboard"}"#).unwrap();
    let out = spec.run_blocking(&ToolCallCtx::default(), &args).unwrap();
    assert!(
        out.contains("switchboard"),
        "echo response should contain input message, got: {out}"
    );
}
