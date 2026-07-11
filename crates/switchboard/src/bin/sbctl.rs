use switchboard::auth::key;
use switchboard::config::AppConfig;
use switchboard::db;
use uuid::Uuid;

type Fallible = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> Fallible {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "create-key" => {
            let tenant_id = args
                .get(2)
                .ok_or("usage: sbctl create-key <tenant_id> [name] [scope,scope,...]")?;
            let name = args.get(3).cloned().unwrap_or_else(|| "default".into());
            let scopes: Vec<String> = args
                .get(4)
                .map(|s| s.split(',').map(str::trim).map(str::to_string).collect())
                .unwrap_or_else(|| vec!["chat".into(), "embeddings".into()]);
            create_key(tenant_id, &name, &scopes).await?;
        }
        "create-tenant" => {
            let name = args
                .get(2)
                .ok_or("usage: sbctl create-tenant <name> [tenant_id]")?;
            let id = args.get(3).cloned();
            create_tenant(name, id.as_deref()).await?;
        }
        "set-budget" => {
            let tenant_id = args
                .get(2)
                .ok_or("usage: sbctl set-budget <tenant_id> <usd|none>")?;
            let amount = args
                .get(3)
                .ok_or("usage: sbctl set-budget <tenant_id> <usd|none>")?;
            set_budget(tenant_id, amount).await?;
        }
        "budget" => {
            let tenant_id = args.get(2).ok_or("usage: sbctl budget <tenant_id>")?;
            show_budget(tenant_id).await?;
        }
        "stats" => {
            stats(args.get(2).map(String::as_str)).await?;
        }
        "migrate" => {
            let url = AppConfig::database_url().map_err(std::io::Error::other)?;
            let pool = db::connect(&url, 2, 5_000).await?;
            db::migrate(&pool).await?;
            println!("migrations applied");
        }
        _ => {
            eprintln!("sbctl commands:");
            eprintln!("  create-tenant <name> [tenant_id]");
            eprintln!("  create-key    <tenant_id> [name]");
            eprintln!("  set-budget    <tenant_id> <usd|none>   e.g. 0.50, 25, none");
            eprintln!("  budget        <tenant_id>              limit / spent / remaining");
            eprintln!("  stats         [tenant_id]              margin, latency percentiles");
            eprintln!("  migrate");
        }
    }
    Ok(())
}

/// USD as an exact decimal string → nanodollars.
///
/// Parsed by hand rather than through `f64`, for the reason `docs/00` gives: money is
/// `bigint` nanodollars, always. `"0.15"` must be exactly `150_000_000`, not
/// `149_999_999.99999997`, and a budget that is off by one nanodollar is a bug report
/// nobody enjoys reading.
fn usd_to_nano(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty amount".into());
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if frac.len() > 9 {
        return Err(format!(
            "{s}: more precision than a nanodollar; at most 9 decimal places"
        ));
    }
    if whole.is_empty() && frac.is_empty() {
        return Err(format!("{s}: not a number"));
    }
    let parse = |part: &str, what: &str| -> Result<i64, String> {
        if part.is_empty() {
            return Ok(0);
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("{s}: {what} must be digits"));
        }
        part.parse::<i64>().map_err(|_| format!("{s}: too large"))
    };
    let whole_n = parse(whole, "whole part")?;
    // Right-pad the fraction to exactly nine digits: "5" is 0.5 USD, not 5 nanodollars.
    let padded = format!("{frac:0<9}");
    let frac_n = parse(&padded, "fraction")?;

    whole_n
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(frac_n))
        .ok_or_else(|| format!("{s}: overflows i64 nanodollars"))
}

fn nano_to_usd(nano: i64) -> String {
    let sign = if nano < 0 { "-" } else { "" };
    let n = nano.unsigned_abs();
    format!("{sign}{}.{:09}", n / 1_000_000_000, n % 1_000_000_000)
}

async fn create_tenant(name: &str, id: Option<&str>) -> Fallible {
    let url = AppConfig::database_url().map_err(std::io::Error::other)?;
    let pool = db::connect(&url, 2, 5_000).await?;
    let tenant = match id {
        Some(raw) => Uuid::parse_str(raw)?,
        None => Uuid::now_v7(),
    };

    sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING")
        .bind(tenant)
        .bind(name)
        .execute(&pool)
        .await?;

    println!("{tenant}");
    Ok(())
}

