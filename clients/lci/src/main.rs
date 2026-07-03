//! `lci` — the operator's terminal admin client for Lightbridge Code Intelligence (ADR-0063).
//!
//! Boot order:
//! 1. Parse args + resolve config (defaults < config.toml < env < flags).
//! 2. Authenticate to Keycloak (cached token → silent refresh → Authorization-Code + PKCE loopback
//!    login). This prints to the *normal* terminal, so it happens BEFORE raw mode.
//! 3. Fetch `/me` for identity + capabilities.
//! 4. Enter the ratatui TUI (approve/deny repos, watch runs) with a guaranteed terminal restore.

mod api;
mod auth;
mod cli;
mod config;
mod render;
mod theme;
mod tui;

use anyhow::{Context, Result};
use api::ApiClient;
use cli::Command;
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr and stay quiet by default (RUST_LOG overrides). The TUI owns the alternate
    // screen; tracing must not scribble over it, so keep it off unless explicitly asked.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let parsed = cli::parse(std::env::args().skip(1))?;

    if parsed.command == Command::Help {
        println!("{}", cli::USAGE);
        return Ok(());
    }

    // A hidden dev/review affordance: render a screen to text and exit. No auth, no network.
    if let Command::Render(spec) = &parsed.command {
        render::run(spec)?;
        return Ok(());
    }

    let cfg = Config::resolve(&parsed.flags)?;

    if parsed.command == Command::Logout {
        let path = Config::token_path()?;
        auth::clear(&path)?;
        println!("Logged out — token cache cleared ({}).", path.display());
        return Ok(());
    }

    let force_login = matches!(parsed.command, Command::Run { force_login: true });

    // Shared HTTP client (rustls via the workspace reqwest feature set).
    let http = reqwest::Client::builder()
        .user_agent(concat!("lci/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    // --- Authenticate (prints to the normal terminal; must precede raw mode) ---
    let token = if force_login {
        auth::force_login(&http, &cfg).await?
    } else {
        auth::ensure_token(&http, &cfg).await?
    };

    let api = ApiClient::new(
        http.clone(),
        cfg.api_url.clone(),
        token.access_token.clone(),
    );

    // Identity + capabilities gate the UI's actions.
    let me = api
        .me()
        .await
        .context("fetching identity (/me) — is the token valid for this control plane?")?;

    println!(
        "Signed in as {} ({} permissions). Starting the console…",
        me.identity(),
        me.permissions.len()
    );

    // --- Enter the TUI (terminal restore guaranteed by tui::run's guard) ---
    let theme_kind = theme::ThemeKind::from_name(&cfg.theme);
    tui::run(
        api,
        me,
        token.expires_at,
        cfg,
        http,
        token.refresh_token.clone(),
        theme_kind,
    )
    .await?;

    Ok(())
}
