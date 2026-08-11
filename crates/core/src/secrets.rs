//! Secret storage for OAuth API providers.
//!
//! Holds each provider's app credentials (`client_id`/`client_secret`) plus the tokens obtained
//! from a 3-legged OAuth flow (`access_token`/`refresh_token`/expiry/scope) and, when a userinfo
//! endpoint is configured, an author/principal URN. Persisted as JSON at
//! `~/.pacewright/secrets.json`, written **0600** (owner-only) since it holds bearer tokens.
//!
//! Wall-clock-free per the core convention: expiry checks take `now_ms` as a parameter (the daemon
//! passes `Clock::now_ms()`), so behaviour is deterministic and testable without a real clock.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One provider's stored credentials + tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderSecret {
    pub client_id: String,
    pub client_secret: String,
    /// OAuth endpoints for this provider, supplied at `oauth login` and reused on refresh so the
    /// engine hard-codes no platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    /// Optional userinfo endpoint; when set, login resolves an author/principal URN from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub userinfo_url: Option<String>,
    /// JSON field of the userinfo response holding the principal id (default `sub`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_field: Option<String>,
    /// Template for the author URN, `{id}` substituted (e.g. `urn:li:person:{id}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urn_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix **milliseconds** when the access token expires (matches `Clock::now_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Author/principal URN resolved once from the userinfo endpoint via `urn_template`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_urn: Option<String>,
}

/// The on-disk secret store, keyed by provider name.
#[derive(Debug, Clone, Default)]
pub struct SecretStore {
    path: PathBuf,
    providers: HashMap<String, ProviderSecret>,
}

impl SecretStore {
    /// Load the store from `path`. A missing file yields an empty store (first run).
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let providers = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing secrets file {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context(format!("reading secrets file {}", path.display())),
        };
        Ok(Self { path, providers })
    }

    /// Persist the store to its path, creating parent dirs and writing **0600**, atomically.
    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.providers).context("serializing secrets")?;
        write_0600(&self.path, &bytes)
    }

    /// The stored record for `provider`, if any.
    pub fn get(&self, provider: &str) -> Option<&ProviderSecret> {
        self.providers.get(provider)
    }

    /// The names of every provider with a stored record, sorted for stable output.
    pub fn providers(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Set (or replace) a provider's OAuth app credentials, preserving any existing tokens.
    pub fn set_app(
        &mut self,
        provider: &str,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) {
        let rec = self.providers.entry(provider.to_string()).or_default();
        rec.client_id = client_id.into();
        rec.client_secret = client_secret.into();
    }

    /// Set (or replace) a provider's OAuth endpoints + requested scope, preserving credentials and
    /// tokens. The engine hard-codes no platform; these come from `oauth login`.
    #[allow(clippy::too_many_arguments)]
    pub fn set_endpoints(
        &mut self,
        provider: &str,
        authorize_url: impl Into<String>,
        token_url: impl Into<String>,
        scope: impl Into<String>,
        userinfo_url: Option<String>,
        id_field: Option<String>,
        urn_template: Option<String>,
    ) {
        let rec = self.providers.entry(provider.to_string()).or_default();
        rec.authorize_url = Some(authorize_url.into());
        rec.token_url = Some(token_url.into());
        rec.scope = Some(scope.into());
        rec.userinfo_url = userinfo_url;
        rec.id_field = id_field;
        rec.urn_template = urn_template;
    }

    /// Record tokens from an OAuth exchange/refresh. Errors if the provider's app isn't set first.
    pub fn set_tokens(
        &mut self,
        provider: &str,
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_at_ms: i64,
        scope: Option<String>,
    ) -> Result<()> {
        let rec = self.providers.get_mut(provider).ok_or_else(|| {
            anyhow!("provider `{provider}` has no app credentials; set_app first")
        })?;
        rec.access_token = Some(access_token.into());
        // A refresh response may omit the refresh_token — keep the existing one in that case.
        if refresh_token.is_some() {
            rec.refresh_token = refresh_token;
        }
        rec.expires_at_ms = Some(expires_at_ms);
        if scope.is_some() {
            rec.scope = scope;
        }
        Ok(())
    }

    /// Clear a provider's tokens + author URN (keeps its app credentials). No-op if absent.
    pub fn clear_tokens(&mut self, provider: &str) {
        if let Some(rec) = self.providers.get_mut(provider) {
            rec.access_token = None;
            rec.refresh_token = None;
            rec.expires_at_ms = None;
            rec.scope = None;
            rec.author_urn = None;
        }
    }

    /// Record the resolved author URN for a provider. Errors if the provider isn't set.
    pub fn set_author_urn(&mut self, provider: &str, urn: impl Into<String>) -> Result<()> {
        let rec = self
            .providers
            .get_mut(provider)
            .ok_or_else(|| anyhow!("provider `{provider}` not set"))?;
        rec.author_urn = Some(urn.into());
        Ok(())
    }

    /// A currently-valid access token for `provider` — `Some` only if a token is stored and its
    /// expiry is more than `skew_ms` in the future relative to `now_ms`. Otherwise `None`.
    pub fn valid_access_token(&self, provider: &str, now_ms: i64, skew_ms: i64) -> Option<&str> {
        let rec = self.providers.get(provider)?;
        let token = rec.access_token.as_deref()?;
        let expires = rec.expires_at_ms?;
        (now_ms < expires - skew_ms).then_some(token)
    }

    /// True if `provider` has an access token that is expired (or within `skew_ms` of expiring) and
    /// therefore should be refreshed before use. A provider with no token returns `false` (it needs
    /// a full OAuth login, not a refresh).
    pub fn needs_refresh(&self, provider: &str, now_ms: i64, skew_ms: i64) -> bool {
        match self.providers.get(provider) {
            Some(rec) if rec.access_token.is_some() => {
                rec.expires_at_ms.is_none_or(|exp| now_ms >= exp - skew_ms)
            }
            _ => false,
        }
    }
}