/// Sets the monthly ceiling. `none` removes it, which means unlimited.
///
/// The ceiling lives in `tenants.monthly_budget_nano` and is read live on every request,
/// so nothing needs to be invalidated here. The Redis counter caches *spend*, not the
/// limit.
async fn set_budget(tenant_id: &str, amount: &str) -> Fallible {
    let url = AppConfig::database_url().map_err(std::io::Error::other)?;
    let pool = db::connect(&url, 2, 5_000).await?;
    let tenant = Uuid::parse_str(tenant_id)?;

    let nano: Option<i64> = if amount.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(usd_to_nano(amount).map_err(std::io::Error::other)?)
    };

    let updated = sqlx::query("UPDATE tenants SET monthly_budget_nano = $1 WHERE id = $2")
        .bind(nano)
        .bind(tenant)
        .execute(&pool)
        .await?;

    if updated.rows_affected() == 0 {
        return Err(format!("no such tenant: {tenant}").into());
    }

    match nano {
        Some(n) => println!("{tenant} budget = {n} nano (${})", nano_to_usd(n)),
        None => println!("{tenant} budget = unlimited"),
    }
    Ok(())
}

/// What a tenant has been given, what it has spent, and what is left.
///
/// Spend is read from `usage_hourly`, the same rollup `budget::check` rebuilds its Redis
/// counter from. That makes this the authority, and the counter merely fast.
async fn show_budget(tenant_id: &str) -> Fallible {
    let url = AppConfig::database_url().map_err(std::io::Error::other)?;
    let pool = db::connect(&url, 2, 5_000).await?;
    let tenant = Uuid::parse_str(tenant_id)?;

    let limit: Option<i64> =
        sqlx::query_scalar("SELECT monthly_budget_nano FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_optional(&pool)
            .await?
            .ok_or_else(|| format!("no such tenant: {tenant}"))?;

    let spent: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(cost_nano), 0)::bigint
        FROM usage_hourly
        WHERE tenant_id = $1 AND hour >= date_trunc('month', now())
        "#,
    )
    .bind(tenant)
    .fetch_one(&pool)
    .await?;

    println!("tenant     {tenant}");
    match limit {
        Some(l) => {
            println!("limit      {l} nano (${})", nano_to_usd(l));
            println!("spent      {spent} nano (${})", nano_to_usd(spent));
            println!(
                "remaining  {} nano (${})",
                l - spent,
                nano_to_usd(l - spent)
            );
        }
        None => {
            println!("limit      unlimited");
            println!("spent      {spent} nano (${})", nano_to_usd(spent));
        }
    }
    Ok(())
}

/// The operator's view: what we earned, what it cost us, and how fast we were.
///
/// Deliberately not an HTTP endpoint. `cost_nano` is revenue and a tenant may see its own;
/// `upstream_cost_nano` is what we paid the provider, and a tenant that can see the margin
/// can negotiate against it. Migration 007 revokes the column from the read-only tool role
/// for the same reason.
///
/// Percentiles come from `percentile_disc`, not `percentile_cont`: an interpolated latency
/// is a number no request ever experienced.
async fn stats(tenant_id: Option<&str>) -> Fallible {
    let url = AppConfig::database_url().map_err(std::io::Error::other)?;
    let pool = db::connect(&url, 2, 5_000).await?;
    let tenant = tenant_id.map(Uuid::parse_str).transpose()?;

    let row: Option<StatsRow> = sqlx::query_as(
        r#"
        SELECT
            count(*)::bigint                                              AS requests,
            count(*) FILTER (WHERE cost_nano > 0)::bigint                 AS charged,
            count(*) FILTER (WHERE cache_hit IS NOT NULL)::bigint         AS cache_hits,
            count(*) FILTER (WHERE status = 'error')::bigint              AS errors,
            count(*) FILTER (WHERE status = 'client_disconnect')::bigint  AS disconnects,
            COALESCE(SUM(cost_nano), 0)::bigint                           AS revenue_nano,
            COALESCE(SUM(upstream_cost_nano), 0)::bigint                  AS cogs_nano,
            COALESCE(percentile_disc(0.50) WITHIN GROUP (ORDER BY latency_ms), 0)::int AS p50,
            COALESCE(percentile_disc(0.95) WITHIN GROUP (ORDER BY latency_ms), 0)::int AS p95,
            COALESCE(percentile_disc(0.99) WITHIN GROUP (ORDER BY latency_ms), 0)::int AS p99,
            COALESCE(MAX(latency_ms), 0)::int                             AS max_ms
        FROM usage_records
        WHERE ($1::uuid IS NULL OR tenant_id = $1)
          AND status <> 'error'
        "#,
    )
    .bind(tenant)
    .fetch_optional(&pool)
    .await?;

    let Some(s) = row else {
        println!("no usage recorded");
        return Ok(());
    };

    let margin = s.revenue_nano - s.cogs_nano;
    let margin_bp = if s.revenue_nano > 0 {
        (margin as i128 * 10_000 / s.revenue_nano as i128) as i64
    } else {
        0
    };

    println!("scope      {}", tenant_id.unwrap_or("all tenants"));
    println!(
        "requests   {} (charged {}, cache hits {}, errors {}, disconnects {})",
        s.requests, s.charged, s.cache_hits, s.errors, s.disconnects
    );
    println!();
    println!(
        "revenue    {} nano (${})",
        s.revenue_nano,
        nano_to_usd(s.revenue_nano)
    );
    println!(
        "cogs       {} nano (${})",
        s.cogs_nano,
        nano_to_usd(s.cogs_nano)
    );
    println!(
        "margin     {} nano (${})  {}.{:02}%",
        margin,
        nano_to_usd(margin),
        margin_bp / 100,
        (margin_bp % 100).abs()
    );
    if margin < 0 {
        println!("           ^^ you are losing money on every request. Check config/models.toml.");
    }
    println!();
    println!(
        "latency    p50 {}ms   p95 {}ms   p99 {}ms   max {}ms",
        s.p50, s.p95, s.p99, s.max_ms
    );
    println!("           (successful requests only; a shed 503 returns in ~1ms and would");
    println!("            drag every percentile down as the gateway got sicker — docs/15 §4)");

    Ok(())
}

