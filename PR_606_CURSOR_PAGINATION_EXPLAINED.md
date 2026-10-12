# PR #606 — `GET /repositories` cursor pagination, explained

How the implementation works end to end: what the migration does and why it exists, how the
frontend asks for a page, and what happens between the request arriving and the JSON going back.

Scope note: this covers `GET /repositories` only. `GET /tasks` (the Runs page) still uses offset
pagination (`?page=`/`?page_size=` + a `total` count) and is untouched by this work.

---

## 1. The problem in one paragraph

`GET /repositories` ran one query with no `LIMIT`: every connected repository, joined against every
task, aggregated, sorted, returned. It worked because installs were small. The fix is to serve one
page at a time — but the *ordering* is "most recently active first", and that ordering was computed
rather than stored, which is what makes this more than a `LIMIT` clause. Sections 2 and 3 are about
that.

---

## 2. Two ways to paginate, and why this endpoint needs the harder one

**Offset pagination** says "skip 40 rows, give me 20". It is trivial to implement and it can tell
you "page 3 of 12". Its weakness is that the skip is positional: if a row is inserted near the top
between two requests, everything shifts down by one and the reader sees a row twice (or misses one).
The cost also grows — `OFFSET 10000` makes the database walk and discard 10 000 rows.

**Keyset (cursor) pagination** says "give me 20 rows that sort after *this specific row*". The
boundary is a value, not a position, so an insert elsewhere in the table cannot shift it. The
database seeks straight to that value in an index and reads forward. That is the whole idea:

```
                    index on (last_task_at DESC, id DESC)
                    ├───────────────────────────────────────────────►
  page 1            ▼ start here
  [ row row row row row ]
                          ▲ cursor = this row's (last_task_at, id)
  page 2                  ▼ seek directly here, read 20 more
                          [ row row row row row ]
```

The phrase "seeks straight to that value in an index" is doing all the work in that sentence. It is
only true if the sort key is a **stored, indexed column**. That is the constraint that shapes
everything below.

---

## 3. Why there is a migration at all

### 3.1 What the old query sorted by

Before this PR, the sort key was produced by the query itself:

```sql
SELECT r.id, ..., MAX(t.created_at) AS last_task_at
  FROM repositories r LEFT JOIN tasks t ON t.repository_id = r.id
 GROUP BY r.id
 ORDER BY last_task_at DESC NULLS LAST, r.owner, r.name
```

`last_task_at` here is an **aggregate**. It does not exist until Postgres has joined every
repository to every one of its tasks and collapsed the groups. Two consequences follow, and both are
fatal for a cursor:

**(a) It cannot be indexed, so the cursor cannot seek.** A keyset predicate over an aggregate has to
be applied *after* the aggregation:

```sql
WITH activity AS ( ...the whole join + GROUP BY... )
SELECT * FROM activity
 WHERE (sort_key, id) < ($cursor_ts, $cursor_id)   -- filters an already-materialised set
 ORDER BY sort_key DESC, id DESC
 LIMIT $n
```

Postgres cannot push that `WHERE` down into the `GROUP BY`. So every page re-scans the entire
`repositories × tasks` join, aggregates all of it, sorts all of it, and then throws away everything
before the cursor to hand back 25 rows. Walking N pages costs N full scans. **That is worse than the
single unbounded query it was meant to replace.**

**(b) It moves, so the cursor can point at a row that is no longer there.** `MAX(t.created_at)`
changes the instant a repository receives a new task. Suppose you are on page 2 and repository X —
already returned on page 1 — gets a run. Its sort key jumps to *now*, so it moves to the very top of
the ordering. The keyset boundary you are holding is now positioned differently relative to X. Rows
get skipped or repeated. The classic rule is: **never paginate on a mutable sort key.** In the
article this design follows, the sort column is `products.created_at`, which is immutable. Ours was
not.

### 3.2 What the migration does about it

