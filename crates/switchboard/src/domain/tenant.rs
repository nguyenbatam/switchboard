use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Tenant {
    pub id: Uuid,
    pub name: String,
    pub rpm_limit: i32,
    pub tpm_limit: i32,
    pub monthly_budget_nano: Option<i64>,
}

/// The scope a key must carry to manage other keys.
///
/// Deliberately not granted by the `{chat,embeddings}` default in `api_keys.scopes`, and
/// deliberately not held by the synthetic dev tenant. A gateway running with
/// `require_auth = false` hands every anonymous caller the dev tenant; if that tenant could
/// mint keys, "auth disabled for local development" would silently mean "key management
/// open to the internet" the first time somebody deployed it.
///
/// The first key is minted by `sbctl`, out of band. Bootstrapping an admin API from inside
/// itself is a chicken-and-egg problem whose usual solution is a default password.
pub const SCOPE_KEYS: &str = "keys";

#[derive(Debug, Clone)]
pub struct AuthenticatedKey {
    /// Set when the request authenticated against `api_keys`; `None` for synthetic dev access.
    pub api_key_id: Option<Uuid>,
    pub tenant: Tenant,
    pub scopes: Vec<String>,
}

impl AuthenticatedKey {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}
