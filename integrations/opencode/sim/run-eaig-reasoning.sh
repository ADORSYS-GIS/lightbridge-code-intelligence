#!/usr/bin/env sh
# RFC-0009 probe item (c) — reasoning fidelity against the REAL eaig gateway.
#
# Runs the built agent image, points opencode at eaig (opencode.eaig.jsonc), asks a reasoning-inducing
# prompt, and reports whether the recorder captured `reasoning.part` entries. This is the ONE probe
# item the offline sim can't fake — it needs a real reasoning model.
#
# ⚠️ eaig is internal; run this where the gateway + CA are reachable. Required env:
#   LCI_EAIG_BASE_URL  LCI_EAIG_API_KEY  LCI_EAIG_MODEL  LCI_EAIG_CA (path to the internal CA bundle)
#
# Usage:
#   LCI_EAIG_BASE_URL=... LCI_EAIG_API_KEY=... LCI_EAIG_MODEL=... LCI_EAIG_CA=/path/to/ca.pem \
#     integrations/opencode/sim/run-eaig-reasoning.sh
set -eu

: "${LCI_EAIG_BASE_URL:?set LCI_EAIG_BASE_URL}"
: "${LCI_EAIG_API_KEY:?set LCI_EAIG_API_KEY}"
: "${LCI_EAIG_MODEL:?set LCI_EAIG_MODEL}"
: "${LCI_EAIG_CA:?set LCI_EAIG_CA (path to the internal CA bundle)}"

IMG="${SIM_IMAGE:-lightbridge-agent-open:poc}"
PLATFORM="${SIM_PLATFORM:-linux/arm64}"
HERE="$(cd "$(dirname "$0")" && pwd)"

exec docker run --rm --platform "$PLATFORM" \
  -v "$HERE:/sim:ro" \
  -v "$LCI_EAIG_CA:/etc/lci/eaig-ca.pem:ro" \
  -e "LCI_EAIG_BASE_URL=$LCI_EAIG_BASE_URL" \
  -e "LCI_EAIG_API_KEY=$LCI_EAIG_API_KEY" \
  -e "LCI_EAIG_MODEL=$LCI_EAIG_MODEL" \
  -e "NODE_EXTRA_CA_CERTS=/etc/lci/eaig-ca.pem" \
  -e "OPENCODE_CONFIG=/sim/opencode.eaig.jsonc" \
  -e "LCI_RECORDER_PATH=/tmp/rec.jsonl" \
  -e "LCI_LOG_SERVICE=lci-eaig" \
  -e "OPENCODE_DISABLE_AUTOUPDATE=1" \
  --entrypoint sh "$IMG" -c '
cd /tmp
timeout 90 opencode run --agent reasoner "Is 91 prime? Reason step by step, then answer." >/tmp/out 2>/tmp/err || true
echo "================ RFC-0009 probe item (c): reasoning fidelity (eaig) ================"
echo "--- assistant output ---"; sed "s/\x1b\[[0-9;]*m//g" /tmp/out | tail -4
# grep -c prints "0" AND exits non-zero on no matches; `|| echo 0` would then DOUBLE it ("0\n0")
# and break the integer test. Capture grep stdout, and fall back to 0 only when grep itself failed.
REASONING=$(grep -c "\"kind\":\"reasoning.part\"" /tmp/rec.jsonl 2>/dev/null) || REASONING=0
echo ""
echo "recorder reasoning.part entries: $REASONING"
if [ "$REASONING" -gt 0 ]; then
  echo "PASS  reasoning survived the eaig path and reached the recorder (item c)"
  grep "\"kind\":\"reasoning.part\"" /tmp/rec.jsonl | head -1 | cut -c1-200
else
  echo "FAIL/UNKNOWN  no reasoning.part captured — check: (1) LCI_EAIG_MODEL is reasoning-capable,"
  echo "  (2) eaig forwards reasoning on the chat-completions path, (3) opencode maps it to a"
  echo "  reasoning message part. Raw opencode stderr follows (the container is --rm, so it is"
  echo "  printed here rather than left in /tmp):"
  sed "s/\x1b\[[0-9;]*m//g" /tmp/err | tail -20
fi
'