`0039_repositories_last_task_at.sql` moves the value from "computed per query" to "stored on the
row". Four statements, one job each.

**Statement 1 — define the column.**

```sql
ALTER TABLE repositories
    ADD COLUMN IF NOT EXISTS last_task_at TIMESTAMPTZ NOT NULL DEFAULT 'epoch';
```

`NOT NULL DEFAULT 'epoch'` is deliberate, and it is worth being precise about why. The natural
choice is a nullable column — NULL meaning "this repository has never run anything". But SQL's
three-valued logic makes NULL poison for a keyset comparison:

```sql
SELECT NULL < '2026-02-01'::timestamptz;   -- → NULL (UNKNOWN), not true, not false
```

A `WHERE` clause keeps only rows where the predicate is **true**. UNKNOWN is not true. So with a
nullable sort key, every never-run repository fails the cursor predicate on every page after the
first and silently vanishes from the list. Using `'epoch'` (1970-01-01T00:00:00Z) as the "never ran"
sentinel makes the comparison **total** — every row has a real, comparable value — and it naturally
sorts last under `DESC`, which is exactly where a never-run repository belongs.

**Statement 2 — populate history.**

```sql
UPDATE repositories r
   SET last_task_at = COALESCE(
       (SELECT MAX(t.created_at) FROM tasks t WHERE t.repository_id = r.id),
       'epoch'
   );
```

The `DEFAULT` only applies to rows inserted *after* the migration. Every repository that already
exists needs its real value computed once, from the same `MAX(created_at)` the old query used. This
runs once, at deploy.

**Statement 3 — make it seekable.**

```sql
CREATE INDEX IF NOT EXISTS repositories_last_task_at_id_idx
    ON repositories (last_task_at DESC, id DESC);
```

The column order and the sort direction match the query's `ORDER BY` exactly. That is what lets
Postgres treat the keyset predicate as an index seek instead of a filter.

**Statement 4 — keep it true.**

```sql
CREATE OR REPLACE FUNCTION repositories_track_last_task_at() RETURNS TRIGGER AS $$
BEGIN
    UPDATE repositories
       SET last_task_at = GREATEST(last_task_at, NEW.created_at)
     WHERE id = NEW.repository_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS tasks_track_repository_activity ON tasks;
CREATE TRIGGER tasks_track_repository_activity
    AFTER INSERT ON tasks
    FOR EACH ROW EXECUTE FUNCTION repositories_track_last_task_at();
```

A stored copy of a derived value is only useful while it stays correct. Tasks are inserted from more
than one place in `db/tasks.rs` (the ordinary webhook path, and the re-run path that computes its
own `run_epoch`), so an `UPDATE` written beside one `INSERT` would be a correctness bug waiting for
the next call site. The trigger cannot be forgotten.

`GREATEST` rather than plain assignment so a backdated insert cannot move a repository *backwards*
in the ordering — the column means "most recent run", not "most recent insert".

### 3.3 A note on the trade-off

This is denormalisation: the same fact now lives in two places (`tasks.created_at` and
`repositories.last_task_at`). That is a real cost — it buys index-seek pagination and pays for it
with a per-insert row update and a schema invariant to maintain. It is flagged in the PR's Reviewer
Focus for exactly that reason.

---

## 4. Why 0039 is so much longer than 0038

`0038_tasks_created_at_id_idx.sql` supports `/tasks`' pagination and is **one statement**:

```sql
CREATE INDEX IF NOT EXISTS tasks_created_at_id_idx ON tasks (created_at DESC, id DESC);
```

`0039` is four statements plus a function. The difference is not that repositories are more
complicated than tasks. It is that **the two migrations are doing different kinds of work.**

`/tasks` sorts by `tasks.created_at`. That column:

- already exists (`0001_init.sql`, `TIMESTAMPTZ NOT NULL DEFAULT now()`),
- is already correct for every historical row,
- is already maintained — the `DEFAULT now()` fills it on insert and nothing ever changes it.

