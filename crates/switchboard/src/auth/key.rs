use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use rand::Rng;

/// Returns `(lookup_prefix, full_key)` where lookup_prefix is 8 chars for DB index.
pub fn generate_key() -> (String, String) {
    let mut rng = rand::rng();
    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    const SECRET_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    let prefix: String = (0..8)
        .map(|_| ALNUM[rng.random_range(0..ALNUM.len())] as char)
        .collect();
    let secret: String = (0..32)
        .map(|_| SECRET_CHARS[rng.random_range(0..SECRET_CHARS.len())] as char)
        .collect();
    let full = format!("sk-sb-{prefix}.{secret}");
    (prefix, full)
}

pub fn hash_secret(secret: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2.hash_password(secret.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

pub fn verify_secret(hash: &str, secret: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_ok()
}

pub fn parse_bearer(header: &str) -> Option<&str> {
    header.strip_prefix("Bearer ").map(str::trim)
}

pub fn split_key(full_key: &str) -> Option<(&str, &str)> {
    let rest = full_key.strip_prefix("sk-sb-")?;
    let (prefix, secret) = rest.split_once('.')?;
    if prefix.len() == 8 && !secret.is_empty() {
        Some((prefix, secret))
    } else {
        None
    }
}
