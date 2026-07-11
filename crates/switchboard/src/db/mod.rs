use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

pub async fn connect(
    database_url: &str,
    max_connections: u32,
    statement_timeout_ms: u64,
) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(60))
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                use sqlx::Executor;

                // A query that outlives the request it belongs to is holding a
                // connection nobody is waiting on any more.
                conn.execute(format!("SET statement_timeout = {statement_timeout_ms}").as_str())
                    .await?;

                // Filtered ANN search (`WHERE tenant_id = $1 ORDER BY embedding <=> $2`)
                // silently loses recall: HNSW walks the graph, the filter then throws
                // most of those neighbours away, and too few survive to fill `k`.
                // Iterative scan keeps walking until enough rows survive the filter.
                //
                // Requires pgvector >= 0.8. On older builds the SET fails, and a
                // lower-recall search beats no database at all, so log and continue.
                for setting in [
                    "SET hnsw.iterative_scan = relaxed_order",
                    "SET hnsw.ef_search = 100",
                    "SET hnsw.max_scan_tuples = 20000",
                ] {
                    if let Err(e) = conn.execute(setting).await {
                        tracing::debug!(setting, error = %e, "hnsw tuning unavailable");
                    }
                }

                Ok(())
            })
        })
        .connect(database_url)
        .await
}

/// The read-only pool the `sql_query` tool runs on.
///
/// `search_path` is pinned to `tool_ro`, and to nothing else. Two reasons, and the second
/// is the one that bites.
///
/// The allowlist in `agent::tools::sql_query` accepts both `tool_ro.tenants_public` and the
/// bare `tenants_public`. Without a search path, the bare form passes validation and then
/// fails at the database with `relation "tenants_public" does not exist` — an allowlist that
/// permits a query which cannot run. A model told it may read `tenants_public` and then told
/// the relation does not exist will simply guess another name, which is what
/// [`eval`'s `tool-002`](../../../../eval/datasets/fast.jsonl) caught.
///
/// And it is `tool_ro` *alone*, not `tool_ro, public`. With `public` on the path an
/// allowlisted-looking bare name could resolve to a real table if one ever came to share a
/// view's name. The role's grants would still refuse it, but a defence that depends on two
/// mechanisms agreeing is a defence with a seam in it.
pub async fn connect_tool_ro(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET search_path TO tool_ro")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

pub fn tool_ro_url_from_app_url(app_url: &str) -> String {
    if let Some((scheme, rest)) = app_url.split_once("://")
        && let Some((_auth, host)) = rest.split_once('@')
    {
        return format!("{scheme}://switchboard_tool_ro:switchboard_ro@{host}");
    }
    app_url.replace(
        "switchboard:switchboard@",
        "switchboard_tool_ro:switchboard_ro@",
    )
}