So the *only* thing missing was an index. One statement, because three of the four jobs were already
done years earlier.

`/repositories` sorts by "most recent run", which was **not a column at all**. Introducing a stored
derived value carries four obligations, and a migration has to discharge every one:

| Obligation | 0038 (`/tasks`) | 0039 (`/repositories`) |
|---|---|---|
| **Define** the column | already exists since `0001_init` | `ALTER TABLE … ADD COLUMN` |
| **Backfill** existing rows | already correct | `UPDATE … SET last_task_at = MAX(...)` |
| **Index** it for the sort | ← the one statement 0038 adds | `CREATE INDEX` |
| **Maintain** it going forward | `DEFAULT now()` on insert, never mutated | trigger on `tasks` insert |

Same destination — an indexed `(timestamp, id)` the keyset can seek on. `/tasks` was three-quarters
of the way there already; `/repositories` started from zero.

### 4.1 The short version that does not work

The obvious objection: why not just add an index and skip the column?

```sql
-- does not help
CREATE INDEX ON tasks (repository_id, created_at DESC);
```

This speeds up *computing* `MAX(t.created_at)` per repository. It does not help the pagination at
all, because the thing being ordered and compared is the **output of the aggregate**, which does not
exist until after the `GROUP BY`. You cannot index a value the query invents. As long as the sort
key is derived, the keyset predicate has to filter a fully materialised result set — problem (a)
from §3.1, unchanged.

Expression indexes do not rescue it either: `CREATE INDEX ON repositories ((SELECT MAX(...)))` is
not valid, because the expression is not immutable and not a function of the indexed row alone.

That is why 0039 is long. The length is the price of making the sort key a real column; there is no
four-line version of this migration that actually works.

---

## 5. The full request flow

Nine steps, from a click to rendered cards.

```
 ┌─ browser ──────────────────────────────────────────────────────┐
 │ [1] RepoList: "Next" click → nuqs writes cursor into the URL   │
 └────────────────────────────┬───────────────────────────────────┘
                              │ navigation
 ┌─ Next.js server ───────────▼───────────────────────────────────┐
 │ [2] Repositories page: read + sanity-check searchParams        │
 │ [3] listRepositoriesPage(): build the query string             │
 └────────────────────────────┬───────────────────────────────────┘
                              │ HTTP + Bearer token
 ┌─ control plane (axum) ─────▼───────────────────────────────────┐
 │ [4] router → Caller + Query extractors                         │
 │ [5] RepositoriesQuery → RepositoriesParams  (validate, or 400) │
 │ [6] list_repositories_page(): one SQL statement                │
 │ [7] derive `next` from the extra row                           │
 │ [8] serialize {repositories, next}                             │
 └────────────────────────────┬───────────────────────────────────┘
                              │ JSON
 ┌─ Next.js server ───────────▼───────────────────────────────────┐
 │ [9] render RepoList with this page + `next`  → back to [1]     │
 └────────────────────────────────────────────────────────────────┘
```

### Step 1 — the cursor lives in the URL

`apps/web/components/repos/repo-list.tsx` is a client component. It holds no list state; it holds
only *where in the list we are*, and it keeps that in the URL via `nuqs`:

```tsx
const [{ q, after_activity_at }, setParams] = useQueryStates({
  q:                 { defaultValue: "", parse: String, clearOnDefault: true },
  after_activity_at: { defaultValue: "", parse: String, clearOnDefault: true },
  after_id:          { defaultValue: "", parse: String, clearOnDefault: true },
});
```

The "Next" button writes the cursor the server just handed back:

```tsx
<button
  disabled={!next}
  onClick={() =>
    next &&
    setParams({
      after_activity_at: next.after_activity_at,
      after_id: String(next.after_id),
    })
  }
>
  Next
</button>
```

Searching writes `q` **and clears the cursor in the same call** — that is why all three params live
in one `useQueryStates` object rather than three separate `useQueryState` hooks:

