# `lci` — Lightbridge Code Intelligence admin TUI

An interactive terminal client (binary `lci`) for the operator of a Lightbridge Code Intelligence
deployment. It replaces the web console's approval gate ([ADR-0063](../../docs/adr/0063-cli-only-repository-approval.md))
with two views:

- **Repositories** — list repos and **approve** / **deny** them (capability-gated on your token).
- **Runs** — watch active review/index tasks and **cancel** a running one.

It authenticates to Keycloak with **OAuth 2.0 Authorization-Code + PKCE over a loopback redirect**,
caches the token in your OS config dir, and refreshes it silently. It talks only to the existing
permission-gated control-plane endpoints — no new server surface, no new trust path.

## Running

```bash
cargo run -p lci                 # run the TUI (cached token → silent refresh → interactive login)
cargo run -p lci -- login        # force an interactive re-auth, then run
cargo run -p lci -- --logout     # delete the cached token and exit
cargo run -p lci -- --help       # usage
```

On first run (or when the cached token is unusable) `lci` prints an authorize URL, opens your browser,
and waits for the redirect on `http://127.0.0.1:8765/callback`. Sign in; the tab shows "Signed in" and
the TUI starts. The login prints to the normal terminal — it happens *before* the alternate screen, so
your scrollback is untouched.

## Keybindings

| Key            | Action                                                    |
| -------------- | --------------------------------------------------------- |
| `q` / `Esc`    | quit                                                      |
| `Tab` / `1` / `2` | switch view (Repositories / Runs)                      |
| `↑`/`↓` or `j`/`k` | move the selection                                    |
| `r`            | refresh now                                               |
| `f`            | cycle filter (repos: all/pending/approved/disabled; runs: active/all) |
| `a`            | approve the selected repository (needs `repo:approve`)    |
| `d`            | deny the selected repository (needs `repo:deny`; purges its index) |
| `c`            | cancel the selected run (needs `task:cancel`)             |
| `?`            | toggle the help overlay                                   |

Approve / deny / cancel each ask for confirmation (`Enter`/`y` to confirm, `Esc`/`n` to cancel). Actions
you lack the permission for are refused with a toast; the status bar shows your identity, effective
capabilities, the API host, and a token-expiry countdown (plus a "re-auth needed" indicator if a
background refresh fails).

## Configuration

Precedence, lowest → highest: **built-in defaults < `config.toml` < environment < flags**.

| Setting        | Default                                            | Env                  | Flag           |
| -------------- | -------------------------------------------------- | -------------------- | -------------- |
| API base URL   | `https://code-intelligence-api.ai.camer.digital`   | `CONTROL_PLANE_URL`  | `--api-url`    |
| OIDC issuer    | `https://auth.verif.fyi/realms/camer-digital`      | `OIDC_ISSUER`        | `--issuer`     |
| OIDC client id | `lightbridge-cli`                                  | `OIDC_CLIENT_ID`     | `--client-id`  |
| Redirect port  | `8765`                                             | `LCI_REDIRECT_PORT`  | `--port`       |
| Scope          | `openid profile email` (fixed)                     | —                    | —              |

The optional config file lives at `<config_dir>/config.toml` (e.g. macOS
`~/Library/Application Support/fyi.camer.lci/config.toml`, Linux `~/.config/lci/config.toml`):

```toml
api_url   = "https://code-intelligence-api.ai.camer.digital"
issuer    = "https://auth.verif.fyi/realms/camer-digital"
client_id = "lightbridge-cli"
port      = 8765
```

The cached token is written to `<config_dir>/token.json` with `0600` permissions. It stores an absolute
`expires_at`; token values are never logged.

## Keycloak setup (operator, one-time)

`lci` is **built and testable today**, but it will **not authenticate against prod until a public client
`lightbridge-cli` exists** in realm `camer-digital` (at `auth.verif.fyi`). The client must be a **public**
(no-secret) client with the standard code flow + PKCE, loopback redirect URIs, and — critically — **the same
client scopes the web client uses that emit the `code-intelligence` audience and the `permissions` claim**.
Without those scopes the control plane will reject the token (wrong audience) or see no capabilities (empty
`permissions`), and every action will 401/403.

