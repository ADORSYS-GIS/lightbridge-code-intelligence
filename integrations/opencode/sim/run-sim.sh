#!/usr/bin/env sh
# RFC-0009 offline loop simulation — drives the REAL opencode binary + our REAL plugins through a
# full tool-call loop with NO eaig and NO control plane, and asserts the gate-interlock + recorder
# behavior. See README.md.
#
# Requires: docker, and the agent image built:
#   docker build --platform linux/arm64 -f integrations/opencode/Dockerfile -t lightbridge-agent-open:poc integrations/opencode
# Run:  integrations/opencode/sim/run-sim.sh
set -eu

IMG="${SIM_IMAGE:-lightbridge-agent-open:poc}"
PLATFORM="${SIM_PLATFORM:-linux/arm64}"
HERE="$(cd "$(dirname "$0")" && pwd)"
PLUGINS="$(cd "$HERE/../plugins" && pwd)"

exec docker run --rm --platform "$PLATFORM" --user root \
  -v "$HERE:/sim:ro" -v "$PLUGINS:/plugins:ro" \
  --entrypoint sh "$IMG" -c '
set -eu
# node is only needed to RUN the mock provider/MCP (opencode itself is the vendored musl binary).
# The real image has no node; the sim adds it at runtime (a sim image could pre-bake it).
apk add --no-cache nodejs >/dev/null 2>&1 || true
command -v node >/dev/null || apk add --no-cache nodejs >/dev/null 2>&1
command -v node >/dev/null || { echo "FATAL: could not install node for the sim"; exit 1; }

export LCI_SIM_PROVIDER_LOG=/tmp/provider.log LCI_SIM_MCP_LOG=/tmp/mcp.log
export LCI_RECORDER_PATH=/tmp/rec.jsonl LCI_LOG_SERVICE=lci-sim
export LCI_GATE_TERMINAL_TOOL=lightbridge_submit_findings
export LCI_GATE_REQUIRED_TOOLS=lightbridge_refute_finding
export OPENCODE_CONFIG=/sim/opencode.sim.jsonc
export OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_MODELS_FETCH=1

node /sim/mock-provider.mjs 2>/dev/null & PROV=$!
sleep 1
cd /tmp
timeout 40 opencode run --agent sim-agent "review the change" >/tmp/run.out 2>/tmp/run.err || true
kill "$PROV" 2>/dev/null || true

echo "================ RFC-0009 offline loop sim ================"
node -e "
const fs=require(\"fs\");
const rd=(p)=>{try{return fs.readFileSync(p,\"utf8\").trim().split(\"\n\").filter(Boolean).map(JSON.parse)}catch{return[]}};
const prov=rd(\"/tmp/provider.log\").filter(d=>d.nTools>0);
const mcp=rd(\"/tmp/mcp.log\");
const rec=rd(\"/tmp/rec.jsonl\");
const err=fs.readFileSync(\"/tmp/run.err\",\"utf8\");

const decisions=prov.map(d=>d.decision.kind===\"tool\"?d.decision.name:\"<text:done>\");
const executed=mcp.filter(d=>d.event===\"tool_call\").map(d=>d.name);
const afters=rec.filter(d=>d.kind===\"tool.after\");
const gateBlocked=/Gate interlock: lightbridge_submit_findings is blocked/.test(err);
const resultsCaptured=afters.filter(a=>a.result&&a.result.content&&a.result.content[0]&&a.result.content[0].text).map(a=>a.tool);
const firstSubmitReachedMcp=executed[0]===\"submit_findings\";

console.log(\"model turns   :\", decisions.join(\" -> \"));
console.log(\"MCP executed  :\", executed.join(\", \"), \"   (first submit_findings must be ABSENT — gate blocked it)\");
console.log(\"recorder after:\", afters.map(a=>a.tool).join(\", \"));
console.log(\"results kept  :\", resultsCaptured.join(\", \"), \"(right-bytes)\");
console.log(\"\");
const chk=(name,ok)=>console.log((ok?\"PASS\":\"FAIL\")+\"  \"+name);
let ok=true;
const a1=gateBlocked; ok=ok&&a1;
const a2=!firstSubmitReachedMcp && executed.filter(x=>x===\"refute_finding\").length>=1; ok=ok&&a2;
const a3=executed.indexOf(\"refute_finding\")<executed.indexOf(\"submit_findings\"); ok=ok&&a3;
const a4=resultsCaptured.includes(\"lightbridge_submit_findings\") && resultsCaptured.includes(\"lightbridge_refute_finding\"); ok=ok&&a4;
const a5=/\"message\":\"tool.done\"/.test(err) && /durationMs/.test(err); ok=ok&&a5;
chk(\"gate-interlock blocked the premature submit_findings\", a1);
chk(\"blocked submit never reached the tool; refute ran first\", a2);
chk(\"refute_finding executed BEFORE the allowed submit_findings\", a3);
chk(\"recorder captured MCP tool RESULTS (right-bytes)\", a4);
chk(\"logger emitted tool.done with durationMs\", a5);
console.log(\"\");
console.log(ok?\"===== SIM PASSED =====\":\"===== SIM FAILED =====\");
process.exit(ok?0:1);
"
'
