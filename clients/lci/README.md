# `lci` — Lightbridge Code Intelligence admin TUI

An interactive terminal client (binary `lci`) for the operator of a Lightbridge Code Intelligence
deployment. It replaces the web console's approval gate ([ADR-0063](../../docs/adr/0063-cli-only-repository-approval.md))
with three views:

- **Repositories** — list repos and **approve** / **deny** them (capability-gated on your token).
- **Runs** — watch active review/index tasks and **cancel** a running one.
- **Run Detail** — open a run (Enter on a Runs row) to see its full metadata, the posted review, and a
  **live-tailing transcript** (the agent's activity log), with autoscroll, a scrollbar, and mouse
  wheel support.

It authenticates to Keycloak with **OAuth 2.0 Authorization-Code + PKCE over a loopback redirect**,
caches the token in your OS data dir, and refreshes it silently. It talks only to the existing
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

### List views (Repositories / Runs)

| Key            | Action                                                    |
| -------------- | --------------------------------------------------------- |
| `q` / `Esc`    | quit                                                      |
| `Tab` / `1` / `2` | switch view (Repositories / Runs)                      |
| `↑`/`↓` or `j`/`k` | move the selection                                    |
| `Enter` / `l` / `→` | **open the selected run's detail page** (Runs only) |
| `r`            | refresh now                                               |
| `f`            | cycle filter (repos: all/pending/approved/disabled; runs: active/all) |
| `a`            | approve the selected repository (needs `repo:approve`)    |
| `d`            | deny the selected repository (needs `repo:deny`; purges its index) |
| `c`            | cancel the selected run (needs `task:cancel`)             |
| `t`            | cycle the color theme (midnight → terminal → nord)        |
| `m`            | toggle mouse capture (see below)                          |
| `?`            | toggle the help overlay                                   |

### Run Detail view (the log tail)

| Key            | Action                                                    |
| -------------- | --------------------------------------------------------- |
| `Esc` / `h` / `←` | back to the Runs list                                  |
| `↑`/`↓` or `j`/`k` | scroll the transcript a line                          |
| `PgUp` / `PgDn` | scroll a page                                            |
| `g` / `Home`   | jump to the top                                           |
| `G` / `End`    | jump to the bottom and **re-engage the live tail**        |
| `r`            | manual refresh (re-fetches metadata + review + transcript) |
| `m`            | toggle mouse capture                                       |
| `?`            | toggle the help overlay                                   |

Approve / deny / cancel each open a **confirm dialog** with two buttons. Focus starts on the safe
**Cancel** button (a reflexive `Enter` never fires a destructive action); `←`/`→`/`Tab` move focus
between the buttons, `Enter` presses the focused one, `y` accepts regardless of focus, `Esc`/`n` cancel.
Actions you lack the permission for are dimmed in the header keymenu and refused with a toast. The header
shows your identity, effective capabilities, the API host, a connection dot, and a token-expiry countdown
(which turns **amber under two minutes**, plus a "re-auth needed" state if a background refresh fails).

### Run Detail & the live transcript tail

Press `Enter` (or `l` / `→`) on a Runs row to open its **detail page**: a **meta** panel (id, status,
repo, target, kind, command, timestamps + a computed duration, job, and — on a failed run — the error
detail), a **review** panel (the summary + a colored `inline N · deferred N · out-of-scope N` tally +
the review permalink, or an inline "no review recorded (yet)"), and a large **transcript** panel — the
agent's activity log, rendered newest-at-bottom with a scrollbar.

While the run is **active** (received/queued/waiting-for-index/running/posting-result) the page polls
every ~2.5s and **auto-scrolls** to the bottom as new turns arrive (a `● live` badge shows in the
header). Scroll up and autoscroll disengages — your position is **held** and a `▼ N new` badge counts
the unseen turns; `G`/`End` jumps to the bottom and re-engages the tail. Once the run reaches a terminal
status the badge flips to `● done` / `● failed` and polling stops. The review + transcript are gated on
the `review:read` capability; without it the page shows an inline "insufficient permission (review:read)"
notice instead of fetching.

> **Scope note:** this tails the agent **transcript** (the activity log) via the existing API, not raw
> container/pod logs — the control plane exposes no `/logs` endpoint. A real container-log tail would
> need a new `GET /tasks/{id}/logs` endpoint + serve-role RBAC (a possible follow-up).

### Mouse & text selection

Mouse capture is **on** by default, so the scroll wheel drives the focused pane (the transcript in the
detail view; the table elsewhere). Capture, however, disables your terminal's **native text selection**
— so press **`m`** to toggle it off when you want to select/copy text (the status bar shows `mouse:on` /
`mouse:off`), and `m` again to turn it back on. Capture is always disabled again on exit (including the
panic/error path), so it never leaks into your shell.

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
reviewing the layout in a PR or a terminal-less CI. Screens: `repos`, `runs`, `detail`, `transcript`,
`confirm`, `help`, `empty`, `small`; tune with `--width`, `--height`, `--theme`. Run `lci --render`
with no name (or `lci --render list`) to print the valid names; an unknown name errors with that same
list. Example: `lci --render detail --width 120 --theme nord`.

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

Run Detail — a completed run with a review (`lci --render detail --width 80 --height 24`). The three
panels (meta / review / transcript) share collapsed borders so they read as one continuous page; the
transcript carries a scrollbar and, while live, a `▼ N new` badge when you've scrolled up:

```text
 ▍ LCI                    Host:  code-intelligence-api.a             <↵/l> open
 Lightbridge Code         User:  operator                            <Esc> back
 Intelligence             Perms: approve / deny / cancel               <G> tail
                          Token: 5m00s   ● connected                  <m> mouse
                                                                       <?> help

  Repositories (0)   Runs (5)  ▸  Run Detail
╭▐ Run 3f2504e0 ▌ ● done───────────────────────────────────────────────────────╮
│ status    done                          sha       —→—                        │
│ repo      vymalo/lightbridge-code-intellcreated   2026-07-03 08:43           │
│ target    PR #128                       started   2026-07-03 08:43           │
│ kind      review                        completed 2026-07-03 09:43           │
│ command   review                        duration  59m55s                     │
│                                         job       review-9f2a                │
╰──────────────────────────────────────────────────────────────────────────────╯
│▐ Review ▌                                                                    │
│ Solid change; two inline nits and one deferred concern about retry backoff.  │
│ inline 2  ·  deferred 1  ·  out-of-scope 0                                   │
╰──────────────────────────────────────────────────────────────────────────────╯
│▐ Transcript ▌ 4                                                              │
│ #0  assistant  ↑1240 ↓58                                                    █│
│ Starting the review. Let me read the diff and the surrounding files to groun║│
╰──────────────────────────────────────────────────────────────────────────────╯
 mouse:on · static                          j/k scroll · G bottom · m mouse · r…
```

Run Detail while live-tailing — an active run, no review yet (`lci --render transcript --width 120
--height 40`). The header shows `● live`, the transcript title shows `tailing`, and the review panel
shows the "no review recorded (yet)" line:

```text
  Repositories (0)   Runs (5)  ▸  Run Detail
╭▐ Run 3f2504e0 ▌ ● live───────────────────────────────────────────────────────────────────────────────────────────────╮
│ status    running                                            sha       —→—                                           │
│ repo      vymalo/lightbridge-code-intelligence               created   2026-07-03 09:42                              │
│ target    PR #128                                            started   2026-07-03 09:42                              │
│ kind      review                                             completed —                                             │
│ command   review                                             duration  1m30s                                         │
│                                                              job       review-9f2a                                   │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
│▐ Review ▌                                                                                                            │
│ • no review recorded (yet)                                                                                           │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
│▐ Transcript ▌ 4  tailing                                                                                             │
│ #0  assistant  ↑1240 ↓58                                                                                            █│
│ Starting the review. Let me read the diff and the surrounding files to ground the findings.                         █│
│                                                                                                                     █│
│ #1  tool                                                                                                            █│
│   ⚙ read_file {end, path, start}                                                                                    █│
│ #2  tool                                                                                                            █│
│   ⚙ search_code {k, query}                                                                                          █│
│ #3  assistant  ↑2980 ↓211                                                                                           █│
│ Two small nits (naming + an unused import) and one deferred concern: the retry loop has no jittered backoff, which c█│
│ n thundering-herd the IdP. Posting the review.                                                                      ║│
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 mouse:on · tail                                                  j/k scroll · G bottom · m mouse · r refresh · Esc bac…
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

Following the ratatui config-directories recipe, config and the token cache live in **separate**
locations: `config.toml` in the OS **config** dir, and the cached token in the OS **data** dir
(`<data_dir>/token.json`, e.g. macOS `~/Library/Application Support/fyi.camer.lci/token.json`, Linux
`~/.local/share/lci/token.json`), written `0600` via an atomic create-`0600`-then-rename (never a
world-readable window). The token stores an absolute `expires_at`; token values are never logged.
Editing or deleting `config.toml` never disturbs the cached session, and vice-versa.

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
- **Terminal safety + pretty reports** — `main` installs **color-eyre**'s panic + error hooks, then a
  panic hook *and* a `Drop` guard both restore the terminal first (disable raw mode, leave the alternate
  screen, **disable mouse capture**, show the cursor). So a panic or error yields a clean, readable
  color-eyre report instead of a corrupted terminal — and mouse capture never leaks into your shell.
- **No blocking on the render path** — every network call runs on a spawned task and posts its result
  back over a channel; the UI stays responsive and auto-refreshes the active view every 5s. The detail
  page's live tail polls every ~2.5s on its own timer, only while it's open and the run is still active.
