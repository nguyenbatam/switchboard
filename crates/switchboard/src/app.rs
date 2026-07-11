use crate::agent::registry::{ToolContext, ToolRegistry, WalletToolConfig};
use crate::agent::tools::mcp;
use crate::agent::{AgentLoop, invocations::InvocationRecorder};
use crate::auth::AuthService;
use crate::config::AppConfig;
use crate::config::models::ModelRegistry;
use crate::providers::registry::ProviderRegistry;
use crate::ratelimit::RateLimiter;
use crate::usage::{TokenCounter, UsageRecorder};
use deadpool_redis::{Config as RedisConfig, Runtime};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub registry: Arc<ProviderRegistry>,
    pub auth: Arc<AuthService>,
    pub usage: Arc<UsageRecorder>,
    pub token_counter: Arc<TokenCounter>,
    pub rate_limiter: Option<Arc<RateLimiter>>,
    pub db: Option<PgPool>,
    pub rag: Option<Arc<crate::rag::RagStore>>,
    pub cache: Option<Arc<crate::cache::ExactCache>>,
    pub agent: Option<Arc<AgentLoop>>,
    pub tool_invocations: Arc<InvocationRecorder>,
    redis: Option<deadpool_redis::Pool>,
}

impl AppState {
    pub async fn boot(config: AppConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let model_registry =
            ModelRegistry::load(&config.models_path).map_err(std::io::Error::other)?;

        let db = match AppConfig::database_url() {
            Ok(url) => match crate::db::connect(
                &url,
                config.database.max_connections,
                config.database.statement_timeout_ms,
            )
            .await
            {
                Ok(pool) => match crate::db::migrate(&pool).await {
                    Ok(()) => {
                        tracing::info!("postgres connected and migrated");
                        Some(pool)
                    }
                    Err(e) => {
                        tracing::warn!(%e, "postgres migration failed, running without postgres");
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!(%e, "postgres unavailable, running without postgres");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(%e, "DATABASE_URL not set, running without postgres");
                None
            }
        };

        let tool_ro = match (db.as_ref(), AppConfig::tool_ro_database_url()) {
            (_, Some(url)) => match crate::db::connect_tool_ro(&url).await {
                Ok(pool) => {
                    tracing::info!("tool_ro postgres connected");
                    Some(pool)
                }
                Err(e) => {
                    tracing::warn!(%e, "tool_ro pool unavailable");
                    None
                }
            },
            (Some(_), None) => {
                if let Ok(url) = AppConfig::database_url() {
                    let ro_url = crate::db::tool_ro_url_from_app_url(&url);
                    match crate::db::connect_tool_ro(&ro_url).await {
                        Ok(pool) => {
                            tracing::info!("tool_ro postgres connected (derived URL)");
                            Some(pool)
                        }
                        Err(e) => {
                            tracing::warn!(%e, "tool_ro pool unavailable");
                            None
                        }
                    }
                } else {
                    None
                }
            }
            _ => None,
        };

        let redis = match AppConfig::redis_url() {
            Ok(url) => {
                let cfg = RedisConfig::from_url(url);
                let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
                tracing::info!("redis connected");
                Some(pool)
            }
            Err(e) => {
                tracing::warn!(%e, "REDIS_URL not set, running without redis");
                None
            }
        };

        let token_counter = Arc::new(TokenCounter::new(&model_registry));
        let registry = Arc::new(ProviderRegistry::with_redis(
            &config,
            model_registry,
            redis.clone(),
        ));
        let auth = Arc::new(AuthService::new(&config, db.clone()));
        // `None` unless `billing.budget_webhook_url` is set. See `usage::notify` for why
        // the URL is the operator's and never a tenant's.
        let budget_notifier =
            crate::usage::notify::BudgetNotifier::new(&config.billing).map(Arc::new);
        let usage = Arc::new(UsageRecorder::new(
            db.clone(),
            redis.clone(),
            budget_notifier,
        ));

        let rag = db.as_ref().map(|pool| {
            Arc::new(crate::rag::RagStore::new(
                pool.clone(),
                registry.clone(),
                config.rag.default_embed_model.clone(),
                config.rag.chunk_tokens,
                config.rag.overlap_tokens,
            ))
        });

        let cache = if config.cache.enabled {
            Some(Arc::new(crate::cache::ExactCache::new(
                config.cache.exact_ttl_ms,
                redis.clone(),
            )))
        } else {
            None
        };

        let rate_limiter = redis
            .as_ref()
            .map(|pool| Arc::new(RateLimiter::new(pool.clone(), config.ratelimit.fail_open)));

        let tool_invocations = Arc::new(InvocationRecorder::new(db.clone()));

        let agent = if config.agent.enabled {
            let wallet = config
                .agent
                .tools
                .wallet
                .as_ref()
                .map(|w| WalletToolConfig {
                    rpc_urls: w.rpc_urls.clone(),
                    max_block_drift: w.max_block_drift,
                    timeout: Duration::from_millis(w.timeout_ms),
                });
            let ctx = ToolContext {
                tool_ro,
                sql_allowed_tables: config.agent.tools.sql_allowed_tables.clone(),
                wallet,
            };
            let mcp_tools = mcp::discover(&config.agent.mcp.servers).await;
            let tools = ToolRegistry::build(
                &ctx,
                mcp_tools,
                Duration::from_millis(config.agent.tool_timeout_ms),
            );
            Some(Arc::new(AgentLoop::new(
                config.agent.max_iterations,
                config.agent.max_total_tokens,
                Duration::from_millis(config.server.request_timeout_ms),
                tools,
            )))
        } else {
            None
        };

        Ok(Self {
            config,
            registry,
            auth,
            usage,
            token_counter,
            rate_limiter,
            db,
            rag,
            cache,
            agent,
            tool_invocations,
            redis,
        })
    }

    pub fn redis(&self) -> Option<&deadpool_redis::Pool> {
        self.redis.as_ref()
    }

    /// Whether Redis is reachable *and configured*.
    ///
    /// This used to answer `true` when `redis == None`, which was less a readiness bug
    /// than a billing one. A pod booted without `REDIS_URL` reported itself ready and
    /// then served traffic with rate limiting failed open, idempotency ignored, and — the
    /// expensive part — `budget::check` unable to read a spend counter. It gave the work
    /// away and told the orchestrator it was healthy.
    pub async fn redis_ok(&self) -> bool {
        match &self.redis {
            Some(pool) => pool.get().await.is_ok(),
            None => false,
        }
    }

    /// Whether Postgres answers, not merely whether a pool was constructed.
    ///
    /// `db.is_some()` stayed true for a pool whose every connection had died. The ledger
    /// lives in Postgres, and a replica that cannot write it is not ready to take money.
    pub async fn postgres_ok(&self) -> bool {
        let Some(pool) = &self.db else {
            return false;
        };
        sqlx::query("SELECT 1").execute(pool).await.is_ok()
    }
}
