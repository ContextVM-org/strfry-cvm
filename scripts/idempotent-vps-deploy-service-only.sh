#!/bin/bash

## Service-only strfry deployment helper.
##
## Deploys strfry on the local Ubuntu VPS, installs a Perl write-policy plugin,
## and sets up a systemd service. Reverse proxy and TLS are intentionally left
## out so they can be managed separately (for example by Caddy).

set -euo pipefail

if [ "$#" -gt 4 ]; then
    echo "Deploys strfry service on the current Ubuntu VPS without configuring a reverse proxy."
    echo
    echo "Usage: $0 [HOST] [ADMIN_EMAIL] [ADMIN_PUBKEY] [REPO_REF]"
    echo "Example: $0 relay.example.com admin@example.com <32-byte-hex-pubkey> master"
    echo "Example: $0"
    exit 1
fi

HOST="${1:-$(hostname -f 2>/dev/null || hostname)}"
ADMIN_EMAIL="${2:-}"
ADMIN_PUBKEY="${3:-}"
REPO_REF="${4:-master}"

set -x

if [ "$(id -u)" -ne 0 ]; then
    echo "This script must be run as root on the target VPS." >&2
    exit 1
fi

STRFRY_USER="strfry"
STRFRY_GROUP="strfry"
STRFRY_HOME="/var/lib/strfry"
STRFRY_DB_DIR="${STRFRY_HOME}/db"
STRFRY_PLUGIN_DIR="/opt/strfry/plugins"
STRFRY_PLUGIN_PATH="${STRFRY_PLUGIN_DIR}/write-policy-allowed-kinds.pl"
STRFRY_RETENTION_PATH="${STRFRY_PLUGIN_DIR}/cleanup-old-kind-1059.pl"
STRFRY_CONFIG="/etc/strfry.conf"
STRFRY_BUILD_DIR="/tmp/strfry-build"
STRFRY_REPO_DIR_NAME="strfry-cvm"
STRFRY_REPO_OWNER="ContextVM-org"
STRFRY_REPO_NAME="strfry-cvm"
STRFRY_GITHUB_BASE_URL="https://github.com/${STRFRY_REPO_OWNER}/${STRFRY_REPO_NAME}"
STRFRY_BINARY_PATH="/usr/local/bin/strfry"
ANNOUNCEMENT_CLEANER_BINARY_PATH="/usr/local/bin/cvm-announcement-cleaner"
STRFRY_REPO_URL="https://github.com/${STRFRY_REPO_OWNER}/${STRFRY_REPO_NAME}.git"
STRFRY_ASSET_BASENAME="strfry-linux-amd64"
ANNOUNCEMENT_CLEANER_ASSET_BASENAME="cvm-announcement-cleaner-linux-amd64"

echo "--- Starting service-only strfry deployment on ${HOST} ---"

fetch_release_asset() {
    local asset_name="$1"
    local output_path="$2"
    local ref_name="$3"
    local api_url release_json asset_url tmp_dir tmp_file

    api_url="https://api.github.com/repos/${STRFRY_REPO_OWNER}/${STRFRY_REPO_NAME}/releases/tags/${ref_name}"
    release_json="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api_url" 2>/dev/null)" || return 1
    asset_url="$(printf '%s\n' "$release_json" | python3 -c 'import json, sys
data = json.load(sys.stdin)
name = sys.argv[1]
for asset in data.get("assets", []):
    if asset.get("name") == name:
        print(asset.get("browser_download_url", ""))
        break
' "$asset_name" 2>/dev/null)"

    if [ -z "$asset_url" ]; then
        echo "No release asset named ${asset_name} found for ${ref_name}" >&2
        return 1
    fi

    tmp_dir="$(dirname "$output_path")"
    mkdir -p "$tmp_dir"
    tmp_file="$(mktemp "${tmp_dir}/.${asset_name}.XXXXXX")"
    if ! curl -fsSL "$asset_url" -o "$tmp_file"; then
        echo "Failed to download release asset ${asset_name} from ${asset_url}" >&2
        rm -f "$tmp_file"
        return 1
    fi

    install -m 0755 "$tmp_file" "$output_path"
    rm -f "$tmp_file"
}

checkout_repo_ref() {
    git fetch --force --tags origin

    if git rev-parse -q --verify "refs/remotes/origin/${REPO_REF}" >/dev/null; then
        git checkout -B deploy-ref "origin/${REPO_REF}"
        return 0
    fi

    if git rev-parse -q --verify "refs/tags/${REPO_REF}" >/dev/null; then
        git checkout -B deploy-ref "refs/tags/${REPO_REF}"
        return 0
    fi

    echo "Could not resolve REPO_REF=${REPO_REF} as either a branch or a tag in ${STRFRY_REPO_URL}" >&2
    return 1
}

