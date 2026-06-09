# cvm-announcement-crawler

Rust sidecar for crawling ContextVM server announcements from external relays and importing healthy ones into a local `strfry` instance.

## Current behavior

- queries external relays for `kind 11316` (server announcement) events to discover server pubkeys
- fetches `kind 10002` relay lists for each discovered pubkey from the local relay and a set of external fallback relays (see source for current list)
- probes candidate servers by sending a minimal ContextVM request
- falls back to probing through the local relay and the relays where the announcement was found when no usable `10002` relay list is found
- downloads announcement-related events (kinds `11316`–`11320`) authored by healthy server pubkeys from source relays:
  - `11316` — Server Announcement
  - `11317` — Tools List
  - `11318` — Resources List
  - `11319` — Resource Templates List
  - `11320` — Prompts List
- imports downloaded events into the local `strfry` database via `strfry import`

When paired with [`cvm-announcement-cleaner`](../cvm-announcement-cleaner/README.md), this creates a self-maintaining live directory of working ContextVM servers: the crawler discovers and imports healthy servers, while the cleaner removes dead ones.

## Usage

```bash
cargo run --manifest-path tools/cvm-announcement-crawler/Cargo.toml -- --dry-run
```

Useful options:

- `--strfry-bin ./strfry`
- `--local-relay ws://127.0.0.1:7777`
- `--timeout-seconds 10`
- `--rounds 1`
- `--interval-seconds 3600`
- `--dry-run`

## Notes

- No persistent state file is needed — each run independently discovers, probes, and imports.
- `strfry import` naturally deduplicates events by ID, so re-importing the same events is harmless.
- The tool downloads events from source relays (fallback list + local relay), not directly from the probed server's relay list. This ensures events are available even if the server's own relay has limited retention.
- When no `10002` relay list can be resolved, probing falls back to the local relay plus the external relays where the server's announcement was originally discovered.
- Only announcement-related event kinds (`11316`–`11320`) are imported — general-purpose events authored by the server pubkey are excluded.
- A typical operational pattern is to start with `--dry-run`, inspect logs, then run without it.
- The crawler uses `strfry import` which bypasses the write-policy plugin, so imported events are not subject to kind restrictions.
