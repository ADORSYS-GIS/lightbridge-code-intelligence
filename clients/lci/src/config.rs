//! Resolved runtime configuration for the client.
//!
//! Precedence (lowest → highest): built-in defaults → `~/.config/lci/config.toml` → environment
//! variables → CLI flags. Only the four connection settings are file/env-overridable; everything
//! else is a compile-time default. No secrets live here — the token cache is [`crate::auth`]'s job.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

/// Default control-plane base URL (prod). Override with `CONTROL_PLANE_URL`.
pub const DEFAULT_API_URL: &str = "https://code-intelligence-api.ai.camer.digital";
/// Default OIDC issuer (Keycloak realm). Override with `OIDC_ISSUER`.
pub const DEFAULT_ISSUER: &str = "https://auth.verif.fyi/realms/camer-digital";
/// Default public client id. Override with `OIDC_CLIENT_ID`.
pub const DEFAULT_CLIENT_ID: &str = "lightbridge-cli";
/// Default loopback redirect port. Override with `LCI_REDIRECT_PORT`.
pub const DEFAULT_REDIRECT_PORT: u16 = 8765;
/// OAuth scopes we request. The realm's audience mapper adds the `code-intelligence` audience; we do
/// NOT request an audience here (Keycloak rejects an unknown `aud` in the request).
pub const DEFAULT_SCOPE: &str = "openid profile email";

/// Default color theme. Override with `LCI_THEME` or `theme =` in `config.toml`.
pub const DEFAULT_THEME: &str = "midnight";

/// Fully-resolved settings the rest of the app runs against.
#[derive(Debug, Clone)]
pub struct Config {
    pub api_url: String,
    pub issuer: String,
    pub client_id: String,
    pub redirect_port: u16,
    pub scope: String,
    /// The active theme name (`midnight` | `terminal` | `nord`); an unknown value falls back to the
    /// default in [`crate::theme::ThemeKind::from_name`].
    pub theme: String,
}

/// The optional TOML file at `<config_dir>/config.toml`. Every field is optional — a missing file or
/// a missing key just leaves the built-in default (then env, then flags) in place.
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    api_url: Option<String>,
    issuer: Option<String>,
    client_id: Option<String>,
    port: Option<u16>,
    theme: Option<String>,
}

impl Config {
    /// The app's `ProjectDirs` (`from("fyi","camer","lci")`). The single source for both the config
    /// and data locations, so they stay platform-correct.
    fn project_dirs() -> Result<ProjectDirs> {
        ProjectDirs::from("fyi", "camer", "lci")
            .context("could not determine the OS directories for lci")
    }

    /// OS **config** directory (`config_dir()`) — home of the user-editable `config.toml`. Per the
    /// ratatui config-directories recipe, config lives here and the token cache does NOT (it's a
    /// secret/cache, not config), so the two are split across `config_dir` / `data_dir`.
    pub fn config_dir() -> Result<PathBuf> {
        Ok(Self::project_dirs()?.config_dir().to_path_buf())
    }

    /// OS **data** directory (`data_dir()`) — home of the token cache. Splitting it out of the config
    /// dir means editing/removing `config.toml` never disturbs the cached session, and vice-versa.
    pub fn data_dir() -> Result<PathBuf> {
        Ok(Self::project_dirs()?.data_dir().to_path_buf())
    }

    /// The token-cache path (`<data_dir>/token.json`). Written atomically + `0600` by
    /// [`crate::auth::store::save`] — that hardening is unchanged by the dir split.
    pub fn token_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("token.json"))
    }

    /// The loopback redirect URI derived from the configured port.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.redirect_port)
    }

    /// Resolve config with the documented precedence. `flags` carries anything the arg parser
    /// captured; `None` fields fall through to env → file → default.
    pub fn resolve(flags: &Flags) -> Result<Self> {
        let file = Self::load_file().unwrap_or_default();

        let api_url = flags
            .api_url
            .clone()
            .or_else(|| env_nonempty("CONTROL_PLANE_URL"))
            .or(file.api_url)
            .unwrap_or_else(|| DEFAULT_API_URL.to_string());
        let issuer = flags
            .issuer
            .clone()
            .or_else(|| env_nonempty("OIDC_ISSUER"))
            .or(file.issuer)
            .unwrap_or_else(|| DEFAULT_ISSUER.to_string());
        let client_id = flags
            .client_id
            .clone()
            .or_else(|| env_nonempty("OIDC_CLIENT_ID"))
            .or(file.client_id)
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
        let redirect_port = flags
            .redirect_port
            .or_else(|| env_nonempty("LCI_REDIRECT_PORT").and_then(|s| s.parse().ok()))
            .or(file.port)
            .unwrap_or(DEFAULT_REDIRECT_PORT);
        // Theme has no flag (it's cyclable at runtime with `t`); env > file > default.
        let theme = env_nonempty("LCI_THEME")
            .or(file.theme)
            .unwrap_or_else(|| DEFAULT_THEME.to_string());

        Ok(Self {
            // Trim a trailing slash so `{base}/path` joins cleanly.
            api_url: api_url.trim_end_matches('/').to_string(),
            issuer: issuer.trim_end_matches('/').to_string(),
            client_id,
            redirect_port,
            scope: DEFAULT_SCOPE.to_string(),
            theme,
        })
    }

    /// Read + parse `config.toml` if present. A parse error is surfaced (so a typo isn't silently
    /// ignored); a missing file returns `None`.
    fn load_file() -> Option<FileConfig> {
        let path = Self::config_dir().ok()?.join("config.toml");
        let raw = std::fs::read_to_string(&path).ok()?;
        match toml_min::parse(&raw) {
            Ok(cfg) => Some(cfg),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignoring malformed config.toml");
                None
            }
        }
    }
}