/// Write `bytes` to `path` atomically (temp + rename) with owner-only (`0600`) permissions.
fn write_0600(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path() -> PathBuf {
        std::env::temp_dir().join(format!("pw-secrets-{}.json", uuid::Uuid::new_v4()))
    }

    #[test]
    fn save_then_load_roundtrips() {
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        s.set_app("acme", "cid", "csecret");
        s.set_tokens(
            "acme",
            "atok",
            Some("rtok".into()),
            1_800_000_000_000,
            Some("openid w_member_social".into()),
        )
        .unwrap();
        s.set_author_urn("acme", "urn:li:person:ACoAA123").unwrap();
        s.save().unwrap();

        let loaded = SecretStore::load(&p).unwrap();
        let rec = loaded.get("acme").unwrap();
        assert_eq!(rec.client_id, "cid");
        assert_eq!(rec.access_token.as_deref(), Some("atok"));
        assert_eq!(rec.refresh_token.as_deref(), Some("rtok"));
        assert_eq!(rec.expires_at_ms, Some(1_800_000_000_000));
        assert_eq!(rec.author_urn.as_deref(), Some("urn:li:person:ACoAA123"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let p = tmp_path();
        let s = SecretStore::load(&p).unwrap();
        assert!(s.get("acme").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        s.set_app("acme", "cid", "csecret");
        s.save().unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secrets file must be owner-only");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn valid_access_token_respects_expiry() {
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        s.set_app("acme", "cid", "csecret");
        s.set_tokens("acme", "atok", None, 1_000_000, None).unwrap();
        // 5-min skew (300_000 ms). Well before expiry → valid.
        assert_eq!(s.valid_access_token("acme", 500_000, 300_000), Some("atok"));
        // Inside the skew window before expiry → treated as invalid.
        assert_eq!(s.valid_access_token("acme", 800_000, 300_000), None);
        // After expiry → invalid.
        assert_eq!(s.valid_access_token("acme", 1_200_000, 300_000), None);
    }

    #[test]
    fn needs_refresh_true_when_expiring() {
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        s.set_app("acme", "cid", "csecret");
        s.set_tokens("acme", "atok", Some("r".into()), 1_000_000, None)
            .unwrap();
        assert!(!s.needs_refresh("acme", 500_000, 300_000));
        assert!(s.needs_refresh("acme", 800_000, 300_000));
        // A provider with no token doesn't "need refresh" (it needs full auth).
        s.set_app("globex", "y", "y");
        assert!(!s.needs_refresh("globex", 500_000, 300_000));
    }

    #[test]
    fn set_tokens_without_app_errs() {
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        assert!(s.set_tokens("acme", "a", None, 1, None).is_err());
    }

    #[test]
    fn clear_tokens_keeps_app_creds() {
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        s.set_app("acme", "cid", "csecret");
        s.set_tokens("acme", "atok", Some("rtok".into()), 1, Some("scope".into()))
            .unwrap();
        s.set_author_urn("acme", "urn:li:person:X").unwrap();
        s.clear_tokens("acme");
        let rec = s.get("acme").unwrap();
        // App credentials survive so a re-login doesn't need them re-supplied.
        assert_eq!(rec.client_id, "cid");
        assert_eq!(rec.client_secret, "csecret");
        // Everything token-shaped is gone.
        assert!(rec.access_token.is_none());
        assert!(rec.refresh_token.is_none());
        assert!(rec.expires_at_ms.is_none());
        assert!(rec.scope.is_none());
        assert!(rec.author_urn.is_none());
    }

    #[test]
    fn clear_tokens_missing_provider_is_noop() {
        let p = tmp_path();
        let mut s = SecretStore::load(&p).unwrap();
        s.clear_tokens("acme"); // must not panic
        assert!(s.get("acme").is_none());
    }
}
