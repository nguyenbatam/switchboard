use crate::agent::registry::{ToolCallCtx, ToolContext, ToolSpec};
use crate::agent::taint::{Effects, Trust};
use crate::domain::schema::{FieldSpec, ToolParameters};
use crate::domain::values::ParsedToolArguments;
use serde::Deserialize;
use sqlparser::ast::{
    Expr, Function, FunctionArguments, ObjectName, Query, SetExpr, Statement, TableFactor,
    TableWithJoins, Value, With,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

const RESULT_LIMIT: u64 = 200;
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// Default-deny. A denylist of known-dangerous functions is a list of the attacks
/// somebody thought of; Postgres ships thousands of functions and gains more with
/// every extension. Anything not named here is refused.
const ALLOWED_FUNCTIONS: &[&str] = &[
    // aggregates
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "stddev",
    "variance",
    // null handling and conditionals
    "coalesce",
    "nullif",
    "greatest",
    "least",
    // strings
    "lower",
    "upper",
    "length",
    "trim",
    "ltrim",
    "rtrim",
    "substring",
    "concat",
    "left",
    "right",
    "replace",
    "split_part",
    "strpos",
    // numbers
    "abs",
    "ceil",
    "ceiling",
    "floor",
    "round",
    "trunc",
    "mod",
    "power",
    "sqrt",
    // time
    "now",
    "date_trunc",
    "date_part",
    "extract",
    "age",
    // casts and formatting
    "cast",
    "to_char",
    "to_number",
    "to_timestamp",
];

#[derive(Deserialize)]
struct SqlQueryArgs {
    query: String,
}

/// The tool's description, with the allowlist in it.
///
/// A model that is told "allowlisted views" and not *which* ones will guess, and its guess
/// will be the table name a human would use — `tenants`, not `tenants_public`. The guess is
/// then refused by the allowlist, the model tries another, and the agent loop burns its
/// iteration budget rediscovering a fact we could simply have told it.
///
/// The allowlist is the security boundary and stays where it is. This is only the sign on
/// the door.
fn describe(allowed: &[String]) -> String {
    let mut names: Vec<&str> = allowed
        .iter()
        .map(|t| t.strip_prefix("tool_ro.").unwrap_or(t))
        .collect();
    names.sort_unstable();
    names.dedup();

    format!(
        "Run a read-only SQL SELECT. Only these relations may be read: {}. \
         Any other table is refused.",
        names.join(", ")
    )
}

pub fn build(ctx: &ToolContext) -> ToolSpec {
    let pool = ctx.tool_ro.clone();
    let allowed: HashSet<String> = ctx
        .sql_allowed_tables
        .iter()
        .map(|t| t.to_ascii_lowercase())
        .collect();

    ToolSpec::new(
        "sql_query",
        describe(&ctx.sql_allowed_tables),
        ToolParameters::object(
            BTreeMap::from([(
                "query".into(),
                FieldSpec::string("A single SELECT statement"),
            )]),
            vec!["query".into()],
            false,
        ),
        Trust::Untrusted,
        Effects::Egress,
        Duration::from_secs(2),
        move |call_cx, args| run_sql(pool.clone(), &allowed, call_cx, args),
    )
}

fn run_sql(
    pool: Option<sqlx::PgPool>,
    allowed: &HashSet<String>,
    call_cx: &ToolCallCtx,
    args: &ParsedToolArguments,
) -> Result<String, String> {
    let pool = pool.as_ref().ok_or("sql_query requires database")?;
    let args: SqlQueryArgs = args.typed()?;

    // The parser is the first lock; row-level security is the second. Without a
    // tenant to scope to, the second lock cannot engage — and the views this tool
    // reads span every tenant in the database. So: no tenant, no query.
    let tenant_id = call_cx
        .tenant_id
        .ok_or("sql_query requires an authenticated tenant")?;

    let sql = validate_and_rewrite(&args.query, allowed)?;
    let wrapped = format!("SELECT row_to_json(q) AS row FROM ({sql}) q");
    let rows: Vec<(serde_json::Value,)> = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

            // Transaction-scoped, so a pooled connection cannot carry one tenant's
            // identity into the next tenant's query.
            sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
                .bind(tenant_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;

            let rows = sqlx::query_as(&wrapped)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;

            tx.rollback().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(rows)
        })
    })?;

    let mut out = serde_json::to_string(&rows.into_iter().map(|(v,)| v).collect::<Vec<_>>())
        .map_err(|e| e.to_string())?;
    if out.len() > MAX_RESULT_BYTES {
        out.truncate(MAX_RESULT_BYTES);
        out.push('…');
    }
    Ok(out)
}

