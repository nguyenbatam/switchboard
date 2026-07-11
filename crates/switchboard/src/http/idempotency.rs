//! Idempotency keys, per OpenAI's `Idempotency-Key` header.
//!
//! A chat completion is not a safe operation to repeat: it costs money. Clients
//! retry anyway — a proxy times out, a mobile network drops, an SDK's own retry
//! fires — and without this, the second attempt runs the model again and the tenant
//! pays twice for one answer.
//!
//! Two guarantees, both of which matter:
//!
//! - **Replay**: a completed request returns its stored response verbatim.
//! - **Conflict**: reusing a key with a *different* body is rejected, rather than
//!   silently replaying the answer to a question nobody asked.
//!
//! Redis holds the record. Without Redis there is no coordination between replicas
//! and the header is ignored — which is the pre-existing behaviour, not a regression.

use crate::http::error::ApiError;
use deadpool_redis::Pool as RedisPool;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Long enough to outlive any sane client retry, short enough not to be a database.
const TTL_MS: u64 = 24 * 3600 * 1000;

/// How long a second caller waits for the first to finish before giving up.
///
/// It waits inside a concurrency slot, and each poll costs two Redis round trips, so
/// the budget is a capacity decision, not a patience one. Ten seconds is above the p99
/// of a non-streaming completion and an order of magnitude below the thirty seconds
/// this used to spend holding a slot for somebody else's request.
const IN_FLIGHT_POLL_MS: u64 = 100;
const IN_FLIGHT_MAX_POLLS: u32 = 100;

/// Returned with the `503` when the poll budget runs out. Short, because the original
/// may finish at any moment — this is "check back", not "the breaker is open".
const IN_FLIGHT_RETRY_AFTER_MS: u64 = 2_000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state")]
enum Record {
    /// Someone is running this request right now.
    InFlight { body_hash: String },
    /// It finished; here is exactly what we sent them.
    Done { body_hash: String, response: String },
}

fn key(tenant: Uuid, idem_key: &str) -> String {
    format!("idem:{tenant}:{idem_key}")
}

fn body_hash(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads the `Idempotency-Key` header, if the client sent a non-empty one.
pub fn key_from_headers(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// What a replay needs in order to reproduce the original answer in whichever
/// shape the retrying client asks for — JSON or SSE.
#[derive(Serialize, Deserialize)]
pub struct StoredResponse {
    pub response: crate::domain::chat::ChatResponse,
    pub served_by: Option<String>,
}

pub enum Outcome {
    /// This caller owns the request and must execute it.
    Proceed(Guard),
    /// A previous identical request already produced this response.
    Replay(String),
}

/// Marks the key in-flight for as long as the request runs.
///
/// If the request panics or fails, `Drop` releases the key so the client can retry
/// rather than being locked out for the full TTL by a request that never completed.
pub struct Guard {
    redis: RedisPool,
    key: String,
    body_hash: String,
    committed: bool,
}

impl Guard {
    /// Stores the response so an identical retry replays it instead of re-running.
    pub async fn commit(mut self, response: &str) {
        self.committed = true;
        store(
            self.redis.clone(),
            self.key.clone(),
            self.body_hash.clone(),
            response.to_string(),
        )
        .await;
    }

    /// Same, from a synchronous context such as `StreamGuard::drop`.
    pub fn commit_detached(mut self, response: String) {
        self.committed = true;
        let (redis, key, hash) = (self.redis.clone(), self.key.clone(), self.body_hash.clone());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { store(redis, key, hash, response).await });
        }
    }
}

async fn store(redis: RedisPool, key: String, body_hash: String, response: String) {
    let record = Record::Done {
        body_hash,
        response,
    };
    let Ok(json) = serde_json::to_string(&record) else {
        return;
    };

    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        let mut conn = redis.get().await?;
        let _: () = conn.pset_ex(&key, json, TTL_MS).await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to store idempotent response");
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // The request died. Leaving the key in-flight would make every retry wait
        // out the poll loop and then fail, for a request that never happened.
        let redis = self.redis.clone();
        let key = self.key.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Ok(mut conn) = redis.get().await {
                    let _: Result<i64, _> = conn.del(&key).await;
                }
            });
        }
    }
}

