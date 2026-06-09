# cvm-announcement-cleaner

Rust sidecar for cleaning dead ContextVM server announcements from a local `strfry` instance.

## Current behavior

- scans local server announcements from `kind 11316`
- fetches `kind 10002` relay lists from the local relay and a set of external fallback relays (see source for current list)
- probes candidate relays by sending a minimal ContextVM request
- falls back to probing through the local relay when no usable `10002` relay list is found
- persists consecutive failure counts in a small JSON file keyed by pubkey
- removes a pubkey entry from the JSON file on success
- removes a pubkey entry from the JSON file after deletion
- prunes state entries for pubkeys that no longer have a local `kind 11316` announcement
- deletes all local events authored by the dead server pubkey after the configured failure threshold is reached, including `kind 10002`

## Usage

```bash
cargo run --manifest-path tools/cvm-announcement-cleaner/Cargo.toml -- --dry-run
```

Useful options:

- `--strfry-bin ./strfry`
- `--state-file /var/lib/strfry/cvm-announcement-cleaner-state.json`
- `--local-relay ws://127.0.0.1:7777`
- `--timeout-seconds 10`
- `--failure-threshold 3`
- `--rounds 3`
- `--interval-seconds 3600`
- `--dry-run`

## Notes

- The state file is created automatically if it does not exist.
- The JSON file only stores pubkeys that are currently on a failing streak.
- A success removes the pubkey entry instead of writing a zero count.
- A threshold-triggered deletion also removes the pubkey entry.
- A typical operational pattern is to start with `--dry-run`, inspect logs, then run without it.
- Once a pubkey crosses the failure threshold, deletion is author-wide for the local relay database rather than limited to announcement kinds.
