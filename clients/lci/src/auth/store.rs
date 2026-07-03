//! The on-disk token cache: a single `token.json` in the OS config dir, written `0600`.
//!
//! We persist `expires_at` as an **absolute** unix timestamp (computed from `expires_in` at fetch
//! time) so a stale process clock across restarts can't be tricked into using an expired token.
//! Token values are never logged.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The token exchange response we care about (Keycloak returns more; extra fields are ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    #[serde(default)]
    pub scope: Option<String>,
    /// Lifetime in seconds from *now*; we convert to an absolute `expires_at` on store.
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub id_token: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

/// The cached token as it lives on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub token_type: String,
    #[serde(default)]
    pub scope: Option<String>,
    /// Absolute expiry (unix seconds). Compared against `now` — never a relative lifetime.
    pub expires_at: i64,
    /// When this token was fetched (unix seconds), for display/debugging.
    pub obtained_at: i64,
    #[serde(default)]
    pub id_token: Option<String>,
}

impl StoredToken {
    /// Build a stored token from a fresh exchange/refresh response, stamping absolute times against
    /// `now` (unix seconds). A missing `expires_in` is treated as already-expired so we don't cache a
    /// token we can't reason about.
    pub fn from_response(resp: TokenResponse, now: i64) -> Self {
        let expires_at = now + resp.expires_in.unwrap_or(0);
        Self {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
            token_type: resp.token_type,
            scope: resp.scope,
            expires_at,
            obtained_at: now,
            id_token: resp.id_token,
        }
    }

    /// Seconds until expiry relative to `now` (negative once expired).
    pub fn seconds_until_expiry(&self, now: i64) -> i64 {
        self.expires_at - now
    }

    /// Whether the access token is usable: present and at least `skew` seconds from expiry.
    pub fn is_fresh(&self, now: i64, skew: i64) -> bool {
        !self.access_token.is_empty() && self.seconds_until_expiry(now) > skew
    }
}

/// Load the cached token, or `None` if the file is absent. A present-but-corrupt file is an error
/// (so we don't silently treat a broken cache as "no token" and mask a real problem — the caller can
/// choose to fall through to interactive login).
pub fn load(path: &Path) -> Result<Option<StoredToken>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let token = serde_json::from_str(&raw)
                .with_context(|| format!("parsing token cache at {}", path.display()))?;
            Ok(Some(token))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading token cache at {}", path.display())),
    }
}

/// Persist the token to `path`, creating the parent dir if needed and enforcing `0600` on the file.
pub fn save(path: &Path, token: &StoredToken) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(token).context("serializing token")?;
    std::fs::write(path, json)
        .with_context(|| format!("writing token cache {}", path.display()))?;
    set_owner_only(path)?;
    Ok(())
}

/// Delete the token cache (for `--logout`). Absent file is a no-op success.
pub fn clear(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing token cache {}", path.display())),
    }
}

/// Restrict the file to owner read/write (`0600`) on Unix. No-op elsewhere.
#[cfg(unix)]
pub fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 on {}", path.display()))
}

#[cfg(not(unix))]
pub fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoredToken {
        StoredToken::from_response(
            TokenResponse {
                access_token: "at".into(),
                refresh_token: Some("rt".into()),
                token_type: "Bearer".into(),
                scope: Some("openid".into()),
                expires_in: Some(300),
                id_token: None,
            },
            1_000_000,
        )
    }

    #[test]
    fn from_response_computes_absolute_expiry() {
        let t = sample();
        assert_eq!(t.expires_at, 1_000_300);
        assert!(t.is_fresh(1_000_000, 60));
        assert!(!t.is_fresh(1_000_290, 60)); // within skew
        assert!(!t.is_fresh(1_000_400, 60)); // past expiry
    }

    #[test]
    fn round_trips_through_disk_and_is_mode_0600() {
        let dir = std::env::temp_dir().join(format!("lci-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("token.json");
        let original = sample();
        save(&path, &original).unwrap();

        let loaded = load(&path).unwrap().expect("token present");
        assert_eq!(loaded.access_token, original.access_token);
        assert_eq!(loaded.refresh_token, original.refresh_token);
        assert_eq!(loaded.expires_at, original.expires_at);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        }

        clear(&path).unwrap();
        assert!(
            load(&path).unwrap().is_none(),
            "cleared cache reads as None"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_loads_as_none() {
        let path = std::env::temp_dir().join("lci-does-not-exist-xyz/token.json");
        assert!(load(&path).unwrap().is_none());
    }
}
