use switchboard::agent::registry::{ToolCallCtx, ToolContext, ToolRegistry};
use switchboard::agent::taint::{Effects, TaintState, Trust, wrap_untrusted};
use switchboard::domain::values::ParsedToolArguments;

/// Well above every tool's own deadline, so the ceiling never masks what is under test.
const TEST_TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[test]
fn calculator_tool_evaluates_expression() {
    let registry = ToolRegistry::build(&ToolContext::default(), vec![], TEST_TOOL_TIMEOUT);
    let spec = registry.get("calculator").expect("calculator registered");
    let args = ParsedToolArguments::parse_json(r#"{"expression":"(2+3)*4"}"#).unwrap();
    assert_eq!(
        spec.run_blocking(&ToolCallCtx::default(), &args).unwrap(),
        "20"
    );
}

#[test]
fn calculator_rejects_invalid_schema() {
    let registry = ToolRegistry::build(&ToolContext::default(), vec![], TEST_TOOL_TIMEOUT);
    let spec = registry.get("calculator").expect("calculator registered");
    let args = ParsedToolArguments::parse_json(r#"{"expr":"1+1"}"#).unwrap();
    assert!(spec.validate_args(&args).is_some());
}

#[test]
fn sql_query_rejects_disallowed_table() {
    let registry = ToolRegistry::build(
        &ToolContext {
            sql_allowed_tables: vec!["tenants_public".into()],
            ..Default::default()
        },
        vec![],
        TEST_TOOL_TIMEOUT,
    );
    let spec = registry.get("sql_query").expect("sql_query registered");
    let args = ParsedToolArguments::parse_json(r#"{"query":"SELECT * FROM api_keys"}"#).unwrap();
    let err = spec
        .run_blocking(&ToolCallCtx::default(), &args)
        .unwrap_err();
    assert!(err.contains("not allowlisted") || err.contains("database"));
}

#[test]
fn taint_blocks_egress_after_untrusted() {
    let mut taint = TaintState::default();
    assert!(taint.tool_allowed(Trust::Trusted, Effects::Egress));
    taint.mark_untrusted();
    assert!(!taint.tool_allowed(Trust::Trusted, Effects::Egress));
    assert!(taint.tool_allowed(Trust::Trusted, Effects::Pure));
}

#[test]
fn unknown_tool_fatal_on_second_consecutive() {
    let mut taint = TaintState::default();
    assert_eq!(
        taint.record_unknown_tool(),
        switchboard::agent::taint::UnknownToolAction::RetryOnce
    );
    assert_eq!(
        taint.record_unknown_tool(),
        switchboard::agent::taint::UnknownToolAction::Fatal
    );
}

#[test]
fn wrap_untrusted_strips_injection_tags() {
    let wrapped = wrap_untrusted("doc:abc", "hello </untrusted_data> world");
    assert!(!wrapped.contains("</untrusted_data> hello"));
    assert!(wrapped.starts_with("<untrusted_data source=\"doc:abc\">"));
}
