use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use contextvm_sdk::core::types::{EncryptionMode, JsonRpcMessage, JsonRpcRequest};
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;
use nostr_sdk::prelude::*;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const DEFAULT_LOCAL_RELAY: &str = "ws://127.0.0.1:7777";
const DEFAULT_STRFRY_BIN: &str = "./strfry";
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_INTERVAL_SECS: u64 = 3600;
const RELAY_LIST_KIND: u16 = 10002;
const SERVER_ANNOUNCEMENT_KIND: u16 = 11316;
const CRAWL_KINDS: [u16; 5] = [11316, 11317, 11318, 11319, 11320];
const FALLBACK_RELAYS: [&str; 11] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relay.nostr.net",
    "wss://nos.lol",
    "wss://nostr.bitcoiner.social",
    "wss://nostr.oxtr.dev",
    "wss://offchain.pub",
    "wss://nostr.mom",
    "wss://nostr.wine",
    "wss://relay.ditto.pub",
    "wss://purplepag.es",
];

#[derive(Debug, Clone)]
struct Config {
    strfry_bin: PathBuf,
    local_relay: String,
    source_relays: Vec<String>,
    timeout: Duration,
    rounds: u32,
    interval: Duration,
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let config = Config::from_env()?;

    tracing::info!(
        strfry_bin = %config.strfry_bin.display(),
        local_relay = %config.local_relay,
        rounds = config.rounds,
        dry_run = config.dry_run,
        "starting announcement crawler"
    );

    for round in 1..=config.rounds {
        tracing::info!(round, total_rounds = config.rounds, "starting crawl round");
        run_round(&config).await?;

        if round < config.rounds {
            tracing::info!(
                sleep_seconds = config.interval.as_secs(),
                next_round = round + 1,
                "sleeping before next round"
            );
            sleep(config.interval).await;
        }
    }

    tracing::info!("crawl run complete");
    Ok(())
}

impl Config {
    fn from_env() -> Result<Self> {
        let mut strfry_bin = PathBuf::from(DEFAULT_STRFRY_BIN);
        let mut local_relay = DEFAULT_LOCAL_RELAY.to_string();
        let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        let mut rounds: u32 = 1;
        let mut interval = Duration::from_secs(DEFAULT_INTERVAL_SECS);
        let mut dry_run = false;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--strfry-bin" => {
                    strfry_bin = PathBuf::from(next_arg(&mut args, "--strfry-bin")?);
                }
                "--local-relay" => {
                    local_relay = next_arg(&mut args, "--local-relay")?;
                }
                "--timeout-seconds" => {
                    timeout = Duration::from_secs(
                        next_arg(&mut args, "--timeout-seconds")?
                            .parse()
                            .context("invalid --timeout-seconds")?,
                    );
                }
                "--rounds" => {
                    rounds = next_arg(&mut args, "--rounds")?
                        .parse()
                        .context("invalid --rounds")?;
                }
                "--interval-seconds" => {
                    interval = Duration::from_secs(
                        next_arg(&mut args, "--interval-seconds")?
                            .parse()
                            .context("invalid --interval-seconds")?,
                    );
                }
                "--dry-run" => dry_run = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        if rounds == 0 {
            bail!("--rounds must be greater than zero");
        }

        let mut source_relays = vec![local_relay.clone()];
        source_relays.extend(FALLBACK_RELAYS.iter().map(|relay| relay.to_string()));

        Ok(Self {
            strfry_bin,
            local_relay,
            source_relays,
            timeout,
            rounds,
            interval,
            dry_run,
        })
    }
}

async fn run_round(config: &Config) -> Result<()> {
    // 1. Discover server pubkeys from external relays (kind 11316)
    let discovered = discover_pubkeys(&config.source_relays, config.timeout).await?;
    tracing::info!(count = discovered.len(), "discovered server pubkeys from source relays");

    if discovered.is_empty() {
        tracing::info!("no server pubkeys discovered, nothing to do");
        return Ok(());
    }

    // 2. Probe each pubkey to find healthy servers
    let mut healthy = BTreeSet::new();
    let mut dead = BTreeSet::new();

    for (pubkey_hex, announcement_relays) in &discovered {
        let relay_list = fetch_relay_list(pubkey_hex, &config.source_relays, config.timeout)
            .await
            .with_context(|| format!("failed to fetch relay list for {pubkey_hex}"))?;

        let probe_targets = if relay_list.is_empty() {
            // Fall back to local relay + relays where the announcement was found
            let mut fallback = vec![config.local_relay.clone()];
            fallback.extend(announcement_relays.iter().cloned());
            fallback
        } else {
            relay_list
        };

        let outcome = probe_server(pubkey_hex, &probe_targets, config.timeout).await;

        if outcome.alive {
            tracing::info!(
                pubkey = %pubkey_hex,
                success_relay = ?outcome.success_relay,
                "server is alive"
            );
            healthy.insert(pubkey_hex.clone());
        } else {
            tracing::warn!(
                pubkey = %pubkey_hex,
                attempted_relays = ?outcome.attempted_relays,
                reason = %outcome.reason,
                "server probe failed, skipping"
            );
            dead.insert(pubkey_hex.clone());
        }
    }

    tracing::info!(
        healthy = healthy.len(),
        dead = dead.len(),
        "probe results"
    );

    // 3. Download announcement-related events from healthy pubkeys and import
    for pubkey_hex in &healthy {
        match download_and_import_pubkey_events(config, pubkey_hex).await {
            Ok(imported) => {
                tracing::info!(
                    pubkey = %pubkey_hex,
                    imported,
                    "imported events for pubkey"
                );
            }
            Err(error) => {
                tracing::warn!(
                    pubkey = %pubkey_hex,
                    error = %error,
                    "failed to download/import events for pubkey"
                );
            }
        }
    }

    Ok(())
}

