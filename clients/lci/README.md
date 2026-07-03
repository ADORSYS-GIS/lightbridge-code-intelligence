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
| `t`            | cycle the color theme (midnight → terminal → nord)        |
| `?`            | toggle the help overlay                                   |

Approve / deny / cancel each open a **confirm dialog** with two buttons. Focus starts on the safe
**Cancel** button (a reflexive `Enter` never fires a destructive action); `←`/`→`/`Tab` move focus
between the buttons, `Enter` presses the focused one, `y` accepts regardless of focus, `Esc`/`n` cancel.
Actions you lack the permission for are dimmed in the header keymenu and refused with a toast. The header
shows your identity, effective capabilities, the API host, a connection dot, and a token-expiry countdown
(which turns **amber under two minutes**, plus a "re-auth needed" state if a background refresh fails).

## Look & themes

The interface is a k9s-/opencode-inspired layout: a fixed **header** (a `▍ LCI` wordmark, a `key: value`
context block, and a right-aligned keymenu), a **pill-tab bar** (the active view gets an accent
background), a bordered **content table** (bold header, an accent selection cursor, semantic status
colors — with short human labels like `indexing`/`done` so the STATUS column never truncates mid-word —
and right-aligned numeric/age columns), and a **status bar** (filter + a braille spinner while fetching,
a semantic-colored auto-dismissing toast, and a key hint). The status bar segments are content-sized and
truncate with an ellipsis (`…`) rather than a hard cut, so nothing clips mid-word on narrow terminals.
Empty states are inline status lines, not centered placards.

Three built-in themes ship, cyclable at runtime with `t` and selectable up-front via the `LCI_THEME` env
var or `theme =` in `config.toml`:

| Theme        | Feel                                                                             |
| ------------ | -------------------------------------------------------------------------------- |
| `midnight`   | **default** — Tokyo-Night-ish warm dark (purple accent, warm-orange brand)       |
| `terminal`   | transparent background — colors from your terminal's own 16 ANSI slots           |
| `nord`       | cool, muted arctic palette                                                       |

```bash
LCI_THEME=nord cargo run -p lci        # start in the nord theme
```

### Screens (rendered to text)

These are produced by the hidden dev/review affordance `lci --render <screen>` (no auth, no network —
seeded fake data), which draws a screen through ratatui's `TestBackend` and prints the buffer. Handy for
reviewing the layout in a PR or a terminal-less CI. Screens: `repos | runs | confirm | help | empty |
too-small`; tune with `--width`, `--height`, `--theme`.

Repositories (`lci --render repos --width 80 --height 24`):

```text
 ▍ LCI                    Host:  code-intelligence-api.a            <a> approve
 Lightbridge Code         User:  operator                              <d> deny
 Intelligence             Perms: approve / deny / cancel             <c> cancel
                          Token: 5m00s   ● connected                  <t> theme
                                                                       <?> help

  Repositories (5)   Runs (0)
╭▐ Repositories ▌ 5  [pending]─────────────────────────────────────────────────╮
│ REPOSITORY              STATUS       TASKS          LAST TASK APPROVED BY    │
│▌vymalo/lightbridge-code pending         12   2026-07-02 09:55 —              │
│ vymalo/ai-helm          approved        48   2026-07-02 09:55 operator       │
│ adorsys-gis/ai-governan pending          0                  — —              │
│ vymalo/home-os          disabled         3   2026-07-02 09:55 operator       │
│ vymalo/eaig             approved        21   2026-07-02 09:55 alice          │
╰──────────────────────────────────────────────────────────────────────────────╯
 filter: pending                            j/k move · f filter · r refresh · q…
```

Runs (`lci --render runs --width 80 --height 24`) — the STATUS column uses short labels
(`waiting_for_index` → `indexing`, `succeeded` → `done`) so it never truncates mid-word:

```text
  Repositories (0)   Runs (5)
╭▐ Runs ▌ 5  [all]─────────────────────────────────────────────────────────────╮
│ STATUS           REPOSITORY       TARGET       KIND        AGE JOB           │
│▌running          vymalo/lightbrid PR #128      review       1m review-9f2a   │
│ queued           vymalo/ai-helm   PR #44       review      20s —             │
│ indexing         adorsys-gis/ai-g issue #12    review       8s —             │
│ done             vymalo/eaig      PR #301      review       1h review-77c1   │
│ failed           vymalo/home-os   PR #9        review       2h review-4d0e   │
╰──────────────────────────────────────────────────────────────────────────────╯
 filter: all                                j/k move · f active/all · r refresh…
```

The approve confirm dialog (`lci --render confirm`), affirmative button focused:

```text
╭▐ Repositories ▌ 5  [pending]─────────────────────────────────────────────────╮
│ REPOSITORY     ╭ Confirm ───────────────────────────────────╮ APPROVED BY    │
│▌vymalo/lightbri│                                            │ —              │
│ vymalo/ai-helm │ Approve vymalo/lightbridge-code-intelligen │ operator       │
│ adorsys-gis/ai-│                                            │ —              │
│ vymalo/home-os │    Opens the gate and triggers indexing.   │ operator       │
│ vymalo/eaig    │                                            │ alice          │
│                │           › Approve ‹     Cancel           │                │
│                ╰────────────────────────────────────────────╯                │
╰──────────────────────────────────────────────────────────────────────────────╯
```

Empty state (`lci --render empty`) — an inline status line inside the frame:

```text
╭▐ Repositories ▌ 0  [pending]─────────────────────────────────────────────────╮
│ • no pending repositories — press f to change the filter, r to refresh       │
╰──────────────────────────────────────────────────────────────────────────────╯
```

Too-small terminal (`lci --render too-small --width 40 --height 10`) — a graceful line, never a panic:

```text
           terminal too small
              need ≥ 60×15
```

## Configuration

Precedence, lowest → highest: **built-in defaults < `config.toml` < environment < flags**.

| Setting        | Default                                            | Env                  | Flag           |
| -------------- | -------------------------------------------------- | -------------------- | -------------- |
| API base URL   | `https://code-intelligence-api.ai.camer.digital`   | `CONTROL_PLANE_URL`  | `--api-url`    |
| OIDC issuer    | `https://auth.verif.fyi/realms/camer-digital`      | `OIDC_ISSUER`        | `--issuer`     |
| OIDC client id | `lightbridge-cli`                                  | `OIDC_CLIENT_ID`     | `--client-id`  |
| Redirect port  | `8765`                                             | `LCI_REDIRECT_PORT`  | `--port`       |
| Scope          | `openid profile email` (fixed)                     | —                    | —              |
| Theme          | `midnight`                                         | `LCI_THEME`          | — (`t` at runtime) |

The optional config file lives at `<config_dir>/config.toml` (e.g. macOS
`~/Library/Application Support/fyi.camer.lci/config.toml`, Linux `~/.config/lci/config.toml`):

```toml
api_url   = "https://code-intelligence-api.ai.camer.digital"
issuer    = "https://auth.verif.fyi/realms/camer-digital"
client_id = "lightbridge-cli"
port      = 8765
theme     = "midnight"   # midnight | terminal | nord
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
