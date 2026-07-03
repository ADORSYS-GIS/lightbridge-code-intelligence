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

use api::ApiClient;
use cli::Command;
use color_eyre::eyre::WrapErr as _;
use config::Config;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    // Install color-eyre's panic + error hooks EARLY, so any panic/error yields a pretty report.
    //
    // Terminal safety: `color_eyre::install()` sets a panic hook; later, `TerminalGuard::enter()`
    // takes that hook and prepends `restore()` (disable raw mode, leave alt-screen, DISABLE MOUSE
    // CAPTURE, show cursor) — so a panic mid-TUI restores the terminal BEFORE color-eyre prints,
    // never a corrupted screen. The error path is covered too: the guard's `Drop` restores the
    // terminal when `tui::run` returns (Ok or Err), before `main` returns `Err` and the runtime
    // prints the eyre report. `restore()` is idempotent, so this never double-restores badly.
    color_eyre::install()?;

    // Logs go to stderr and stay quiet by default (RUST_LOG overrides). The TUI owns the alternate
    // screen; tracing must not scribble over it, so keep it off unless explicitly asked.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let parsed = cli::parse(std::env::args().skip(1)).map_err(anyhow_to_eyre)?;

    if parsed.command == Command::Help {
        println!("{}", cli::USAGE);
        return Ok(());
    }

    // A hidden dev/review affordance: render a screen to text and exit. No auth, no network.
    if let Command::Render(spec) = &parsed.command {
        render::run(spec)
            .map_err(anyhow_to_eyre)
            .wrap_err("rendering the requested screen")?;
        return Ok(());
    }

    let cfg = Config::resolve(&parsed.flags)
        .map_err(anyhow_to_eyre)
        .wrap_err("resolving configuration")?;

    if parsed.command == Command::Logout {
        let path = Config::token_path().map_err(anyhow_to_eyre)?;
        auth::clear(&path).map_err(anyhow_to_eyre)?;
        println!("Logged out — token cache cleared ({}).", path.display());
        return Ok(());
    }

    let force_login = matches!(parsed.command, Command::Run { force_login: true });

    // Shared HTTP client (rustls via the workspace reqwest feature set).
    let http = reqwest::Client::builder()
        .user_agent(concat!("lci/", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building HTTP client")?;

    // --- Authenticate (prints to the normal terminal; must precede raw mode) ---
    let token = if force_login {
        auth::force_login(&http, &cfg).await
    } else {
        auth::ensure_token(&http, &cfg).await
    }
    .map_err(anyhow_to_eyre)
    .wrap_err("authenticating to the identity provider")?;

    let api = ApiClient::new(
        http.clone(),
        cfg.api_url.clone(),
        token.access_token.clone(),
    );

    // Identity + capabilities gate the UI's actions.
    let me = api
        .me()
        .await
        .map_err(anyhow_to_eyre)
        .wrap_err("fetching identity (/me) — is the token valid for this control plane?")?;

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
    .await
    .map_err(anyhow_to_eyre)
    .wrap_err("running the console")?;

    Ok(())
}

/// Bridge an `anyhow::Error` into a `color_eyre::eyre::Report` without flattening the source chain:
/// `anyhow::Error → Box<dyn Error + Send + Sync>` (which preserves the chain), then `Report::from`.
/// Our internal APIs use `anyhow`; only `main` speaks `eyre` (for the pretty report), so we convert
/// at that one boundary.
fn anyhow_to_eyre(err: anyhow::Error) -> color_eyre::eyre::Report {
    let boxed: Box<dyn std::error::Error + Send + Sync + 'static> = err.into();
    color_eyre::eyre::eyre!(boxed)
}
