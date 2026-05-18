use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use contextvm_sdk::core::types::{EncryptionMode, JsonRpcMessage, JsonRpcRequest};
use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
use contextvm_sdk::signer;
use contextvm_sdk::transport::client::NostrClientTransportConfig;
use nostr_sdk::prelude::*;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const DEFAULT_LOCAL_RELAY: &str = "ws://127.0.0.1:7777";
const DEFAULT_STRFRY_BIN: &str = "./strfry";
const DEFAULT_STATE_FILE: &str = "/var/lib/strfry/cvm-announcement-cleaner-state.json";
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
const DEFAULT_INTERVAL_SECS: u64 = 3600;
const RELAY_LIST_KIND: u16 = 10002;
const SERVER_ANNOUNCEMENT_KIND: u16 = 11316;
const FALLBACK_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relay.nostr.net",
    "wss://nos.lol",
];

#[derive(Debug, Clone)]
struct Config {
    strfry_bin: PathBuf,
    state_file: PathBuf,
    local_relay: String,
    relay_list_sources: Vec<String>,
    timeout: Duration,
    failure_threshold: u32,
    rounds: u32,
    interval: Duration,
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct AnnouncementEvent {
    pubkey: String,
    kind: u16,
}

#[derive(Debug, Clone)]
struct RelayListResult {
    relays: Vec<String>,
    source_relays: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProbeOutcome {
    alive: bool,
    attempted_relays: Vec<String>,
    success_relay: Option<String>,
    reason: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let config = Config::from_env()?;

    tracing::info!(
        strfry_bin = %config.strfry_bin.display(),
        state_file = %config.state_file.display(),
        local_relay = %config.local_relay,
        rounds = config.rounds,
        failure_threshold = config.failure_threshold,
        dry_run = config.dry_run,
        "starting announcement cleaner"
    );

    let mut failures = load_failure_state(&config.state_file)?;

    for round in 1..=config.rounds {
        tracing::info!(round, total_rounds = config.rounds, "starting cleanup round");
        run_round(&config, &mut failures).await?;
        save_failure_state(&config.state_file, &failures)?;

        if round < config.rounds {
            tracing::info!(
                sleep_seconds = config.interval.as_secs(),
                next_round = round + 1,
                "sleeping before next round"
            );
            sleep(config.interval).await;
        }
    }

    tracing::info!("cleanup run complete");
    Ok(())
}

impl Config {
    fn from_env() -> Result<Self> {
        let mut strfry_bin = PathBuf::from(DEFAULT_STRFRY_BIN);
        let mut state_file = PathBuf::from(DEFAULT_STATE_FILE);
        let mut local_relay = DEFAULT_LOCAL_RELAY.to_string();
        let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        let mut failure_threshold = DEFAULT_FAILURE_THRESHOLD;
        let mut rounds = DEFAULT_FAILURE_THRESHOLD;
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
                "--state-file" => {
                    state_file = PathBuf::from(next_arg(&mut args, "--state-file")?);
                }
                "--timeout-seconds" => {
                    timeout = Duration::from_secs(
                        next_arg(&mut args, "--timeout-seconds")?
                            .parse()
                            .context("invalid --timeout-seconds")?,
                    );
                }
                "--failure-threshold" => {
                    failure_threshold = next_arg(&mut args, "--failure-threshold")?
                        .parse()
                        .context("invalid --failure-threshold")?;
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

        if failure_threshold == 0 {
            bail!("--failure-threshold must be greater than zero");
        }

        let mut relay_list_sources = vec![local_relay.clone()];
        relay_list_sources.extend(FALLBACK_RELAYS.iter().map(|relay| relay.to_string()));

        Ok(Self {
            strfry_bin,
            state_file,
            local_relay,
            relay_list_sources,
            timeout,
            failure_threshold,
            rounds,
            interval,
            dry_run,
        })
    }
}

async fn run_round(config: &Config, failures: &mut BTreeMap<String, u32>) -> Result<()> {
    let announcements = scan_announcements(&config.strfry_bin).await?;
    let current_pubkeys: BTreeSet<String> = announcements.iter().map(|event| event.pubkey.clone()).collect();

    failures.retain(|pubkey, _| current_pubkeys.contains(pubkey));

    tracing::info!(count = announcements.len(), "loaded local server announcements");

    for announcement in announcements {
        let relay_list = fetch_relay_list(&announcement.pubkey, &config.relay_list_sources, config.timeout)
            .await
            .with_context(|| format!("failed to fetch relay list for {}", announcement.pubkey))?;

        let probe_targets = if relay_list.relays.is_empty() {
            vec![config.local_relay.clone()]
        } else {
            relay_list.relays.clone()
        };

        let outcome = probe_server(&announcement.pubkey, &probe_targets, config.timeout).await;

        if outcome.alive {
            failures.remove(&announcement.pubkey);
            tracing::info!(
                pubkey = %announcement.pubkey,
                success_relay = ?outcome.success_relay,
                source_relays = ?relay_list.source_relays,
                "server is alive"
            );
            continue;
        }

        let failure_count = failures.entry(announcement.pubkey.clone()).or_insert(0);
        *failure_count += 1;

        tracing::warn!(
            pubkey = %announcement.pubkey,
            failure_count = *failure_count,
            threshold = config.failure_threshold,
            attempted_relays = ?outcome.attempted_relays,
            reason = %outcome.reason,
            "server probe failed"
        );

        if *failure_count >= config.failure_threshold {
            delete_announcement_family(config, &announcement.pubkey).await?;
            failures.remove(&announcement.pubkey);
        }
    }

    Ok(())
}

async fn scan_announcements(strfry_bin: &PathBuf) -> Result<Vec<AnnouncementEvent>> {
    let filter = format!(r#"{{"kinds":[{SERVER_ANNOUNCEMENT_KIND}]}}"#);
    let output = Command::new(strfry_bin)
        .arg("scan")
        .arg(filter)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {} scan", strfry_bin.display()))?;

    if !output.status.success() {
        bail!(
            "strfry scan failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("strfry scan output was not valid UTF-8")?;
    let mut announcements = Vec::new();

    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: AnnouncementEvent = serde_json::from_str(line)
            .with_context(|| format!("failed to parse announcement event: {line}"))?;
        if event.kind == SERVER_ANNOUNCEMENT_KIND {
            announcements.push(event);
        }
    }

    Ok(announcements)
}

async fn fetch_relay_list(pubkey_hex: &str, sources: &[String], timeout_duration: Duration) -> Result<RelayListResult> {
    let pubkey = PublicKey::from_hex(pubkey_hex)
        .with_context(|| format!("invalid server pubkey: {pubkey_hex}"))?;
    let mut latest_event: Option<Event> = None;
    let mut source_relays = Vec::new();

    for source in sources {
        match fetch_latest_relay_list_from_source(pubkey, source, timeout_duration).await {
            Ok(Some(event)) => {
                source_relays.push(source.clone());
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

    let relays = latest_event
        .as_ref()
        .map(extract_relays)
        .unwrap_or_default();

    Ok(RelayListResult { relays, source_relays })
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
        Some(other) => Err(anyhow!("unexpected message: {}", serde_json::to_string(&other)?)),
        None => Err(anyhow!("server closed response channel without replying")),
    }
}

async fn delete_announcement_family(config: &Config, pubkey_hex: &str) -> Result<()> {
    let filter = format!(r#"{{"authors":["{pubkey_hex}"]}}"#);

    if config.dry_run {
        tracing::warn!(pubkey = %pubkey_hex, filter = %filter, "dry run: would delete all events for pubkey");
        return Ok(());
    }

    let output = Command::new(&config.strfry_bin)
        .arg("delete")
        .arg("--filter")
        .arg(filter)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {} delete", config.strfry_bin.display()))?;

    if !output.status.success() {
        bail!(
            "strfry delete failed for {pubkey_hex}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    tracing::warn!(pubkey = %pubkey_hex, "deleted all events for pubkey");
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

fn load_failure_state(path: &PathBuf) -> Result<BTreeMap<String, u32>> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create state directory {}", parent.display()))?;
        }
        fs::write(path, b"{}")
            .with_context(|| format!("failed to initialize state file {}", path.display()))?;
        return Ok(BTreeMap::new());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read state file {}", path.display()))?;

    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }

    let entries: BTreeMap<String, u32> = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse state file {}", path.display()))?;

    Ok(entries)
}

fn save_failure_state(path: &PathBuf, failures: &BTreeMap<String, u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create state directory {}", parent.display()))?;
    }

    let json = serde_json::to_vec_pretty(failures)
        .with_context(|| format!("failed to serialize state file {}", path.display()))?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, json)
        .with_context(|| format!("failed to write temporary state file {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to replace state file {}", path.display()))?;

    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cvm_announcement_cleaner=info".parse().unwrap())
                .add_directive("contextvm_sdk=warn".parse().unwrap()),
        )
        .try_init();
}

fn print_help() {
    println!(
        "cvm-announcement-cleaner\n\n\
Usage:\n\
  cargo run --manifest-path tools/cvm-announcement-cleaner/Cargo.toml -- [options]\n\n\
Options:\n\
  --strfry-bin <path>          Path to the strfry binary [default: ./strfry]\n\
  --state-file <path>          Path to the persisted failure state JSON [default: /var/lib/strfry/cvm-announcement-cleaner-state.json]\n\
  --local-relay <url>          Local relay URL used for fallback probing [default: ws://127.0.0.1:7777]\n\
  --timeout-seconds <n>        Network timeout in seconds [default: 10]\n\
  --failure-threshold <n>      Consecutive failed rounds required before deletion [default: 3]\n\
  --rounds <n>                 Number of rounds to execute in this process [default: 3]\n\
  --interval-seconds <n>       Delay between rounds [default: 3600]\n\
  --dry-run                    Log deletion decisions without deleting\n\
  --help                       Show this message\n"
    );
}
