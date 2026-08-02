# Live verification: check-run/commit-status reporting (#559)

Throwaway PR to confirm the check-run/commit-status reporting feature (#558/#559) works against a
real GitHub App installation now that it's deployed: a "Lightbridge Review" Check Run should appear
`in_progress` on this PR's head SHA shortly after it opens, then resolve to `success`/`neutral` once
the automatic on-open review finalizes.

This file (and this PR) is not meant to be merged — closing without merging once the check has been
observed.
