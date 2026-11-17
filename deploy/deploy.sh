#!/usr/bin/env bash
# Ship the current commit to a server and restart siar. Usage: deploy/deploy.sh <ssh-host>
# Expects the layout from deploy/README.md: user siar, /opt/siar/{app,data,.env}, systemd unit siar.
set -euo pipefail
HOST=${1:-${SIAR_DEPLOY_HOST:?usage: deploy/deploy.sh <ssh-host> (or set SIAR_DEPLOY_HOST)}}
cd "$(dirname "$0")/.."
git archive --format=tar HEAD | ssh "$HOST" 'sudo rm -rf /opt/siar/app.new && sudo mkdir -p /opt/siar/app.new && sudo tar -x -C /opt/siar/app.new'
ssh "$HOST" 'set -e
  sudo rm -rf /opt/siar/app.old
  [ -d /opt/siar/app ] && sudo mv /opt/siar/app /opt/siar/app.old
  sudo mv /opt/siar/app.new /opt/siar/app
  sudo chown -R root:siar /opt/siar/app
  sudo systemctl restart siar
  sleep 2
  systemctl is-active siar
  curl -fsS http://127.0.0.1:8787/health'