install_cleaner_binary() {
    local asset_name
    asset_name="${ANNOUNCEMENT_CLEANER_ASSET_BASENAME}"

    echo "Attempting to fetch prebuilt cleaner binary from release ${REPO_REF}"
    if fetch_release_asset "$asset_name" "$ANNOUNCEMENT_CLEANER_BINARY_PATH" "$REPO_REF"; then
        echo "Installed prebuilt cleaner binary from ${STRFRY_GITHUB_BASE_URL} release ${REPO_REF}"
        return 0
    fi

    echo "Prebuilt cleaner binary could not be installed for release ${REPO_REF}; falling back to local cargo build"
    cargo build --manifest-path "$STRFRY_BUILD_DIR/tools/cvm-announcement-cleaner/Cargo.toml" --release
    cp "$STRFRY_BUILD_DIR/tools/cvm-announcement-cleaner/target/release/cvm-announcement-cleaner" "$ANNOUNCEMENT_CLEANER_BINARY_PATH"
    chmod 0755 "$ANNOUNCEMENT_CLEANER_BINARY_PATH"
}

install_strfry_binary() {
    local asset_name
    asset_name="${STRFRY_ASSET_BASENAME}"

    echo "Attempting to fetch prebuilt strfry binary from release ${REPO_REF}"
    if fetch_release_asset "$asset_name" "$STRFRY_BINARY_PATH" "$REPO_REF"; then
        echo "Installed prebuilt strfry binary from ${STRFRY_GITHUB_BASE_URL} release ${REPO_REF}"
        return 0
    fi

    echo "Prebuilt strfry binary could not be installed for release ${REPO_REF}; falling back to local make build"
    make -j"$(nproc)"
    cp "$STRFRY_BUILD_DIR/strfry" "$STRFRY_BINARY_PATH"
    chmod 0755 "$STRFRY_BINARY_PATH"
}

apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates curl git g++ make pkg-config libtool perl python3 \
    libssl-dev zlib1g-dev liblmdb-dev libflatbuffers-dev \
    libsecp256k1-dev libzstd-dev

if [ "$(free -m | awk '/^Swap/ {print $2}')" -lt 4000 ]; then
    if [ ! -f /swapfile ]; then
        fallocate -l 4G /swapfile
        chmod 600 /swapfile
        mkswap /swapfile
    fi
    swapon /swapfile || true
    if ! grep -q '^/swapfile ' /etc/fstab; then
        echo '/swapfile none swap sw 0 0' >> /etc/fstab
    fi
fi

if ! getent group "$STRFRY_GROUP" >/dev/null; then
    addgroup --system "$STRFRY_GROUP"
fi

if ! id "$STRFRY_USER" >/dev/null 2>&1; then
    adduser --system --ingroup "$STRFRY_GROUP" --home "$STRFRY_HOME" --shell /usr/sbin/nologin --no-create-home "$STRFRY_USER"
fi

mkdir -p "$STRFRY_HOME" "$STRFRY_DB_DIR" "$STRFRY_PLUGIN_DIR"
chown -R "$STRFRY_USER":"$STRFRY_GROUP" "$STRFRY_HOME"
chmod 0750 "$STRFRY_DB_DIR"
chmod 0755 "$STRFRY_PLUGIN_DIR"

if [ -d "$STRFRY_BUILD_DIR/.git" ]; then
    cd "$STRFRY_BUILD_DIR"
    checkout_repo_ref
    git submodule update --init --recursive
else
    rm -rf "$STRFRY_BUILD_DIR"
    git clone --branch "$REPO_REF" "$STRFRY_REPO_URL" "$STRFRY_BUILD_DIR"
    cd "$STRFRY_BUILD_DIR"
    git submodule update --init --recursive
    make setup-golpe
fi

cd "$STRFRY_BUILD_DIR"
make setup-golpe

if systemctl list-unit-files strfry.service --no-legend 2>/dev/null | grep -q '^strfry\.service'; then
    systemctl stop strfry || true
fi

install_strfry_binary
install_cleaner_binary

cat > "$STRFRY_PLUGIN_PATH" << 'PLUGIN_EOF'
#!/usr/bin/env perl

use strict;
use warnings;
use JSON::PP qw(decode_json encode_json);

$| = 1;

sub is_allowed_kind {
    my ($kind) = @_;

    return 1 if $kind == 1059;
    return 1 if $kind == 21059;
    return 1 if $kind == 25910;
    return 1 if $kind >= 10000 && $kind <= 19999;

    return 0;
}