```tsx
<SearchInput
  value={q}
  onChange={(e) => setParams({ q: e.target.value, after_activity_at: null, after_id: null })}
/>
```

This matters. A cursor identifies a row *within a particular result set*. Carrying it across a
search change would resume in the middle of a list that no longer exists. Clearing it is what makes
"search resets to page 1" true by construction rather than by remembering to do it.

Because all of this is URL state, the resulting page is a real address —
`/dashboard/repositories?after_activity_at=…&after_id=41` — which is shareable, and which the
browser's Back button navigates correctly. That is why the pager has no "Previous" button and the
API has no `before_*` params: the browser already provides it.

### Step 2 — the Server Component reads the URL

`apps/web/app/dashboard/repositories/page.tsx` runs on the server on every request
(`export const dynamic = "force-dynamic"`), and turns the URL back into typed values:

```tsx
const sp = await searchParams;
const q = typeof sp.q === "string" && sp.q ? sp.q : undefined;
// Both halves or neither: the control plane rejects half a cursor rather than quietly serving the
// first page again.
const afterActivityAt = typeof sp.after_activity_at === "string" ? sp.after_activity_at : null;
const afterId = typeof sp.after_id === "string" ? Number(sp.after_id) : null;
const after =
  afterActivityAt && afterId !== null && Number.isInteger(afterId)
    ? { after_activity_at: afterActivityAt, after_id: afterId }
    : undefined;

const result = await listRepositoriesPage({ pageSize: REPOS_PAGE_SIZE, q, after });
```

`REPOS_PAGE_SIZE` (12) lives in `lib/domain/repos.ts` rather than in `RepoList`, because a
`"use client"` module cannot export a plain value to a Server Component — the same reason
`RUNS_PAGE_SIZE` sits beside the Runs view instead of inside it.

**This is the page size the reviewer asked for: a parameter the frontend chooses and sends, not a
server-side constant the client silently inherits.**

### Step 3 — the API client builds the query string

`apps/web/lib/server/api.ts`. This function replaced the old `listRepositories()` that looped over
pages internally; it now takes parameters and returns exactly one page, mirroring the existing
`listTasksPage`:

```ts
export async function listRepositoriesPage(
  params: RepositoriesPageParams,
): Promise<ApiResult<RepositoriesPageResponse>> {
  const query = new URLSearchParams({ page_size: String(params.pageSize) });
  if (params.q) query.set("q", params.q);
  if (params.after) {
    query.set("after_activity_at", params.after.after_activity_at);
    query.set("after_id", String(params.after.after_id));
  }

  try {
    const res = await authedFetch(`/repositories?${query.toString()}`);
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as RepositoriesPageResponse };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}
```

`authedFetch` attaches the OIDC access token from the httpOnly session cookie as a Bearer
credential. The URL on the wire:

```
GET /api/v2/repositories?page_size=12&q=vymalo&after_activity_at=2026-02-01T09%3A14%3A02Z&after_id=41
```

Note what that URL tells you just by reading it: 12 rows, matching "vymalo", starting after the
repository whose last run was at 09:14:02 with id 41. That legibility is the point of not
base64-encoding the cursor.

### Step 4 — routing and extraction

`services/control-plane/src/main.rs`:

```rust
.route("/repositories", get(tasks::list_repositories))
```

The handler signature declares what axum must produce before the body runs:

```rust
pub async fn list_repositories(
    caller: Caller,                       // authenticated identity + scopes
    State(state): State<AppState>,        // shared state (the DB pool)
    Query(query): Query<RepositoriesQuery>,  // parsed query string
) -> Response {
```

`Query<RepositoriesQuery>` deserializes the query string into:

```rust
#[derive(Debug, Deserialize)]
pub struct RepositoriesQuery {
    pub page_size: Option<i64>,
    pub q: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub after_activity_at: Option<OffsetDateTime>,
    pub after_id: Option<i64>,
}
```

