use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use redis::AsyncCommands;

use crate::domain::chat::{ChatRequest, ChatResponse};

use super::key::{cache_key, is_cacheable};

/// Two tiers. Moka absorbs the repeated hit inside one process; Redis is what makes
/// a hit on replica A visible to replica B, which is most of the reason a gateway
/// cache is worth having at all.
pub struct ExactCache {
    local: Cache<String, ChatResponse>,
    redis: Option<deadpool_redis::Pool>,
    ttl: Duration,
}

fn redis_key(key: &str) -> String {
    format!("cache:chat:{key}")
}

/// Roughly how many bytes one cached answer occupies.
///
/// `max_capacity` counts whatever the weigher returns. Counting *entries* — the previous
/// behaviour — makes a 10,000-entry cap mean a few megabytes for terse answers and most
/// of a gigabyte for long ones. That is a memory limit set by what your users happen to
/// ask for.
fn weigh(value: &ChatResponse) -> usize {
    let content: usize = value
        .choices
        .iter()
        .filter_map(|c| c.message.content.as_ref())
        .map(|c| match c {
            crate::domain::chat::MessageContent::Text(t) => t.len(),
            crate::domain::chat::MessageContent::Parts(parts) => parts.len() * 256,
        })
        .sum();
    // A floor for the envelope: ids, usage, finish reason.
    content + 512
}

/// 64 MiB of answers per replica.
const MAX_WEIGHT: u64 = 64 * 1024 * 1024;

impl ExactCache {
    pub fn new(ttl_ms: u64, redis: Option<deadpool_redis::Pool>) -> Self {
        let ttl = Duration::from_millis(ttl_ms);
        Self {
            local: Cache::builder()
                .time_to_live(ttl)
                .weigher(|key: &String, value: &ChatResponse| {
                    (weigh(value) + key.len()).min(u32::MAX as usize) as u32
                })
                .max_capacity(MAX_WEIGHT)
                .build(),
            redis,
            ttl,
        }
    }

    pub async fn get(&self, tenant_id: uuid::Uuid, req: &ChatRequest) -> Option<ChatResponse> {
        if !is_cacheable(req) {
            return None;
        }
        let key = cache_key(tenant_id, req);

        if let Some(hit) = self.local.get(&key).await {
            return Some(hit);
        }

        let redis = self.redis.as_ref()?;
        let json: Option<String> = async {
            let mut conn = redis.get().await.ok()?;
            conn.get::<_, Option<String>>(redis_key(&key)).await.ok()?
        }
        .await;

        let resp: ChatResponse = serde_json::from_str(&json?).ok()?;
        // Promote, so the next hit on this replica never leaves the process.
        self.local.insert(key, resp.clone()).await;
        Some(resp)
    }

    pub async fn put(&self, tenant_id: uuid::Uuid, req: &ChatRequest, resp: ChatResponse) {
        if !is_cacheable(req) {
            return;
        }
        let key = cache_key(tenant_id, req);

        if let Some(redis) = &self.redis
            && let Ok(json) = serde_json::to_string(&resp)
        {
            let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
                let mut conn = redis.get().await?;
                let _: () = conn
                    .pset_ex(redis_key(&key), json, self.ttl.as_millis() as u64)
                    .await?;
                Ok(())
            }
            .await;

            if let Err(e) = result {
                // A cache we could not write is a slow request, not a failed one.
                tracing::debug!(error = %e, "failed to write shared cache entry");
            }
        }

        self.local.insert(key, resp).await;
    }
}

pub type SharedExactCache = Arc<ExactCache>;
