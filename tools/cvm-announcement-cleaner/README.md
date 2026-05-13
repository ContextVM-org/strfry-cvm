# cvm-announcement-cleaner

Rust sidecar for cleaning dead ContextVM server announcements from a local `strfry` instance.

## Current behavior

- scans local server announcements from `kind 11316`
- fetches `kind 10002` relay lists from:
  - local relay
  - `wss://relay.damus.io`
  - `wss://relay.primal.net`
  - `wss://relay.nostr.net`
  - `wss://nos.lol`
- probes candidate relays by sending a minimal ContextVM request
- falls back to probing through the local relay when no usable `10002` relay list is found
- tracks failures only in memory during the running process
- deletes all local events authored by the dead server pubkey after the configured failure threshold is reached, including `kind 10002`

## Usage

```bash
cargo run --manifest-path tools/cvm-announcement-cleaner/Cargo.toml -- --dry-run
```

Useful options:

- `--strfry-bin ./strfry`
- `--local-relay ws://127.0.0.1:7777`
- `--timeout-seconds 10`
- `--failure-threshold 3`
- `--rounds 3`
- `--interval-seconds 3600`
- `--dry-run`

## Notes

- This tool intentionally does not persist state.
- To model consecutive failures without persistence, run multiple rounds in one process.
- A typical operational pattern is to start with `--dry-run`, inspect logs, then run without it.
- Once a pubkey crosses the failure threshold, deletion is author-wide for the local relay database rather than limited to announcement kinds.
