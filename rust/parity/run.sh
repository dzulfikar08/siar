#!/usr/bin/env bash
# TS-vs-Rust output comparison on a seeded test database (no real accounts, dummy keys).
#   rust/parity/run.sh <path to the Rust siar binary>
# Starts fake platforms, seeds one database with the TS code, copies it, runs `bun src/index.ts serve`
# and the Rust binary side by side on the copies, and diffs every tool's answer.
# Also here: cpu.ts (local per-call CPU), bench.sh + vps-bench.sh (production read-only benchmark),
# ro-compare.ts (read-only comparison against a copy of production data).
set -u
BIN=$(realpath "$1")
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
WORK=$(mktemp -d)
FAKE_PORT=19010 TS_PORT=19001 RS_PORT=19002
export SIAR_VAULT_KEY=$(head -c32 /dev/zero | tr '\0' '\7' | base64)
export SIAR_API_TOKEN=parity-token-0123456789abcdefgh SIAR_PUBLIC_URL=https://siar.example.com SIAR_OPERATOR="Test Op" SIAR_CONTACT=t@example.com
export SIAR_IG_BASE=http://127.0.0.1:$FAKE_PORT/ig SIAR_THREADS_BASE=http://127.0.0.1:$FAKE_PORT/th FAKE=http://127.0.0.1:$FAKE_PORT
export INSTAGRAM_CLIENT_SECRET=igsecret SIAR_META_VERIFY_TOKEN=vtok X_CLIENT_ID=xid X_CLIENT_SECRET=xsecret THREADS_CLIENT_ID=tid THREADS_CLIENT_SECRET=ts
export LINKEDIN_CLIENT_ID=lid LINKEDIN_CLIENT_SECRET=ls
# The TS serve always runs its scheduler; give the Rust one the same loops, with long ticks so
# nothing fires during the run (automation jobs use relaxed timing: 2+ minutes).
export SIAR_TICK_MS=600000 SIAR_ENGAGE_MS=6000000

for port in $FAKE_PORT $TS_PORT $RS_PORT; do
  if ss -ltn | grep -q ":$port "; then echo "port $port is in use; stop the old run first" >&2; exit 1; fi
done 2>/dev/null
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

bun "$ROOT/rust/parity/fake-platforms.ts" $FAKE_PORT 2>/dev/null & pids+=($!)
mkdir -p "$WORK/ts"
(cd "$ROOT" && SIAR_DATA_DIR="$WORK/ts" bun rust/parity/seed.ts >/dev/null)
cp -a "$WORK/ts" "$WORK/rs"
(cd "$ROOT" && SIAR_DATA_DIR="$WORK/ts" SIAR_PORT=$TS_PORT exec bun src/index.ts serve >"$WORK/ts.log" 2>&1) & pids+=($!)
SIAR_DATA_DIR="$WORK/rs" SIAR_PORT=$RS_PORT "$BIN" serve >"$WORK/rs.log" 2>&1 & pids+=($!)
for i in $(seq 50); do curl -sf localhost:$TS_PORT/health >/dev/null && curl -sf localhost:$RS_PORT/health >/dev/null && break; sleep 0.1; done
bun "$ROOT/rust/parity/compare.ts" http://127.0.0.1:$TS_PORT http://127.0.0.1:$RS_PORT $SIAR_API_TOKEN