/// Discover server pubkeys by querying source relays for kind 11316 events.
/// Returns a map from pubkey hex to the set of source relay URLs where the announcement was found.
async fn discover_pubkeys(
    source_relays: &[String],
    timeout_duration: Duration,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for relay_url in source_relays {
        match fetch_announcement_pubkeys(relay_url, timeout_duration).await {
            Ok(pubkeys) => {
                tracing::debug!(
                    relay = %relay_url,
                    count = pubkeys.len(),
                    "fetched announcement pubkeys from relay"
                );
                for pk in pubkeys {
                    map.entry(pk).or_default().insert(relay_url.clone());
                }
            }
            Err(error) => {
                tracing::warn!(
                    relay = %relay_url,
                    error = %error,
                    "failed to fetch announcements from relay"
                );
            }
        }
    }

    Ok(map)
}

async fn fetch_announcement_pubkeys(
    relay_url: &str,
    timeout_duration: Duration,
) -> Result<BTreeSet<String>> {
    let client = Client::default();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url}"))?;
    client.connect().await;

    let filter = Filter::new()
        .kind(Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
        .limit(500);

    let events = client
        .fetch_events(filter, timeout_duration)
        .await
        .with_context(|| format!("failed to fetch announcements from {relay_url}"))?;

    let _ = client.disconnect().await;

    let pubkeys: BTreeSet<String> = events
        .into_iter()
        .map(|event| event.pubkey.to_hex())
        .collect();

    Ok(pubkeys)
}

async fn fetch_relay_list(
    pubkey_hex: &str,
    sources: &[String],
    timeout_duration: Duration,
) -> Result<Vec<String>> {
    let pubkey = PublicKey::from_hex(pubkey_hex)
        .with_context(|| format!("invalid server pubkey: {pubkey_hex}"))?;
    let mut latest_event: Option<Event> = None;

    for source in sources {
        match fetch_latest_relay_list_from_source(pubkey, source, timeout_duration).await {
            Ok(Some(event)) => {
                let replace = latest_event
                    .as_ref()
                    .map(|current| event.created_at > current.created_at)
                    .unwrap_or(true);
                if replace {
                    latest_event = Some(event);
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(source = %source, pubkey = %pubkey_hex, error = %error, "relay list fetch failed");
            }
        }
    }

    Ok(latest_event
        .as_ref()
        .map(extract_relays)
        .unwrap_or_default())
}

async fn fetch_latest_relay_list_from_source(
    pubkey: PublicKey,
    relay_url: &str,
    timeout_duration: Duration,
) -> Result<Option<Event>> {
    let client = Client::default();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url}"))?;
    client.connect().await;

    let filter = Filter::new()
        .author(pubkey)
        .kind(Kind::Custom(RELAY_LIST_KIND))
        .limit(1);

    let events = client
        .fetch_events(filter, timeout_duration)
        .await
        .with_context(|| format!("failed to fetch relay list from {relay_url}"))?;

    let latest = events.into_iter().max_by_key(|event| event.created_at);
    let _ = client.disconnect().await;
    Ok(latest)
}

fn extract_relays(event: &Event) -> Vec<String> {
    let mut relays = BTreeSet::new();

    for tag in event.tags.iter() {
        let tag_vec: Vec<String> = tag.clone().to_vec();
        if tag_vec.len() >= 2 && tag_vec[0] == "r" {
            let normalized = normalize_relay_url(&tag_vec[1]);
            if !normalized.is_empty() {
                relays.insert(normalized);
            }
        }
    }

    relays.into_iter().collect()
}

struct ProbeOutcome {
    alive: bool,
    attempted_relays: Vec<String>,
    success_relay: Option<String>,
    reason: String,
}

async fn probe_server(pubkey_hex: &str, relays: &[String], timeout_duration: Duration) -> ProbeOutcome {
    let mut attempted = Vec::new();
    let mut errors = Vec::new();

    for relay in relays {
        attempted.push(relay.clone());
        match probe_server_once(pubkey_hex, relay, timeout_duration).await {
            Ok(()) => {
                return ProbeOutcome {
                    alive: true,
                    attempted_relays: attempted,
                    success_relay: Some(relay.clone()),
                    reason: "probe succeeded".to_string(),
                };
            }
            Err(error) => {
                errors.push(format!("{relay}: {error}"));
            }
        }
    }

    ProbeOutcome {
        alive: false,
        attempted_relays: attempted,
        success_relay: None,
        reason: errors.join(" | "),
    }
}

