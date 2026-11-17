#!/usr/bin/env bash
# Per-call CPU of a siar service's cgroup for read-only MCP tools (the method used for the Bun numbers).
#   UNIT=siar PORT=8787 ./bench.sh        UNIT=siar-rs PORT=8788 ./bench.sh
# Reads the API token from /opt/siar/.env with sudo (never printed). Picks one account per platform.
set -u
UNIT=${UNIT:-siar}
PORT=${PORT:-8787}
TOKEN=$(sudo grep '^SIAR_API_TOKEN=' /opt/siar/.env | cut -d= -f2- | tr -d '"')
CG=/sys/fs/cgroup/system.slice/$UNIT.service/cpu.stat
u() { awk '/^usage_usec/{print $2}' "$CG"; }
mcp() { curl -s -X POST "http://127.0.0.1:$PORT/mcp" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "$1"; }
call() { curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/mcp" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}"; }
accounts=$(mcp '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_accounts","arguments":{}}}')
acc() { printf '%s' "$accounts" | python3 -c "import json,sys; r=json.load(sys.stdin)['result']['structuredContent']['items']; print(next(a['id'] for a in r if a['platform']=='$1'))"; }
TH=$(acc threads) IG=$(acc instagram) YT=$(acc youtube) LI=$(acc linkedin)
run() {
  local name=$1 args=$2 best=999999999 all=""
  for i in 1 2 3 4 5; do a=$(u); code=$(call "$name" "$args"); b=$(u); d=$((b - a)); all="$all $d"; [ $d -lt $best ] && best=$d; done
  printf "%-34s http=%s min=%6dus  runs(us):%s\n" "$name $3" "$code" "$best" "$all"
}
idle_a=$(u); sleep 60; idle_b=$(u); echo "idle: $((idle_b - idle_a))us CPU over 60s"
run status '{}' ""
run list_accounts '{}' ""
run list_platforms '{}' ""
run list_posts '{"limit":100}' "(100)"
run list_ideas '{}' ""
run best_times "{\"account_id\":\"$TH\"}" "(threads)"
run account_analytics "{\"account_id\":\"$IG\"}" "(ig live API)"
run list_account_posts "{\"account_id\":\"$IG\",\"limit\":25}" "(ig live API)"
run list_comments "{\"account_id\":\"$IG\"}" "(ig, db)"
run account_analytics "{\"account_id\":\"$YT\"}" "(youtube live)"
run list_account_posts "{\"account_id\":\"$LI\",\"limit\":25}" "(linkedin, error)"
run validate_post "{\"content\":\"hello world #test\",\"targets\":[\"$TH\",\"$LI\"]}" ""
run list_automations '{}' ""
run list_conversations "{\"account_id\":\"$IG\",\"limit\":10}" "(ig live API)"
run list_stories "{\"account_id\":\"$IG\"}" "(ig live API)"