/// Claims `idem_key` for this request, or hands back the response of an identical
/// one that already ran.
pub async fn begin(
    redis: &RedisPool,
    tenant: Uuid,
    idem_key: &str,
    body: &[u8],
) -> Result<Outcome, ApiError> {
    let key = key(tenant, idem_key);
    let hash = body_hash(body);

    for _ in 0..IN_FLIGHT_MAX_POLLS {
        let claimed = try_claim(redis, &key, &hash).await?;
        match claimed {
            Claim::Won => {
                return Ok(Outcome::Proceed(Guard {
                    redis: redis.clone(),
                    key,
                    body_hash: hash,
                    committed: false,
                }));
            }
            Claim::Done(response) => return Ok(Outcome::Replay(response)),
            Claim::Conflict => {
                return Err(ApiError::IdempotencyConflict {
                    message: format!(
                        "Idempotency-Key '{idem_key}' was already used with a different request \
                         body. Mint a new key; retrying this one cannot succeed."
                    ),
                });
            }
            Claim::InFlight => {
                // An identical request is running. Wait for its answer rather than
                // starting a second, equally expensive one.
                tokio::time::sleep(std::time::Duration::from_millis(IN_FLIGHT_POLL_MS)).await;
            }
        }
    }

    // The original is still running after the whole poll budget. Waiting longer holds a
    // concurrency slot for a request that is not ours. `503`, not `500`: a client must
    // read this as "come back", not as a bug, or it abandons an answer it is paying for.
    Err(ApiError::Overloaded {
        message: "an in-flight request with this Idempotency-Key has not finished yet".into(),
        retry_after_ms: Some(IN_FLIGHT_RETRY_AFTER_MS),
    })
}

enum Claim {
    Won,
    InFlight,
    Done(String),
    Conflict,
}

async fn try_claim(redis: &RedisPool, key: &str, hash: &str) -> Result<Claim, ApiError> {
    let mut conn = redis.get().await.map_err(|e| ApiError::Server {
        message: e.to_string(),
    })?;

    let in_flight = serde_json::to_string(&Record::InFlight {
        body_hash: hash.to_string(),
    })
    .unwrap_or_default();

    // SET NX is the whole concurrency story: exactly one caller creates the key.
    let won: Option<String> = redis::cmd("SET")
        .arg(key)
        .arg(&in_flight)
        .arg("NX")
        .arg("PX")
        .arg(TTL_MS)
        .query_async(&mut *conn)
        .await
        .map_err(|e| ApiError::Server {
            message: e.to_string(),
        })?;

    if won.is_some() {
        return Ok(Claim::Won);
    }

    let existing: Option<String> = conn.get(key).await.map_err(|e| ApiError::Server {
        message: e.to_string(),
    })?;

    let Some(raw) = existing else {
        // It expired between the SET and the GET. Next loop iteration claims it.
        return Ok(Claim::InFlight);
    };

    match serde_json::from_str::<Record>(&raw) {
        Ok(Record::Done {
            body_hash,
            response,
        }) => {
            if body_hash == hash {
                Ok(Claim::Done(response))
            } else {
                Ok(Claim::Conflict)
            }
        }
        Ok(Record::InFlight { body_hash }) => {
            if body_hash == hash {
                Ok(Claim::InFlight)
            } else {
                Ok(Claim::Conflict)
            }
        }
        Err(_) => Ok(Claim::InFlight),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_scoped_to_the_tenant() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_ne!(key(a, "same"), key(b, "same"));
        assert!(key(a, "k").starts_with("idem:"));
    }

    #[test]
    fn the_body_hash_distinguishes_different_requests() {
        assert_eq!(body_hash(b"{\"a\":1}"), body_hash(b"{\"a\":1}"));
        assert_ne!(body_hash(b"{\"a\":1}"), body_hash(b"{\"a\":2}"));
    }

    #[test]
    fn records_round_trip_through_json() {
        let done = Record::Done {
            body_hash: "abc".into(),
            response: "{\"ok\":true}".into(),
        };
        let raw = serde_json::to_string(&done).unwrap();
        match serde_json::from_str::<Record>(&raw).unwrap() {
            Record::Done {
                body_hash,
                response,
            } => {
                assert_eq!(body_hash, "abc");
                assert_eq!(response, "{\"ok\":true}");
            }
            _ => panic!("wrong variant"),
        }
    }
}
