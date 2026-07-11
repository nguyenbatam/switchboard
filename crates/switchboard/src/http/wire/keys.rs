use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    /// A human label. Not a secret, not unique, and the only thing an operator will have to
    /// go on when deciding whether a key is still needed.
    pub name: String,
    /// Defaults to `{chat, embeddings}`.
    ///
    /// A caller can only grant scopes it already holds. Otherwise the first key with `keys`
    /// would be a privilege-escalation primitive rather than an administrative one.
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    /// Optional expiry, milliseconds from now.
    ///
    /// A key that never expires is a key that is still valid on the day the laptop it was
    /// copied to is sold.
    #[serde(default)]
    pub expires_in_ms: Option<u64>,
}

/// The **only** response that ever contains a secret.
///
/// `key` is shown once, here, and is unrecoverable afterwards: `api_keys` stores an
/// argon2 hash and there is no path back. Every other shape in this module carries the
/// prefix, which is the non-secret half and is what an operator matches against a log line.
#[derive(Debug, Serialize)]
pub struct CreatedKey {
    pub object: String,
    pub id: Uuid,
    pub name: String,
    /// `sk-sb-<prefix>.<secret>`. Store it now.
    pub key: String,
    pub key_prefix: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A key as anyone may see it afterwards. No hash, no secret.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct KeyObject {
    #[sqlx(default)]
    #[serde(default = "api_key_object")]
    pub object: String,
    pub id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub scopes: Vec<String>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn api_key_object() -> String {
    "api_key".into()
}

#[derive(Debug, Serialize)]
pub struct KeyListResponse {
    pub object: String,
    pub data: Vec<KeyObject>,
}

impl KeyListResponse {
    pub fn new(mut data: Vec<KeyObject>) -> Self {
        for k in &mut data {
            k.object = api_key_object();
        }
        Self {
            object: "list".into(),
            data,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RotateKeyRequest {
    /// How long the old key keeps working, in milliseconds. Default zero: revoked at once.
    ///
    /// A rotation with no overlap is an outage for every process still holding the old key.
    /// A rotation with unbounded overlap is not a rotation. The grace period is the caller's
    /// deploy window, and they are the only one who knows it.
    #[serde(default)]
    pub grace_ms: Option<u64>,
}

/// What a rotation produced: the new key, once, and when the old one stops working.
#[derive(Debug, Serialize)]
pub struct RotatedKey {
    pub object: String,
    pub new_key: CreatedKey,
    pub revoked_key_id: Uuid,
    /// When the old key stops being accepted. `now` when no grace was asked for.
    pub old_key_expires_at: chrono::DateTime<chrono::Utc>,
}