while (my $line = <STDIN>) {
    chomp $line;

    my $req = eval { decode_json($line) };
    if ($@) {
        print STDERR "failed to decode JSON input: $@\n";
        next;
    }

    if (($req->{type} // '') ne 'new') {
        print STDERR "unexpected request type\n";
        next;
    }

    my $event = $req->{event} // {};
    my $id = $event->{id};
    my $kind = $event->{kind};

    my $res = { id => $id };

    if (defined $kind && is_allowed_kind($kind)) {
        $res->{action} = 'accept';
    } else {
        $res->{action} = 'reject';
        $res->{msg} = defined $kind
            ? "blocked: event kind $kind is not allowed"
            : 'blocked: missing event kind';
    }

    print encode_json($res), "\n";
}
PLUGIN_EOF

chown root:"$STRFRY_GROUP" "$STRFRY_PLUGIN_PATH"
chmod 0750 "$STRFRY_PLUGIN_PATH"

install -m 0750 ./scripts/cleanup-old-kind-1059.pl "$STRFRY_RETENTION_PATH"
chown root:"$STRFRY_GROUP" "$STRFRY_RETENTION_PATH"

cat > "$STRFRY_CONFIG" << CONFIG_EOF
db = "${STRFRY_DB_DIR}/"

relay {
    bind = "127.0.0.1"
    port = 7777
    nofiles = 65536
    realIpHeader = "x-forwarded-for"

    info {
        name = "strfry relay at ${HOST}"
        description = "A strfry relay running at ${HOST}"
        pubkey = "${ADMIN_PUBKEY}"
        contact = "${ADMIN_EMAIL}"
        nips = ""
    }

    writePolicy {
        plugin = "${STRFRY_PLUGIN_PATH}"
        timeoutSeconds = 10
    }

    compression {
        enabled = true
        slidingWindow = true
    }

    logging {
        invalidEvents = true
    }

    negentropy {
        enabled = true
        maxSyncEvents = 1000000
    }
}
CONFIG_EOF

chown root:"$STRFRY_GROUP" "$STRFRY_CONFIG"
chmod 0640 "$STRFRY_CONFIG"

cat > /etc/systemd/system/strfry.service << 'SERVICE_EOF'
[Unit]
Description=strfry nostr relay
After=network.target

[Service]
User=strfry
Group=strfry
ExecStart=/usr/local/bin/strfry relay --config /etc/strfry.conf
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
NoNewPrivileges=yes
ProtectHome=yes
ProtectSystem=full
ReadWritePaths=/var/lib/strfry

[Install]
WantedBy=multi-user.target
SERVICE_EOF

cat > /etc/systemd/system/cvm-announcement-cleaner.service << 'CLEANER_SERVICE_EOF'
[Unit]
Description=ContextVM announcement cleaner
After=network-online.target strfry.service
Wants=network-online.target

[Service]
User=strfry
Group=strfry
Type=oneshot
ExecStart=/usr/local/bin/cvm-announcement-cleaner --strfry-bin /usr/local/bin/strfry --local-relay ws://127.0.0.1:7777 --failure-threshold 1 --rounds 1
NoNewPrivileges=yes
ProtectHome=yes
ProtectSystem=full
ReadWritePaths=/var/lib/strfry
CLEANER_SERVICE_EOF

cat > /etc/systemd/system/cvm-announcement-cleaner.timer << 'CLEANER_TIMER_EOF'
[Unit]
Description=Run ContextVM announcement cleaner hourly

[Timer]
OnBootSec=10m
OnUnitActiveSec=1h
Persistent=true

[Install]
WantedBy=timers.target
CLEANER_TIMER_EOF

cat > /etc/systemd/system/strfry-1059-retention.service << RETENTION_SERVICE_EOF
[Unit]
Description=Delete old kind 1059 events from strfry
After=network-online.target strfry.service
Wants=network-online.target

[Service]
Type=oneshot
User=strfry
Group=strfry
ExecStart=${STRFRY_RETENTION_PATH} --strfry-bin /usr/local/bin/strfry --max-age-seconds 86400 --kind 1059
NoNewPrivileges=yes
ProtectHome=yes
ProtectSystem=full
ReadWritePaths=/var/lib/strfry
RETENTION_SERVICE_EOF

cat > /etc/systemd/system/strfry-1059-retention.timer << 'RETENTION_TIMER_EOF'
[Unit]
Description=Run strfry kind 1059 retention cleanup daily

[Timer]
OnBootSec=15m
OnUnitActiveSec=1d
Persistent=true

[Install]
WantedBy=timers.target
RETENTION_TIMER_EOF

systemctl daemon-reload
systemctl enable strfry
systemctl enable cvm-announcement-cleaner.timer
systemctl enable strfry-1059-retention.timer
systemctl restart strfry
systemctl restart cvm-announcement-cleaner.timer
systemctl restart strfry-1059-retention.timer

echo "--- strfry service deployment complete ---"
echo "Binary:  ${STRFRY_BINARY_PATH}"
echo "Cleaner: ${ANNOUNCEMENT_CLEANER_BINARY_PATH}"
echo "Config:  ${STRFRY_CONFIG}"
echo "DB:      ${STRFRY_DB_DIR}"
echo "Plugin:  ${STRFRY_PLUGIN_PATH}"
echo "Retention: ${STRFRY_RETENTION_PATH}"
echo "Logs:    journalctl -u strfry -f"
echo "Cleaner Logs: journalctl -u cvm-announcement-cleaner -f"
echo "Retention Logs: journalctl -u strfry-1059-retention -f"
echo "Status:  systemctl status strfry"
echo "Cleaner Service Status: systemctl status cvm-announcement-cleaner"
echo "Cleaner Timer Status: systemctl status cvm-announcement-cleaner.timer"
echo "Cleaner Schedule: systemctl list-timers cvm-announcement-cleaner.timer"
echo "Retention Timer Status: systemctl status strfry-1059-retention.timer"
echo "Retention Schedule: systemctl list-timers strfry-1059-retention.timer"