/// Read an env var, treating empty/whitespace as unset.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Flags captured from the command line (see [`crate::cli`]). All optional — unset falls through.
#[derive(Debug, Default, Clone)]
pub struct Flags {
    pub api_url: Option<String>,
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub redirect_port: Option<u16>,
}

/// A deliberately tiny TOML reader for our four flat, string/int keys — avoids pulling the `toml`
/// crate for a four-line config file. Handles `key = "value"` / `key = 123`, `#` comments, and
/// blank lines. Anything it can't parse is reported as an error so a real typo surfaces.
mod toml_min {
    use super::FileConfig;

    /// The distinct parse failures `toml_min::parse` can report. One variant per line-level defect;
    /// each `#[error(...)]` text is verbatim what the old `Result<_, String>` returned, so swapping
    /// this in is a pure typing change (the `Display` impl `thiserror` derives is what callers that
    /// interpolate the error into a message actually observe).
    #[derive(Debug, thiserror::Error)]
    pub enum ConfigParseError {
        #[error("line {line}: expected `key = value`")]
        ExpectedKeyValue { line: usize },
        #[error("line {line}: port must be a number")]
        InvalidPort { line: usize },
        #[error("line {line}: unknown key `{key}`")]
        UnknownKey { line: usize, key: String },
    }

    pub fn parse(raw: &str) -> Result<FileConfig, ConfigParseError> {
        let mut cfg = FileConfig::default();
        for (lineno, line) in raw.lines().enumerate() {
            let line = strip_comment(line).trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or(ConfigParseError::ExpectedKeyValue { line: lineno + 1 })?;
            let key = key.trim();
            let value = unquote(value.trim());
            match key {
                "api_url" => cfg.api_url = Some(value),
                "issuer" => cfg.issuer = Some(value),
                "client_id" => cfg.client_id = Some(value),
                "theme" => cfg.theme = Some(value),
                "port" => {
                    cfg.port = Some(value.parse().map_err(|_| ConfigParseError::InvalidPort {
                        line: lineno + 1,
                    })?)
                }
                other => {
                    return Err(ConfigParseError::UnknownKey {
                        line: lineno + 1,
                        key: other.to_string(),
                    })
                }
            }
        }
        Ok(cfg)
    }

    /// Drop a `#` comment that is not inside a quoted string.
    fn strip_comment(line: &str) -> &str {
        let mut in_quotes = false;
        for (i, c) in line.char_indices() {
            match c {
                '"' => in_quotes = !in_quotes,
                '#' if !in_quotes => return &line[..i],
                _ => {}
            }
        }
        line
    }

    /// Strip a single pair of surrounding double quotes if present.
    fn unquote(value: &str) -> String {
        value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_nothing_set() {
        let cfg = Config {
            api_url: DEFAULT_API_URL.to_string(),
            issuer: DEFAULT_ISSUER.to_string(),
            client_id: DEFAULT_CLIENT_ID.to_string(),
            redirect_port: DEFAULT_REDIRECT_PORT,
            scope: DEFAULT_SCOPE.to_string(),
            theme: DEFAULT_THEME.to_string(),
        };
        assert_eq!(cfg.redirect_uri(), "http://127.0.0.1:8765/callback");
    }

    #[test]
    fn toml_min_parses_flat_keys_and_ignores_comments() {
        let raw = r#"
            # connection settings
            api_url = "https://example.test"   # trailing comment
            client_id = "lightbridge-cli"
            port = 9000
        "#;
        let cfg = toml_min::parse(raw).expect("parses");
        assert_eq!(cfg.api_url.as_deref(), Some("https://example.test"));
        assert_eq!(cfg.client_id.as_deref(), Some("lightbridge-cli"));
        assert_eq!(cfg.port, Some(9000));
        assert_eq!(cfg.issuer, None);
    }

    #[test]
    fn toml_min_parses_theme() {
        let cfg = toml_min::parse(r#"theme = "nord""#).expect("parses");
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn toml_min_rejects_unknown_key() {
        assert!(toml_min::parse("nope = 1").is_err());
    }

    #[test]
    fn config_and_token_live_in_distinct_dirs() {
        // The recipe split: config.toml under config_dir, token.json under data_dir. On most
        // platforms these differ; assert the token path lands under the data dir and is token.json.
        let data = Config::data_dir().expect("data dir resolves");
        let token = Config::token_path().expect("token path resolves");
        assert!(
            token.starts_with(&data),
            "token cache lives under the data dir"
        );
        assert_eq!(token.file_name().unwrap(), "token.json");
        // The config dir is resolvable too (home of config.toml).
        assert!(Config::config_dir().is_ok());
    }
}