fn validate_and_rewrite(sql: &str, allowed: &HashSet<String>) -> Result<String, String> {
    // The parser decides how many statements this is, and nothing else does.
    //
    // This used to be `if sql.contains(';') { reject }` — a textual proxy for "exactly one
    // statement", which is a question the parser answers exactly. The proxy refused two
    // things it should not have: a trailing semicolon, which every language model writes and
    // which terminates a single statement rather than beginning a second one; and a
    // semicolon inside a string literal, as in `WHERE name = 'a;b'`.
    //
    // Nothing is lost. `SELECT 1; DROP TABLE t` parses as two statements and is refused two
    // lines below, on the authority that actually knows. And `Statement::Query` below refuses
    // a lone `DROP` — as, independently, do the `SELECT`-only grants on the `tool_ro` role.
    let dialect = PostgreSqlDialect {};
    let statements = Parser::parse_sql(&dialect, sql).map_err(|e| format!("parse error: {e}"))?;
    if statements.len() != 1 {
        return Err("exactly one statement is allowed".into());
    }

    let stmt = &statements[0];
    match stmt {
        Statement::Query(query) => {
            validate_query(query, allowed)?;
            let rewritten = inject_limit(query, RESULT_LIMIT)?;
            Ok(rewritten)
        }
        _ => Err("only SELECT queries are allowed".into()),
    }
}

fn validate_query(query: &Query, allowed: &HashSet<String>) -> Result<(), String> {
    if let Some(with) = &query.with {
        validate_with(with, allowed)?;
    }
    validate_set_expr(&query.body, allowed)
}

fn validate_with(with: &With, allowed: &HashSet<String>) -> Result<(), String> {
    for cte in &with.cte_tables {
        if !matches!(&*cte.query.body, SetExpr::Select(_)) {
            return Err("CTE must be a SELECT".into());
        }
        validate_set_expr(&cte.query.body, allowed)?;
    }
    Ok(())
}

fn validate_set_expr(expr: &SetExpr, allowed: &HashSet<String>) -> Result<(), String> {
    match expr {
        SetExpr::Select(select) => {
            // `SELECT ... INTO t` is a write wearing a SELECT's clothes.
            if select.into.is_some() {
                return Err("SELECT ... INTO is not allowed".into());
            }
            for table in &select.from {
                validate_table_ref(table, allowed)?;
            }
            if let Some(where_clause) = &select.selection {
                validate_expr(where_clause, allowed)?;
            }
            for expr in &select.projection {
                validate_select_item(expr, allowed)?;
            }
            if let Some(having) = &select.having {
                validate_expr(having, allowed)?;
            }
            Ok(())
        }
        SetExpr::Query(q) => validate_query(q, allowed),
        SetExpr::SetOperation { left, right, .. } => {
            validate_set_expr(left, allowed)?;
            validate_set_expr(right, allowed)
        }
        _ => Err("only SELECT queries are allowed".into()),
    }
}

fn validate_table_ref(table: &TableWithJoins, allowed: &HashSet<String>) -> Result<(), String> {
    match &table.relation {
        TableFactor::Table { name, .. } => check_table_name(name, allowed),
        TableFactor::Derived { subquery, .. } => validate_query(subquery, allowed),
        _ => Err("unsupported table reference".into()),
    }
}

