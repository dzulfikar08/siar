#!/usr/bin/env bash
# Hosted mode memory with many tenants (run on CI): RSS after 1, 50 and 500 tenants have each
# made a few MCP calls, with the default tenant cache and with a cache that keeps all open.
#   rust/parity/tenants-rss.sh <path to siar>
set -u
BIN=$(realpath "$1")
run() {
  local cache=$1 port=$2
  local dir; dir=$(mktemp -d)
  export SIAR_MODE=hosted SIAR_DATA_DIR=$dir SIAR_VAULT_KEY=$(head -c32 /dev/urandom | base64) SIAR_PUBLIC_URL=https://app.example.com SIAR_PORT=$port SIAR_SCHEDULER=off SIAR_TENANT_CACHE=$cache
  "$BIN" serve >"$dir/log" 2>&1 &
  local pid=$!
  for i in $(seq 50); do curl -sf localhost:$port/health >/dev/null && break; sleep 0.1; done
  local keys=() n=0
  for i in $(seq 500); do keys+=("$("$BIN" tenant create "t$i" | jq -r .api_key)"); done
  echo "## SIAR_TENANT_CACHE=$cache"
  echo "idle, no tenant opened: $(grep -E 'VmRSS' /proc/$pid/status | tr -s ' \t' ' ')"
  for upto in 1 50 500; do
    while [ $n -lt $upto ]; do
      k=${keys[$n]}
      for call in '{"name":"status","arguments":{}}' '{"name":"create_idea","arguments":{"title":"hello"}}' '{"name":"list_posts","arguments":{"limit":20}}' '{"name":"list_ideas","arguments":{}}'; do
        curl -s -o /dev/null -X POST localhost:$port/mcp -H "authorization: Bearer $k" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":$call}"
      done
      n=$((n + 1))
    done
    echo "after $upto tenant(s) active: $(grep -E 'VmRSS|VmHWM' /proc/$pid/status | tr -s ' \t' ' ' | tr '\n' ' ')"
  done
  kill $pid; wait $pid 2>/dev/null; rm -rf "$dir"
}
# Same-runner baseline: single-tenant mode, idle and after the same four calls.
single() {
  local dir; dir=$(mktemp -d)
  SIAR_DATA_DIR=$dir SIAR_VAULT_KEY=$(head -c32 /dev/urandom | base64) SIAR_API_TOKEN=bench-token-0123456789abcdef SIAR_PORT=18800 SIAR_SCHEDULER=off "$BIN" serve >"$dir/log" 2>&1 &
  local pid=$!
  for i in $(seq 50); do curl -sf localhost:18800/health >/dev/null && break; sleep 0.1; done
  echo "## single-tenant baseline"
  echo "idle: $(grep -E 'VmRSS' /proc/$pid/status | tr -s ' \t' ' ')"
  for call in '{"name":"status","arguments":{}}' '{"name":"create_idea","arguments":{"title":"hello"}}' '{"name":"list_posts","arguments":{"limit":20}}' '{"name":"list_ideas","arguments":{}}'; do
    curl -s -o /dev/null -X POST localhost:18800/mcp -H "authorization: Bearer bench-token-0123456789abcdef" -H 'content-type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":$call}"
  done
  echo "after 4 calls: $(grep -E 'VmRSS|VmHWM' /proc/$pid/status | tr -s ' \t' ' ' | tr '\n' ' ')"
  kill $pid; wait $pid 2>/dev/null; rm -rf "$dir"
}
single
run 64 18801
run 1000 18802
