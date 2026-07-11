//! Everything a third party must implement to be a good citizen of the gateway.
//!
//! Nothing in this module knows what a bot is. It is the reusable half of this example:
//! `docs/15 §7` (the wire) and `docs/15 §8` (the obligations), as code.

pub mod aimd;
pub mod backoff;
pub mod client;
pub mod headers;
pub mod models;
pub mod retry_budget;
pub mod usage;

pub use aimd::{AdaptiveLimiter, Aimd};
pub use client::{Answer, Config, GatewayClient, Headroom, Outcome, idempotency_key};
pub use headers::{BudgetView, Bucket, Provenance, RateLimitView};
pub use models::{Drift, ModelCard};
pub use retry_budget::RetryBudget;
pub use usage::{UsageRecord, UsageSnapshot};
