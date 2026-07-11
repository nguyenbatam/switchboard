pub mod models;
pub mod pricing;

use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Where the model registry lives. Overridable so a deployment can ship its
    /// own catalogue, and so tests can boot against a registry of their own.
    #[serde(default = "default_models_path")]
    pub models_path: String,
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub auth: AuthConfig,
    pub ratelimit: RateLimitConfig,
    pub providers: ProvidersConfig,
    pub cache: CacheConfig,
    pub rag: RagConfig,
    pub agent: AgentConfig,
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub billing: BillingConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BillingConfig {
    /// Where to POST a notification when a tenant crosses a budget threshold.
    ///
    /// **Operator-configured, never per-tenant, and that is a security decision rather than
    /// a convenience one.** A tenant-supplied URL is a server-side request forgery
    /// primitive: the gateway would fetch, from inside the cluster, whatever address a
    /// customer typed into a form — `http://169.254.169.254/`, `http://postgres:5432`, a
    /// neighbour's admin port. Filtering that safely means resolving DNS, rejecting every
    /// private range, and re-checking after the redirect and after the TTL expires, and
    /// getting it wrong once is enough.
    ///
    /// So the gateway posts to *your* endpoint, with `tenant_id` in the payload, and you
    /// fan out to your customers however you already do. `None` disables the notification.
    #[serde(default)]
    pub budget_webhook_url: Option<String>,
    /// Fractions of the monthly budget at which a notification fires, once each per month.
    #[serde(default = "default_budget_thresholds")]
    pub budget_thresholds: Vec<f64>,
    #[serde(default = "default_webhook_timeout_ms")]
    pub webhook_timeout_ms: u64,
}

/// Warn at 80%, warn louder at 90%, and say so when the tap closes.
fn default_budget_thresholds() -> Vec<f64> {
    vec![0.80, 0.90, 1.00]
}

fn default_webhook_timeout_ms() -> u64 {
    3_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Ceiling on producing a response, not on streaming its body. Exceeded, it is a
    /// `504`, and the client learns its request is gone instead of holding a socket.
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_concurrency")]
    pub concurrency_limit: usize,
    /// Peers whose `x-forwarded-for` may be believed.
    ///
    /// Empty by default, which is the safe reading of "we do not know what is in front
    /// of us": an unlisted peer's XFF is ignored and its own socket address is used.
    /// Populate this with your load balancer's addresses, never with `0.0.0.0/0`.
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
}

fn default_models_path() -> String {
    "config/models.toml".into()
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8080
}
fn default_request_timeout_ms() -> u64 {
    300_000
}
fn default_concurrency() -> usize {
    256
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_max_conn")]
    pub max_connections: u32,
    #[serde(default = "default_stmt_timeout")]
    pub statement_timeout_ms: u64,
}

fn default_max_conn() -> u32 {
    10
}
fn default_stmt_timeout() -> u64 {
    5000
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    #[serde(default = "default_pool")]
    pub pool_size: usize,
}

fn default_pool() -> usize {
    8
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub require_auth: bool,
    #[serde(default = "default_dev_tenant")]
    pub dev_tenant_id: String,
}

fn default_dev_tenant() -> String {
    "00000000-0000-7000-8000-000000000001".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_true")]
    pub fail_open: bool,
    #[serde(default = "default_rpm")]
    pub default_rpm: u32,
    #[serde(default = "default_tpm")]
    pub default_tpm: u32,
    #[serde(default = "default_ip_rpm")]
    pub ip_rpm: u32,
}

fn default_true() -> bool {
    true
}
fn default_rpm() -> u32 {
    60
}
fn default_tpm() -> u32 {
    60000
}
fn default_ip_rpm() -> u32 {
    120
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProvidersConfig {
    pub ollama: OllamaConfig,
    pub openai: OpenAiConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaConfig {
    #[serde(default = "default_ollama_url")]
    pub base_url: String,
    #[serde(default = "default_ollama_timeouts")]
    pub timeouts: TimeoutsConfig,
    #[serde(default = "default_ollama_max_inflight")]
    pub max_inflight: usize,
}

/// Ollama on one box runs a handful of generations in parallel and queues the rest
/// internally, invisibly, slowing every request together. Set this to what your box can
/// actually generate at once — the point is to shed the surplus visibly instead.
fn default_ollama_max_inflight() -> usize {
    8
}

fn default_ollama_url() -> String {
    "http://localhost:11434".into()
}

/// Ollama's first request after a cold start pulls the model into memory, which on
/// laptop-class hardware routinely takes longer than the 30s a hosted API needs to
/// produce a first token. Only `ttft` is relaxed; a stall is still a stall.
fn default_ollama_timeouts() -> TimeoutsConfig {
    TimeoutsConfig {
        ttft_ms: 120_000,
        ..TimeoutsConfig::default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiConfig {
    #[serde(default = "default_openai_url")]
    pub base_url: String,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default = "default_openai_max_inflight")]
    pub max_inflight: usize,
}

/// Set this below your org's RPM/TPM ceiling. Above it, OpenAI answers the overflow with
/// `429`s, which then count against our own circuit breaker — we would be teaching the
/// breaker that a healthy provider is broken.
fn default_openai_max_inflight() -> usize {
    64
}

fn default_openai_url() -> String {
    "https://api.openai.com/v1".into()
}

/// The four clocks, in milliseconds. See `providers::timeouts` for why one is not enough.
#[derive(Debug, Clone, Deserialize)]
pub struct TimeoutsConfig {
    #[serde(default = "default_connect_ms")]
    pub connect_ms: u64,
    #[serde(default = "default_ttft_ms")]
    pub ttft_ms: u64,
    #[serde(default = "default_stall_ms")]
    pub stall_ms: u64,
    #[serde(default = "default_total_ms")]
    pub total_ms: u64,
}

fn default_connect_ms() -> u64 {
    2_000
}
fn default_ttft_ms() -> u64 {
    30_000
}
fn default_stall_ms() -> u64 {
    30_000
}
fn default_total_ms() -> u64 {
    300_000
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect_ms: default_connect_ms(),
            ttft_ms: default_ttft_ms(),
            stall_ms: default_stall_ms(),
            total_ms: default_total_ms(),
        }
    }
}