This is the **first** validation layer, and it is free: it is type checking. `?page_size=abc` or
`?after_activity_at=not-a-date` never reaches the handler body — axum rejects the request with a 400
before it runs. The `with = "time::serde::rfc3339::option"` attribute is what parses
`2026-02-01T09:14:02Z` into an `OffsetDateTime`; `default` is required because a missing key must
produce `None` rather than a deserialization error.

Because that path is a wire contract and easy to break silently, it has a test that parses a real
URL rather than hand-building the struct:

```rust
#[test]
fn repositories_query_parses_a_cursor_from_the_url() {
    let uri: axum::http::Uri =
        "/repositories?page_size=10&q=shop&after_activity_at=2026-02-01T09:14:02Z&after_id=7"
            .parse()
            .expect("valid uri");
    let Query(query) = Query::<RepositoriesQuery>::try_from_uri(&uri).expect("parses");
    let params = RepositoriesParams::try_from(query).expect("valid");
    ...
}
```

Then the handler checks authorization and the pool:

```rust
if let Err(e) = caller.require("repo:read") {
    return e.into_response();
}
let Some(pool) = state.db.as_ref() else {
    return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
};
```

### Step 5 — semantic validation, in one place

Types are not enough: `page_size=-5` is a perfectly good `i64` and a nonsense page size. The
**second** validation layer is a typed conversion, and it is the only route from untrusted input to
the query layer:

```rust
/// Validated [`RepositoriesQuery`]. Building one is the only way into the query, so an out-of-range
/// page size or half a cursor cannot reach SQL.
#[derive(Debug)]
struct RepositoriesParams {
    page_size: i64,
    q: Option<String>,
    after: Option<(OffsetDateTime, i64)>,
}

impl TryFrom<RepositoriesQuery> for RepositoriesParams {
    type Error = (StatusCode, String);

    fn try_from(query: RepositoriesQuery) -> Result<Self, Self::Error> {
        // Rejected rather than clamped: a client asking for 500 and quietly receiving 100 has no way
        // to notice it is paging against a size it did not choose.
        let page_size = match query.page_size {
            None => DEFAULT_REPOSITORY_PAGE_SIZE,
            Some(size) if (1..=MAX_REPOSITORY_PAGE_SIZE).contains(&size) => size,
            Some(_) => {
                return Err(bad_request(format!(
                    "page_size must be between 1 and {MAX_REPOSITORY_PAGE_SIZE}"
                )));
            }
        };
        // Half a cursor is a client bug, not a first page — ignoring the given half would serve page
        // one forever.
        let after = match (query.after_activity_at, query.after_id) {
            (Some(activity_at), Some(id)) => Some((activity_at, id)),
            (None, None) => None,
            _ => {
                return Err(bad_request(
                    "after_activity_at and after_id must be given together".to_string(),
                ));
            }
        };
        Ok(Self {
            page_size,
            // Trimmed before it reaches the pattern: surrounding whitespace in a search box is
            // typing, not a term, and `ILIKE '% shop %'` would match nothing.
            q: query.q.map(|q| q.trim().to_string()).filter(|q| !q.is_empty()),
            after,
        })
    }
}
```

Three decisions worth naming:

- **`page_size` is rejected, not clamped.** The previous implementation used `.clamp(1, 100)`, so a
  client asking for 500 silently got 100 and had no way to detect that it was paging against a size
  it did not choose. A 400 with the valid range is information; a clamp is a lie.
- **Half a cursor is an error.** If only `after_id` arrives, the alternatives are to ignore it (the
  client silently re-reads page 1 forever) or to guess. Rejecting is the only option that surfaces
  the bug.
- **The type is the guarantee.** `RepositoriesParams` has private fields and one constructor. There
  is no code path where an unchecked `page_size` reaches SQL, and that is enforced by the compiler
  rather than by reviewer vigilance.

