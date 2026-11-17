# Week-long trial of the Rust build

The Rust binary runs next to the Bun service on the same VPS, with its own fresh database, its own
public URL and one test account. It never opens `/opt/siar/data` and never sees the real accounts.

| | Bun (production) | Rust (trial) |
|---|---|---|
| unit | `siar.service` | `siar-rs.service` (`deploy/siar-rs.service`) |
| binary | `/usr/local/bin/bun src/index.ts serve` | `/opt/siar-rs/bin/siar serve` |
| data | `/opt/siar/data` | `/opt/siar-rs/data` (new, empty) |
| env | `/opt/siar/.env` | `/opt/siar-rs/.env` |
| port | 8787 | 8788 |
| scheduler | on | on |

## Setup (not done yet)

1. Download the `siar-x86_64-linux-musl` artifact of the latest green `rust` workflow run and copy
   `siar` to `/opt/siar-rs/bin/siar` (owner root, mode 755).
2. `sudo install -d -o siar -g siar -m 750 /opt/siar-rs/data`
3. Write `/opt/siar-rs/.env` (owner root, mode 600) with:
   - a new `SIAR_VAULT_KEY` and `SIAR_API_TOKEN` (`/opt/siar-rs/bin/siar keygen`), never the production ones,
     so a mix-up cannot decrypt production tokens;
   - `SIAR_PORT=8788`, `SIAR_HOST=127.0.0.1`;
   - `SIAR_PUBLIC_URL` on its own hostname (for example a second Cloudflare Tunnel route to port 8788),
     so OAuth callbacks and `/media/…` reach the trial instance;
   - only the app credentials the test account needs, and `SIAR_OPERATOR`, `SIAR_CONTACT`;
   - no R2 settings, or a separate bucket/prefix, so staged media never lands next to production media.
   Do not set `SIAR_DATA_DIR` there: the unit sets it to `/opt/siar-rs/data`.
4. `sudo cp deploy/siar-rs.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl enable --now siar-rs`
5. Register the trial's callback URL in the test app, connect the test account with `connect_account`
   (MCP at `$SIAR_PUBLIC_URL/mcp` with the trial token), then schedule test posts to it only.

The production `siar` service and `/opt/siar` are not touched by any of these steps.

## What to watch during the week

- `systemctl show siar-rs -p MemoryCurrent -p MemoryPeak -p CPUUsageNSec` next to `siar`, daily.
- `journalctl -u siar-rs`: scheduler, automation jobs and polling logs (`[scheduler]`, `[automations]`).
- Posts to the test account go out on time, retries and backoff behave, metrics snapshots appear
  (`post_metric_history`), and an automation on the test account answers comments or DMs.

## Ending the trial

`sudo systemctl disable --now siar-rs && sudo rm /etc/systemd/system/siar-rs.service && sudo systemctl daemon-reload`,
then remove `/opt/siar-rs` once its data is no longer needed.