impl From<&TimeoutsConfig> for crate::providers::timeouts::Timeouts {
    fn from(c: &TimeoutsConfig) -> Self {
        use std::time::Duration;
        Self {
            connect: Duration::from_millis(c.connect_ms),
            ttft: Duration::from_millis(c.ttft_ms),
            stall: Duration::from_millis(c.stall_ms),
            total: Duration::from_millis(c.total_ms),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_cache_ttl_ms")]
    pub exact_ttl_ms: u64,
    #[serde(default)]
    pub enabled: bool,
}

fn default_cache_ttl_ms() -> u64 {
    3_600_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct RagConfig {
    #[serde(default = "default_embed_model")]
    pub default_embed_model: String,
    /// Target chunk size, in tokens. Characters are not a stable unit: the same
    /// number buys half as much text in Vietnamese as in English.
    #[serde(default = "default_chunk_tokens")]
    pub chunk_tokens: usize,
    #[serde(default = "default_chunk_overlap")]
    pub overlap_tokens: usize,
}

fn default_embed_model() -> String {
    "nomic-embed-text".into()
}

fn default_chunk_tokens() -> usize {
    512
}

fn default_chunk_overlap() -> usize {
    64
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            default_embed_model: default_embed_model(),
            chunk_tokens: default_chunk_tokens(),
            overlap_tokens: default_chunk_overlap(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_max_iter")]
    pub max_iterations: u32,
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: u32,
    /// Per-tool wall clock. Wired into `ToolRegistry::build`; it used to be declared
    /// here and read nowhere, while the tools silently borrowed the whole request timeout.
    #[serde(default = "default_tool_timeout_ms")]
    pub tool_timeout_ms: u64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub tools: AgentToolsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentToolsConfig {
    #[serde(default = "default_sql_tables")]
    pub sql_allowed_tables: Vec<String>,
    #[serde(default)]
    pub wallet: Option<WalletRpcConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletRpcConfig {
    #[serde(default = "default_wallet_rpcs")]
    pub rpc_urls: Vec<String>,
    #[serde(default = "default_block_drift")]
    pub max_block_drift: u64,
    #[serde(default = "default_wallet_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wallet_rpcs() -> Vec<String> {
    vec![
        "https://eth.llamarpc.com".into(),
        "https://rpc.ankr.com/eth".into(),
    ]
}

fn default_block_drift() -> u64 {
    3
}

fn default_wallet_timeout_ms() -> u64 {
    3_000
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default = "default_mcp_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_mcp_timeout_ms() -> u64 {
    30_000
}

fn default_sql_tables() -> Vec<String> {
    vec![
        "tool_ro.tenants_public".into(),
        "tool_ro.usage_by_model".into(),
        "tenants_public".into(),
        "usage_by_model".into(),
    ]
}

fn default_max_iter() -> u32 {
    8
}

fn default_max_total_tokens() -> u32 {
    32_000
}

fn default_tool_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryConfig {
    #[serde(default = "default_log_format")]
    pub log_format: String,
    #[serde(default = "default_true")]
    pub metrics_enabled: bool,
}

fn default_log_format() -> String {
    "pretty".into()
}

impl AppConfig {
    /// The error is boxed: `figment::Error` is several hundred bytes, and an
    /// `Err` that large would be paid for on every `Ok` return of this signature.
    pub fn load() -> Result<Self, Box<figment::Error>> {
        let mut figment = Figment::new().merge(Toml::file("config/switchboard.toml"));

        if Path::new("config/local.toml").exists() {
            figment = figment.merge(Toml::file("config/local.toml"));
        }

        figment = figment.merge(Env::prefixed("SB__").split("__"));

        let config: AppConfig = figment.extract().map_err(Box::new)?;
        Ok(config)
    }

    pub fn database_url() -> Result<String, String> {
        std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".into())
    }

    pub fn redis_url() -> Result<String, String> {
        std::env::var("REDIS_URL").map_err(|_| "REDIS_URL is not set".into())
    }

    pub fn openai_api_key() -> Option<String> {
        std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
    }

    pub fn tool_ro_database_url() -> Option<String> {
        std::env::var("TOOL_RO_DATABASE_URL")
            .ok()
            .filter(|k| !k.is_empty())
    }
}