The handler body is then three lines of plumbing with no arithmetic in it:

```rust
let params = match RepositoriesParams::try_from(query) {
    Ok(params) => params,
    Err(rejection) => return rejection.into_response(),
};

match crate::db::list_repositories_page(pool, params.q.as_deref(), params.after, params.page_size).await {
```

### Step 6 — the query

`services/control-plane/src/db/repositories.rs`:

```rust
pub async fn list_repositories_page(
    pool: &PgPool,
    q: Option<&str>,
    after: Option<(OffsetDateTime, i64)>,
    page_size: i64,
) -> Result<(Vec<RepositoryRow>, Option<(OffsetDateTime, i64)>), sqlx::Error> {
    // One row past the page proves whether another page exists, so a full last page doesn't cost the
    // caller a request to discover an empty tail.
    let mut rows = sqlx::query_as::<_, RepositoryRow>(
        "SELECT r.id, r.platform_repo_id, r.platform, r.owner, r.name, r.default_branch, r.status, \
           (r.status = 'approved') AS active, r.approved_at, r.approved_by, \
           c.task_count, NULLIF(r.last_task_at, 'epoch') AS last_task_at \
         FROM repositories r \
         LEFT JOIN LATERAL ( \
           SELECT COUNT(*) AS task_count FROM tasks t WHERE t.repository_id = r.id \
         ) c ON TRUE \
         WHERE ($1::text IS NULL OR r.owner || '/' || r.name ILIKE '%' || $1 || '%') \
           AND ($2::timestamptz IS NULL OR (r.last_task_at, r.id) < ($2, $3)) \
         ORDER BY r.last_task_at DESC, r.id DESC \
         LIMIT $4",
    )
    .bind(q)
    .bind(after.map(|(last_task_at, _)| last_task_at))
    .bind(after.map(|(_, id)| id))
    .bind(page_size + 1)
    .fetch_all(pool)
    .await?;
```

Clause by clause:

**`ORDER BY r.last_task_at DESC, r.id DESC`** — matches `repositories_last_task_at_id_idx` exactly,
so this is an index scan, not a sort. `id` is the tie-break, and it is not decorative: every
never-run repository shares the `'epoch'` sort key, so without a unique final key the boundary
inside that group is undefined.

**`(r.last_task_at, r.id) < ($2, $3)`** — Postgres row-value comparison. It is equivalent to the
expanded form you will see in most tutorials:

```sql
-- these mean the same thing
(last_task_at, id) < ($2, $3)
last_task_at < $2 OR (last_task_at = $2 AND id < $3)
```

The row-value form is preferred here for two reasons: the planner matches it against a composite
index directly, and there is no way to fat-finger the `=`/`<` pattern in the expanded version. The
predicate reads as "strictly after this row in the `ORDER BY`" — `DESC` ordering means "after" is
"less than".

**`($2::timestamptz IS NULL OR …)`** — the first page binds `NULL` for both cursor halves, which
makes the predicate vacuously true and returns from the top. One statement serves both cases; no
branching, no second query string.

**`ILIKE '%' || $1 || '%'` over `r.owner || '/' || r.name`** — the search, matching the `owner/name`
slug case-insensitively. This is the same shape `list_tasks_page` already uses for its own `q`.

**`LEFT JOIN LATERAL (SELECT COUNT(*) …)`** — `task_count` still has to be counted per repository,
but a `LATERAL` subquery is evaluated **per output row**, so it runs `page_size + 1` times against
`tasks_repo_status_idx`. The old `LEFT JOIN tasks … GROUP BY r.id` fanned out every task row for
every repository before collapsing them — cost proportional to the whole tasks table, on every page.

**`NULLIF(r.last_task_at, 'epoch') AS last_task_at`** — the sentinel is an internal ordering device,
not part of the API. This maps it back to `NULL` on the way out, so the JSON keeps reporting "never
ran" as an absent timestamp and `RepoCard`'s existing `{repo.last_task_at && …}` guard is unchanged.

