//! A tiny hand-rolled argument parser — the surface is small (a couple of subcommands + four
//! connection flags), so `clap` would be more dependency than it earns.
//!
//! Usage:
//! ```text
//! lci                 run the TUI (auth from cache, refresh, or interactive login)
//! lci login           force an interactive re-auth, then run
//! lci --logout        delete the cached token and exit
//! lci --help          print usage
//! ```
//! Connection overrides: `--api-url`, `--issuer`, `--client-id`, `--port` (each also has an env var;
//! flags win). See [`crate::config`].

use crate::config::Flags;
use anyhow::{bail, Result};

/// What the parsed command line asks us to do.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Run the TUI. `force_login` = the `login` subcommand.
    Run { force_login: bool },
    /// `--logout`: clear the token cache and exit.
    Logout,
    /// `--help`/`-h`: print usage and exit.
    Help,
}

/// The parsed invocation: a command plus any connection-flag overrides.
#[derive(Debug)]
pub struct Parsed {
    pub command: Command,
    pub flags: Flags,
}

/// Parse `args` (excluding argv[0]).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed> {
    let mut command = Command::Run { force_login: false };
    let mut flags = Flags::default();
    let mut iter = args.into_iter().peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "login" => command = Command::Run { force_login: true },
            "--logout" => command = Command::Logout,
            "--help" | "-h" => command = Command::Help,
            "--api-url" => flags.api_url = Some(take_value(&mut iter, "--api-url")?),
            "--issuer" => flags.issuer = Some(take_value(&mut iter, "--issuer")?),
            "--client-id" => flags.client_id = Some(take_value(&mut iter, "--client-id")?),
            "--port" => {
                let raw = take_value(&mut iter, "--port")?;
                flags.redirect_port = Some(
                    raw.parse()
                        .map_err(|_| anyhow::anyhow!("--port must be a number, got `{raw}`"))?,
                );
            }
            // `--flag=value` forms.
            other if other.starts_with("--api-url=") => flags.api_url = Some(after_eq(other)),
            other if other.starts_with("--issuer=") => flags.issuer = Some(after_eq(other)),
            other if other.starts_with("--client-id=") => flags.client_id = Some(after_eq(other)),
            other if other.starts_with("--port=") => {
                let raw = after_eq(other);
                flags.redirect_port = Some(
                    raw.parse()
                        .map_err(|_| anyhow::anyhow!("--port must be a number, got `{raw}`"))?,
                );
            }
            other => bail!("unknown argument `{other}` (try --help)"),
        }
    }

    Ok(Parsed { command, flags })
}

/// The usage text printed for `--help`.
pub const USAGE: &str = "\
lci — Lightbridge Code Intelligence admin TUI

USAGE:
    lci [SUBCOMMAND] [OPTIONS]

SUBCOMMANDS:
    (none)        run the TUI (cached token → silent refresh → interactive login)
    login         force an interactive re-auth, then run the TUI

OPTIONS:
    --logout              delete the cached token and exit
    --api-url <URL>       control-plane base URL       (env CONTROL_PLANE_URL)
    --issuer <URL>        OIDC issuer                   (env OIDC_ISSUER)
    --client-id <ID>      OIDC public client id         (env OIDC_CLIENT_ID)
    --port <PORT>         loopback redirect port        (env LCI_REDIRECT_PORT)
    -h, --help            print this help

Config precedence (low → high): defaults < ~/.config/lci/config.toml < env < flags.";

fn take_value<I: Iterator<Item = String>>(iter: &mut I, flag: &str) -> Result<String> {
    iter.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn after_eq(arg: &str) -> String {
    arg.split_once('=').map(|x| x.1).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Parsed {
        parse(args.iter().map(|s| s.to_string())).unwrap()
    }

    #[test]
    fn no_args_runs_tui() {
        let p = parse_str(&[]);
        assert_eq!(p.command, Command::Run { force_login: false });
    }

    #[test]
    fn login_subcommand_forces_login() {
        assert_eq!(
            parse_str(&["login"]).command,
            Command::Run { force_login: true }
        );
    }

    #[test]
    fn logout_and_help() {
        assert_eq!(parse_str(&["--logout"]).command, Command::Logout);
        assert_eq!(parse_str(&["--help"]).command, Command::Help);
        assert_eq!(parse_str(&["-h"]).command, Command::Help);
    }

    #[test]
    fn parses_connection_flags_both_forms() {
        let p = parse_str(&["--api-url", "https://a.test", "--port=9999"]);
        assert_eq!(p.flags.api_url.as_deref(), Some("https://a.test"));
        assert_eq!(p.flags.redirect_port, Some(9999));
    }

    #[test]
    fn rejects_unknown_and_bad_port() {
        assert!(parse(["--nope".to_string()]).is_err());
        assert!(parse(["--port".to_string(), "abc".to_string()]).is_err());
    }
}
