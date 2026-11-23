#!/usr/bin/env bash
# Install the CI-built binary of the current commit on a server and restart siar.
# Usage: deploy/deploy.sh <ssh-host>
# Needs: the commit pushed and its `rust` workflow run green (gh CLI signed in), and the layout from
# deploy/siar.service: user siar, /opt/siar/{bin,data,.env}, systemd unit siar.
set -euo pipefail
HOST=${1:-${SIAR_DEPLOY_HOST:?usage: deploy/deploy.sh <ssh-host> (or set SIAR_DEPLOY_HOST)}}
cd "$(dirname "$0")/.."
SHA=$(git rev-parse HEAD)
RUN=$(gh run list --workflow rust.yml --commit "$SHA" --status success --limit 1 --json databaseId -q '.[0].databaseId')
[ -n "$RUN" ] || { echo "no green rust workflow run for $SHA yet: push, wait for CI, then deploy" >&2; exit 1; }
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
gh run download "$RUN" -n siar-x86_64-linux-musl -D "$TMP"
scp -q "$TMP/siar" "$HOST:/tmp/siar.new"
ssh "$HOST" 'set -e
  sudo install -d -m 755 /opt/siar/bin
  [ -f /opt/siar/bin/siar ] && sudo cp /opt/siar/bin/siar /opt/siar/bin/siar.prev
  sudo install -m 755 /tmp/siar.new /opt/siar/bin/siar
  rm -f /tmp/siar.new
  sudo systemctl restart siar
  sleep 2
  systemctl is-active siar
  curl -fsS http://127.0.0.1:8787/health'
echo "deployed $SHA (CI run $RUN); the previous binary is /opt/siar/bin/siar.prev"
