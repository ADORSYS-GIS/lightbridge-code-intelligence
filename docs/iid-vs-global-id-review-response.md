# Response: IID vs global ID in GitLab web URLs

**Reviewer's concern**: GitLab web URLs (e.g. MR page) need the **IID** (project-scoped ID), not the **global ID**. Does `target_id` store the IID?

**Answer: Yes, `target_id` stores the IID in both code paths.**

## MR webhook → `target_id`

`webhook.rs:243` reads `attrs["object_attributes"]["iid"]` (the IID) → `target_id` at line 361.

## Note webhook → `target_id`

`webhook.rs:488-493` reads `payload["merge_request"]["iid"]` or `payload["issue"]["iid"]` (the IID) → `target_id` at line 557.

## Why this is correct

Per [GitLab Merge Requests API docs](https://docs.gitlab.com/ee/api/merge_requests.html):
- **`id`** = globally unique ID (used by GitLab internally across all projects)
- **`iid`** = internal ID, scoped to the project (what users see in URLs like `/-/merge_requests/15`)

The webhook payloads also use `iid` — see [Merge request events](https://docs.gitlab.com/ee/user/project/integrations/webhook_events.html#merge-request-events) and [Note events](https://docs.gitlab.com/ee/user/project/integrations/webhook_events.html#note-events) docs.

Both webhook paths use `iid`, never `id`. The `target_id` in our DB holds the project-scoped IID, which matches what the frontend needs for web URLs.