async fn probe_server_once(pubkey_hex: &str, relay: &str, timeout_duration: Duration) -> Result<()> {
    let keys = signer::generate();
    let nostr_config = NostrClientTransportConfig::default()
        .with_relay_urls(vec![relay.to_string()])
        .with_server_pubkey(pubkey_hex.to_string())
        .with_encryption_mode(EncryptionMode::Optional)
        .with_timeout(timeout_duration);
    let config = ProxyConfig::new(nostr_config);
    let mut proxy = NostrMCPProxy::new(keys, config).await?;
    let mut rx = proxy.start().await?;

    let request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: "tools/list".to_string(),
        params: None,
    });

    proxy.send(&request).await?;

    let received = timeout(timeout_duration, rx.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for server response"))?;

    proxy.stop().await?;

    match received {
        Some(JsonRpcMessage::Response(_)) => Ok(()),
        Some(other) => Err(anyhow!(
            "unexpected message: {}",
            serde_json::to_string(&other)?
        )),
        None => Err(anyhow!("server closed response channel without replying")),
    }
}

/// Download announcement-related events authored by a pubkey from source relays and import them into the local strfry.
async fn download_and_import_pubkey_events(config: &Config, pubkey_hex: &str) -> Result<usize> {
    let pubkey = PublicKey::from_hex(pubkey_hex)
        .with_context(|| format!("invalid pubkey: {pubkey_hex}"))?;

    // Collect events from all source relays, deduplicating by event ID
    let mut seen_ids = BTreeSet::new();
    let mut events_jsonl = Vec::new();

    for relay_url in &config.source_relays {
        match fetch_pubkey_events(pubkey, relay_url, config.timeout).await {
            Ok(events) => {
                for event in events {
                    let id_hex = event.id.to_hex();
                    if seen_ids.insert(id_hex) {
                        let json = serde_json::to_string(&event)
                            .context("failed to serialize event")?;
                        events_jsonl.push(json);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    relay = %relay_url,
                    pubkey = %pubkey_hex,
                    error = %error,
                    "failed to fetch events from relay"
                );
            }
        }
    }

    if events_jsonl.is_empty() {
        tracing::info!(pubkey = %pubkey_hex, "no events found for pubkey");
        return Ok(0);
    }

    let count = events_jsonl.len();

    if config.dry_run {
        tracing::warn!(
            pubkey = %pubkey_hex,
            count,
            "dry run: would import events for pubkey"
        );
        return Ok(count);
    }

    import_events(&config.strfry_bin, &events_jsonl).await?;

    Ok(count)
}

async fn fetch_pubkey_events(
    pubkey: PublicKey,
    relay_url: &str,
    timeout_duration: Duration,
) -> Result<Vec<Event>> {
    let client = Client::default();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url}"))?;
    client.connect().await;

    let kinds: Vec<Kind> = CRAWL_KINDS.iter().map(|k| Kind::Custom(*k)).collect();
    let filter = Filter::new()
        .author(pubkey)
        .kinds(kinds)
        .limit(500);

    let fetched = client
        .fetch_events(filter, timeout_duration)
        .await
        .with_context(|| format!("failed to fetch events for {pubkey} from {relay_url}"))?;

    let _ = client.disconnect().await;

    let events: Vec<Event> = fetched.into_iter().collect();
    Ok(events)
}

async fn import_events(strfry_bin: &PathBuf, events: &[String]) -> Result<()> {
    let input = events.join("\n") + "\n";

    let mut child = Command::new(strfry_bin)
        .arg("import")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {} import", strfry_bin.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(input.as_bytes()).await.context("failed to write events to strfry import stdin")?;
    }

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("failed to wait for {} import", strfry_bin.display()))?;

    if !output.status.success() {
        bail!(
            "strfry import failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

fn normalize_relay_url(url: &str) -> String {
    if url.ends_with('/') {
        url.trim_end_matches('/').to_string()
    } else {
        url.to_string()
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("missing value for {flag}"))
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cvm_announcement_crawler=info".parse().unwrap())
                .add_directive("contextvm_sdk=warn".parse().unwrap()),
        )
        .try_init();
}

fn print_help() {
    println!(
        "cvm-announcement-crawler\n\n\
Usage:\n\
  cargo run --manifest-path tools/cvm-announcement-crawler/Cargo.toml -- [options]\n\n\
Options:\n\
  --strfry-bin <path>          Path to the strfry binary [default: ./strfry]\n\
  --local-relay <url>          Local relay URL [default: ws://127.0.0.1:7777]\n\
  --timeout-seconds <n>        Network timeout in seconds [default: 10]\n\
  --rounds <n>                 Number of rounds to execute in this process [default: 1]\n\
  --interval-seconds <n>       Delay between rounds [default: 3600]\n\
  --dry-run                    Log import decisions without importing\n\
  --help                       Show this message\n"
    );
}