**`LIMIT $4` bound to `page_size + 1`** — see the next step.

### Step 7 — deriving `next` from a fact

```rust
    let has_next = rows.len() as i64 > page_size;
    rows.truncate(page_size as usize);
    // Back to the sentinel the ordering uses, so the next page resumes inside the never-run group
    // rather than restarting at its head.
    let next = has_next.then(|| rows.last()).flatten().map(|last| {
        (
            last.last_task_at.unwrap_or(OffsetDateTime::UNIX_EPOCH),
            last.id,
        )
    });
    Ok((rows, next))
}
```

Asking for one row more than the page needs turns "is there another page?" from a guess into an
observation. The previous implementation inferred it from a full page (`rows.len() == limit`), which
is wrong precisely when the last page happens to fill exactly: it hands back a cursor, and the
client spends a request to discover an empty result. The extra row is discarded before serializing.

The `unwrap_or(UNIX_EPOCH)` undoes the `NULLIF` from the projection. The cursor has to be expressed
in the same terms the `ORDER BY` uses, or paging across the never-run group breaks. `UNIX_EPOCH` in
`time` and `'epoch'` in Postgres are the same instant.

### Step 8 — the response

```rust
/// Where the next page starts. Serialized under the names the client sends them back as.
#[derive(Debug, Serialize)]
struct RepositoriesCursor {
    #[serde(with = "time::serde::rfc3339")]
    after_activity_at: OffsetDateTime,
    after_id: i64,
}

#[derive(Debug, Serialize)]
struct RepositoriesPage {
    repositories: Vec<crate::db::RepositoryRow>,
    /// `null` on the last page.
    next: Option<RepositoriesCursor>,
}
```

```json
{
  "repositories": [ { "id": 41, "owner": "vymalo", "name": "shop", "task_count": 12,
                      "last_task_at": "2026-02-01T09:14:02Z", ... } ],
  "next": { "after_activity_at": "2026-01-28T16:02:11Z", "after_id": 17 }
}
```

The cursor fields are serialized under **the exact names the client sends back**, so round-tripping
is copy-the-object rather than translate-the-object. `"next": null` means last page.

### Step 9 — render, and back to step 1

```tsx
<RepoList
  repos={result.data.repositories}
  next={result.data.next}
  ...
/>
```

`RepoList` renders the cards and enables "Next" only when `next` is non-null. Clicking it writes
those two values into the URL — which is step 1, and the loop closes.

---

## 6. A worked example

Four repositories. One has run a task; three never have:

| id | slug | `last_task_at` (stored) |
|---|---|---|
| 1 | `vymalo/shop` | `2026-02-01T09:14:02Z` |
| 2 | `vymalo/idle-0` | `epoch` |
| 3 | `vymalo/idle-1` | `epoch` |
| 4 | `vymalo/idle-2` | `epoch` |

`ORDER BY last_task_at DESC, id DESC` gives: **1, 4, 3, 2**. (The real timestamp first; then the
three epoch rows, broken by descending id.)

**Request 1** — `?page_size=2`

- Binds `NULL, NULL, 3` → cursor predicate is vacuously true, `LIMIT 3`.
- Rows back: `[1, 4, 3]` — three rows for a page size of two.
- `has_next = 3 > 2` → **true**. Truncate to `[1, 4]`.
- `next` = last row's key = `(epoch, 4)` → serialized `{"after_activity_at":"1970-01-01T00:00:00Z","after_id":4}`.

**Request 2** — `?page_size=2&after_activity_at=1970-01-01T00:00:00Z&after_id=4`

- Predicate `(last_task_at, id) < (epoch, 4)`:
  - id 1 → `2026-02-01 < epoch`? No. Excluded. ✓ (already seen)
  - id 4 → `(epoch, 4) < (epoch, 4)`? No. Excluded. ✓ (already seen)
  - id 3 → `epoch = epoch` and `3 < 4`? **Yes.** Included.
  - id 2 → `epoch = epoch` and `2 < 4`? **Yes.** Included.
