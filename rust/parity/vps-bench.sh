#!/usr/bin/env bash
# On the VPS: run the Rust binary as a transient unit (scheduler off) on a copy of the data, measure
# idle memory and per-call CPU, then the same for the live Bun service. Read-only for production.
# Expects /tmp/siar-rs/{siar,bench.sh,override.env,data}; override.env holds SIAR_DATA_DIR, SIAR_PORT=8788, SIAR_SCHEDULER=off.
set -u
cd /tmp/siar-rs
mem() { local pid; pid=$(systemctl show "$1" -p MainPID --value); echo "$1 $(sudo grep -E 'VmRSS|VmHWM' /proc/$pid/status | tr -s ' \t' ' ' | tr '\n' ' ') $(systemctl show "$1" -p MemoryCurrent)"; }
echo "########## RUST ($(stat -c %s /tmp/siar-rs/siar) bytes)"
sudo systemd-run -q --unit=siar-rs -p User=siar -p Group=siar -p EnvironmentFile=/opt/siar/.env -p EnvironmentFile=/tmp/siar-rs/override.env \
  -E SIAR_DATA_DIR=/tmp/siar-rs/data -E SIAR_PORT=8788 -E SIAR_SCHEDULER=off /tmp/siar-rs/siar serve
sleep 1
pid=$(systemctl show siar-rs -p MainPID --value)
sudo sh -c "tr '\0' '\n' < /proc/$pid/environ" | grep -q '^SIAR_SCHEDULER=off$' || { echo "scheduler not off, stopping"; sudo systemctl stop siar-rs; exit 1; }
sleep 60; echo "idle-60s: $(mem siar-rs)"
UNIT=siar-rs PORT=8788 ./bench.sh 2>/dev/null
echo "after-bench: $(mem siar-rs)"
sudo systemctl stop siar-rs
echo "########## BUN (live siar service, $(stat -c %s /usr/local/bin/bun) bytes)"
echo "now: $(mem siar)"
UNIT=siar PORT=8787 ./bench.sh 2>/dev/null
echo "after-bench: $(mem siar)"