#[derive(sqlx::FromRow)]
struct StatsRow {
    requests: i64,
    charged: i64,
    cache_hits: i64,
    errors: i64,
    disconnects: i64,
    revenue_nano: i64,
    cogs_nano: i64,
    p50: i32,
    p95: i32,
    p99: i32,
    max_ms: i32,
}

/// Mints a key out of band.
///
/// This is the bootstrap, and it exists because `/v1/keys` requires the `keys` scope to
/// mint a key with the `keys` scope. Bootstrapping an admin API from inside itself is a
/// chicken-and-egg problem whose usual answer is a default password, and this is the other
/// answer: an operator, a shell, and the database.
///
///     sbctl create-key <tenant_id> admin keys,chat,embeddings
async fn create_key(tenant_id: &str, name: &str, scopes: &[String]) -> Fallible {
    let url = AppConfig::database_url().map_err(std::io::Error::other)?;
    let pool = db::connect(&url, 2, 5_000).await?;
    let tenant = Uuid::parse_str(tenant_id)?;

    let (prefix, full_key) = key::generate_key();
    let secret_part = full_key.split_once('.').map(|(_, s)| s).unwrap_or("");
    let hash = key::hash_secret(secret_part).map_err(|e| std::io::Error::other(e.to_string()))?;

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, tenant_id, key_prefix, key_hash, name, scopes)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(prefix)
    .bind(hash)
    .bind(name)
    .bind(scopes)
    .execute(&pool)
    .await?;

    // The only time this secret exists outside an argon2 hash.
    println!("{full_key}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_parses_exactly_into_nanodollars() {
        assert_eq!(usd_to_nano("1").unwrap(), 1_000_000_000);
        assert_eq!(usd_to_nano("0.5").unwrap(), 500_000_000);
        assert_eq!(usd_to_nano("0.15").unwrap(), 150_000_000);
        assert_eq!(usd_to_nano("25").unwrap(), 25_000_000_000);
        assert_eq!(usd_to_nano("0.000000001").unwrap(), 1);
        assert_eq!(usd_to_nano(".5").unwrap(), 500_000_000);
    }

    #[test]
    fn the_fraction_is_left_aligned_not_right() {
        // "0.5" is fifty cents, not five nanodollars. Getting this backwards is the
        // classic fixed-point bug, and it is silent.
        assert_eq!(usd_to_nano("0.5").unwrap(), 500_000_000);
        assert_ne!(usd_to_nano("0.5").unwrap(), 5);
    }

    #[test]
    fn f64_would_have_been_wrong_here() {
        // (0.15_f64 * 1e9) as i64 == 150000000 today, but 0.1 + 0.2 != 0.3 and the same
        // class of error reaches money as soon as the arithmetic is more than one step.
        // Parsing the decimal directly cannot drift.
        assert_eq!(usd_to_nano("0.15").unwrap(), 150_000_000);
        assert_eq!(usd_to_nano("0.07").unwrap(), 70_000_000);
    }

    #[test]
    fn rejects_more_precision_than_a_nanodollar() {
        assert!(usd_to_nano("0.0000000001").is_err());
    }

    #[test]
    fn rejects_junk() {
        assert!(usd_to_nano("abc").is_err());
        assert!(usd_to_nano("1.2.3").is_err());
        assert!(usd_to_nano("").is_err());
        assert!(usd_to_nano("-1").is_err());
    }

    #[test]
    fn nano_renders_back_to_usd() {
        assert_eq!(nano_to_usd(150_000_000), "0.150000000");
        assert_eq!(nano_to_usd(1_000_000_000), "1.000000000");
        assert_eq!(nano_to_usd(0), "0.000000000");
    }

    #[test]
    fn usd_round_trips() {
        for s in ["0.15", "1", "25", "0.000000001"] {
            let nano = usd_to_nano(s).unwrap();
            assert_eq!(usd_to_nano(&nano_to_usd(nano)).unwrap(), nano);
        }
    }
}