- Rows back: `[3, 2]` — two rows, `LIMIT` was 3.
- `has_next = 2 > 2` → **false**. `next` = `null`.

Every repository appeared exactly once. This is what
`list_repositories_page_walks_the_null_activity_boundary` asserts.

### Why the tie-break is load-bearing

Run request 2 again with the `id` dropped from the predicate — `WHERE last_task_at < $2`:

- id 3 → `epoch < epoch`? **No.** Excluded.
- id 2 → `epoch < epoch`? **No.** Excluded.

Zero rows. The walk terminates believing it is done, and repositories 3 and 2 are **silently lost**.
That is not hypothetical — it is the observed failure when the tie-break is removed:

```
assertion `left == right` failed: every repository must appear exactly once
  left: [1, 4]
 right: [1, 2, 3, 4]
```

The whole never-run group shares one sort key, so the unique tie-break is the only thing separating
those rows. This is also why the epoch sentinel and the `id` tie-break are one design, not two
independent choices.

---

## 7. What was removed, and why

| Removed | Reason |
|---|---|
| `services/control-plane/src/http/cursor.rs` (86 lines + tests) | base64 over `"<rfc3339>\|<id>"` is opacity in appearance only — decodable by anyone, securing nothing. Its one real benefit is schema freedom for third-party clients; this API has one client, in the same repo, deployed together. GitHub's own commit pager (`?after=<sha>+34`) is plain text for the same reason. |
| `?status=` on `GET /repositories` | Added by the earlier revision with **no caller**. The web client never sent it, and the approval queue filters status on `/admin/repositories`, a different route with its own handler. |
| The page walk in `listRepositories()` | It converted a paginated endpoint back into an unbounded one at the caller: same total work, plus N round trips. |
| `apps/web/lib/hooks/use-pagination.ts` | Client-side slicing of a fully-fetched list. With server paging there is nothing left to slice, and nothing else imported it. |
| Client-side search filter in `RepoList` | Filtering a single page in the browser would only ever search the ~12 rows on screen — a silent wrong-results bug. Moved to the server as `q`. |

---

## 8. Where validation happens, in summary

Four layers, each catching what the one before it structurally cannot:

1. **Type** (serde/axum) — `page_size=abc`, `after_activity_at=yesterday` → 400 before the handler runs.
2. **Semantic** (`TryFrom`) — range, both-or-neither cursor halves, blank search → 400 with a message.
3. **Authorization** (`caller.require("repo:read")`) — 401/403.
4. **SQL** — every value is a bound parameter (`$1`…`$4`). No string interpolation anywhere, so the
   search term and the cursor cannot be injection vectors regardless of content.

---

## 9. What this does *not* cover

`GET /tasks` — the Runs page — still uses **offset** pagination and is untouched:

| | `/repositories` (this PR) | `/tasks` (unchanged) |
|---|---|---|
| Params | `page_size`, `q`, `after_activity_at`, `after_id` | `page`, `page_size`, `status`, `repository_id`, `q` |
| SQL | `WHERE (last_task_at, id) < ($2,$3) … LIMIT` | `LIMIT $4 OFFSET $5` |
| Response | `{repositories, next}` | `{tasks, total}` |
| UI | First / Next | Numbered pages (`1 / 12`) |

Deliberate, for two reasons. Converting it means deleting shipped, tested code (#587/#588) and
losing jump-to-page-N plus the `total` count that the Runs page's empty state and range label are
built on — a cursor structurally cannot provide a total. And the pressure is lower there:
`tasks.created_at` is immutable and already indexed (migration 0038), so offset drift is a much
smaller problem than it was for a mutable "last activity" key.

If the two are to be unified later, `/tasks` is the easier conversion of the two — its sort key
already satisfies every property this PR had to build from scratch.