fn check_table_name(name: &ObjectName, allowed: &HashSet<String>) -> Result<(), String> {
    let full = name
        .0
        .iter()
        .map(|p| p.value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(".");
    let short = full.rsplit('.').next().unwrap_or(&full).to_string();

    if allowed.contains(&full) || allowed.contains(&short) {
        Ok(())
    } else {
        Err(format!("table '{full}' is not allowlisted"))
    }
}

fn validate_expr(expr: &Expr, allowed: &HashSet<String>) -> Result<(), String> {
    match expr {
        Expr::Function(Function { name, args, .. }) => {
            let fname = name
                .0
                .last()
                .map(|i| i.value.to_ascii_lowercase())
                .unwrap_or_default();
            if !ALLOWED_FUNCTIONS.contains(&fname.as_str()) {
                return Err(format!("function '{fname}' is not on the allowlist"));
            }
            if let FunctionArguments::List(list) = args {
                for arg in &list.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(e),
                    ) = arg
                    {
                        validate_expr(e, allowed)?;
                    }
                }
            }
            Ok(())
        }
        Expr::Subquery(q) => validate_query(q, allowed),
        Expr::BinaryOp { left, right, .. } => {
            validate_expr(left, allowed)?;
            validate_expr(right, allowed)
        }
        Expr::UnaryOp { expr, .. } => validate_expr(expr, allowed),
        Expr::Nested(e) => validate_expr(e, allowed),
        Expr::Cast { expr, .. } => validate_expr(expr, allowed),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                validate_expr(op, allowed)?;
            }
            for (c, r) in conditions.iter().zip(results.iter()) {
                validate_expr(c, allowed)?;
                validate_expr(r, allowed)?;
            }
            if let Some(e) = else_result {
                validate_expr(e, allowed)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_select_item(
    item: &sqlparser::ast::SelectItem,
    allowed: &HashSet<String>,
) -> Result<(), String> {
    use sqlparser::ast::SelectItem;
    match item {
        SelectItem::UnnamedExpr(e) => validate_expr(e, allowed),
        SelectItem::ExprWithAlias { expr, .. } => validate_expr(expr, allowed),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => Ok(()),
    }
}

fn inject_limit(query: &Query, limit: u64) -> Result<String, String> {
    let mut q = query.clone();
    if q.limit.is_none() && q.fetch.is_none() {
        q.limit = Some(Expr::Value(Value::Number(limit.to_string(), false)));
    }
    Ok(q.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_multi_statement() {
        let allowed = HashSet::from(["tenants_public".into()]);
        assert!(validate_and_rewrite("SELECT 1; SELECT 2", &allowed).is_err());
    }

    #[test]
    fn rejects_non_select() {
        let allowed = HashSet::from(["tenants_public".into()]);
        assert!(validate_and_rewrite("DELETE FROM tenants_public", &allowed).is_err());
    }

    #[test]
    fn injects_limit() {
        let allowed = HashSet::from(["tenants_public".into()]);
        let sql = validate_and_rewrite("SELECT * FROM tool_ro.tenants_public", &allowed).unwrap();
        assert!(sql.to_ascii_lowercase().contains("limit"));
    }

    #[test]
    fn rejects_select_into() {
        let allowed = HashSet::from(["tenants_public".into()]);
        let err = validate_and_rewrite("SELECT * INTO evil FROM tenants_public", &allowed)
            .expect_err("SELECT ... INTO writes a table");
        assert!(err.contains("INTO"));
    }

    #[test]
    fn allows_ordinary_aggregates() {
        let allowed = HashSet::from(["usage_by_model".into()]);
        assert!(
            validate_and_rewrite(
                "SELECT model_alias, sum(cost_nano) FROM usage_by_model GROUP BY 1",
                &allowed
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_functions_absent_from_the_allowlist() {
        let allowed = HashSet::from(["tenants_public".into()]);

        // Not a function anyone listed as forbidden, and that is precisely the point.
        let err = validate_and_rewrite(
            "SELECT * FROM tenants_public WHERE name = current_setting('app.tenant_id')",
            &allowed,
        )
        .expect_err("default-deny");
        assert!(err.contains("allowlist"));

        for forbidden in [
            "SELECT pg_read_file('/etc/passwd')",
            "SELECT query_to_xml('SELECT 1', true, true, '')",
            "SELECT pg_sleep(10)",
        ] {
            assert!(
                validate_and_rewrite(forbidden, &allowed).is_err(),
                "{forbidden} must be refused"
            );
        }
    }

    #[test]
    fn rejects_a_function_hidden_in_the_projection() {
        let allowed = HashSet::from(["tenants_public".into()]);
        assert!(
            validate_and_rewrite(
                "SELECT pg_read_file('/etc/passwd') FROM tenants_public",
                &allowed
            )
            .is_err(),
            "the projection is reachable too, not just the WHERE clause"
        );
    }

    #[test]
    fn rejects_tables_outside_the_allowlist() {
        let allowed = HashSet::from(["tenants_public".into()]);
        assert!(validate_and_rewrite("SELECT * FROM api_keys", &allowed).is_err());
    }

    /// Every language model terminates a statement with `;`. Refusing it made the tool look
    /// broken to the one caller it has.
    #[test]
    fn a_trailing_semicolon_terminates_a_statement_rather_than_beginning_a_second() {
        let allowed = HashSet::from(["tenants_public".into()]);
        let sql = validate_and_rewrite("SELECT count(*) FROM tenants_public;", &allowed)
            .expect("one statement, terminated");
        assert!(sql.to_lowercase().contains("from tenants_public"), "{sql}");
        assert!(validate_and_rewrite("SELECT 1 FROM tenants_public  ;  ", &allowed).is_ok());
    }

    /// And the thing the textual check was standing in for is still refused — by the parser,
    /// which is the only thing that actually knows how many statements a string holds.
    #[test]
    fn a_second_statement_is_still_refused() {
        let allowed = HashSet::from(["tenants_public".into()]);
        let err = validate_and_rewrite(
            "SELECT 1 FROM tenants_public; DROP TABLE api_keys",
            &allowed,
        )
        .expect_err("two statements");
        assert!(err.contains("one statement"), "{err}");

        assert!(validate_and_rewrite("DROP TABLE api_keys", &allowed).is_err());
    }

    /// A semicolon inside a string literal is data, not a statement separator. The old
    /// textual scan could not tell the difference.
    #[test]
    fn a_semicolon_inside_a_string_literal_is_data() {
        let allowed = HashSet::from(["tenants_public".into()]);
        assert!(
            validate_and_rewrite("SELECT * FROM tenants_public WHERE name = 'a;b'", &allowed)
                .is_ok()
        );
    }

    /// The sign on the door names the relations, unqualified and sorted, and never the
    /// schema — the connection's `search_path` supplies that.
    #[test]
    fn the_description_tells_the_model_what_it_may_read() {
        let d = describe(&[
            "tool_ro.usage_by_model".into(),
            "tenants_public".into(),
            "tool_ro.tenants_public".into(),
        ]);
        assert!(d.contains("tenants_public, usage_by_model"), "{d}");
        assert!(
            !d.contains("tool_ro."),
            "the schema is not the model's business: {d}"
        );
    }
}
