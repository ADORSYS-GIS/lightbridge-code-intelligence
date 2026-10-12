## 1. Summary

This PR changes:

- Registers `review_delivery.py` in `generate.py` to ensure it is generated as a dashboard JSON.
- Updates `review_delivery.py` to include detailed HTTP status dashboards (2xx, 4xx, 401/403, 5xx) to track review egress failures.
- Introduces an inline comment detail table to display precise findings (file, line, body) linked to outbound platform requests.
- Enables the new dashboard in the observability Helm chart `values.yaml`.

It solves:

- Lack of granular visibility into platform API response errors (400, 401, 422) during automated review delivery.
- Missing drill-down views to inspect specific inline comment payloads for failed or rejected posts.

---

## 2. Intent

The intent of this PR is:

> To allow operators and administrators to quickly diagnose why review posts to GitHub, GitLab, or Bitbucket are failing by exposing the exact HTTP response statuses and the associated inline comment payloads from the `outbox` and `review_comments` tables.

---

## 3. Scope

### In Scope

- Grafana foundation SDK Python generator updates in `tools/dashboard-gen`.
- Extension of `review_delivery` dashboard with new layout and panels.
- Enabling the dashboard in `values.yaml`.

### Out of Scope

- Changes to Rust control-plane egress logic.
- Adding structured HTTP status columns to the `outbox` table (relying on regex extraction from `last_error` for now).

---

## 4. Verification

I verified this change by:

- [x] Running automated tests
- [x] Running manual tests
- [ ] Checking logs
- [ ] Checking metrics
- [ ] Testing error cases
- [ ] Testing permissions/security behavior
- [ ] Testing rollback or failure behavior, if relevant

Commands run:

```bash
docker run --rm -v $(pwd):/app -w /app python:3.12 /bin/bash -c "pip install -r tools/dashboard-gen/requirements.txt && python tools/dashboard-gen/generate.py"
```

Results:

```text
wrote review-delivery.json
```

---

## 5. Screenshots / Evidence

Add evidence here:

* Dashboard JSON generated successfully.

---

## 6. Risk Assessment

Risk level:

* [x] Low
* [ ] Medium
* [ ] High

Potential risks:

* Dashboard JSON might conflict if others have uncommitted changes.

Mitigation:

* Deterministic JSON output via generator.

---

## 7. AI Usage Declaration

AI was used for:

* [x] Understanding existing code
* [x] Generating code
* [ ] Refactoring
* [ ] Generating tests
* [ ] Drafting documentation
* [ ] Reviewing the diff
* [ ] Not used

Human verification:

* [x] I understand every meaningful change in this PR
* [x] I checked generated code manually
* [x] I checked generated tests manually
* [ ] I removed unsupported AI assumptions
* [x] I accept responsibility for this PR

---

## 8. Reviewer Focus

Please focus your review on:

* [x] Correctness
* [ ] Architecture
* [ ] Security
* [ ] Performance
* [ ] Tests
* [x] Maintainability
* [x] Product intent
* [ ] Edge cases