### Option 1 — `kcadm.sh`

```bash
# Authenticate kcadm to the realm's admin (adjust server + admin creds).
kcadm.sh config credentials \
  --server https://auth.verif.fyi \
  --realm master --user "$KC_ADMIN" --password "$KC_ADMIN_PASSWORD"

# Create the public client with PKCE + loopback redirects.
kcadm.sh create clients -r camer-digital -f - <<'JSON'
{
  "clientId": "lightbridge-cli",
  "name": "Lightbridge CLI (loopback PKCE public client)",
  "enabled": true,
  "protocol": "openid-connect",
  "publicClient": true,
  "standardFlowEnabled": true,
  "implicitFlowEnabled": false,
  "directAccessGrantsEnabled": false,
  "serviceAccountsEnabled": false,
  "redirectUris": [
    "http://127.0.0.1:8765/callback",
    "http://localhost:8765/callback"
  ],
  "attributes": {
    "pkce.code.challenge.method": "S256"
  }
}
JSON

# Attach the SAME audience + permissions scopes the web client (`lightbridge-web`) uses, so the token
# carries the `code-intelligence` audience and the `permissions` claim. Replace <scope> with each of
# those default client scopes as configured in this realm.
CID=$(kcadm.sh get clients -r camer-digital -q clientId=lightbridge-cli --fields id --format csv --noquotes)
kcadm.sh update clients/$CID/default-client-scopes/<scope-id> -r camer-digital
```

> The exact scope names/ids are realm-specific; mirror whatever `lightbridge-web` has under
> **Client scopes → Default**. The audience mapper adds `code-intelligence` to the access token; the
> permissions mapper emits the `permissions` claim ([ADR-0023](../../docs/adr/0023-db-backed-rbac.md)).

### Option 2 — realm-JSON client stanza

Model it on `deploy/keycloak/realm-lightbridge.json`'s `lightbridge-web` client — same shape, loopback
redirect URIs, and the audience mapper. Add this object to the realm's `clients` array (and attach the
web client's audience + permissions scopes, or inline the mappers as `protocolMappers`):

```json
{
  "clientId": "lightbridge-cli",
  "name": "Lightbridge CLI (loopback PKCE public client)",
  "enabled": true,
  "protocol": "openid-connect",
  "publicClient": true,
  "standardFlowEnabled": true,
  "implicitFlowEnabled": false,
  "directAccessGrantsEnabled": false,
  "serviceAccountsEnabled": false,
  "redirectUris": [
    "http://127.0.0.1:8765/callback",
    "http://localhost:8765/callback"
  ],
  "webOrigins": [],
  "attributes": {
    "pkce.code.challenge.method": "S256"
  },
  "protocolMappers": [
    {
      "name": "code-intelligence-audience",
      "protocol": "openid-connect",
      "protocolMapper": "oidc-audience-mapper",
      "consentRequired": false,
      "config": {
        "included.custom.audience": "code-intelligence",
        "id.token.claim": "false",
        "access.token.claim": "true"
      }
    }
  ]
}
```

> The `permissions` claim ([ADR-0023](../../docs/adr/0023-db-backed-rbac.md)) is emitted by the realm's
> existing permissions mapper/scope — reuse the same one the web client relies on rather than duplicating
> it here. `lci` requests scope `openid profile email` only; it does **not** request an audience (Keycloak
> rejects an unknown `aud`/`resource` in the authorize request — the mapper adds it server-side).

## Design notes

- **Hand-rolled OAuth** — PKCE is `base64url(sha256(verifier))` and the code/token exchange is a plain
  reqwest POST, so there's no `oauth2` crate dependency. See `src/auth/`.
- **Terminal safety** — a panic hook *and* a `Drop` guard both restore the terminal (leave the alternate
  screen, disable raw mode, show the cursor), so a crash never leaves your terminal wrecked.
- **No blocking on the render path** — every network call runs on a spawned task and posts its result
  back over a channel; the UI stays responsive and auto-refreshes the active view every 5s.
