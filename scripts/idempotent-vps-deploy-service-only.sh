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
STRFRY_CONFIG="/etc/strfry.conf"
STRFRY_BUILD_DIR="/tmp/strfry-build"
STRFRY_BINARY_PATH="/usr/local/bin/strfry"
STRFRY_REPO_URL="https://github.com/hoytech/strfry.git"

echo "--- Starting service-only strfry deployment on ${HOST} ---"

apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates git g++ make pkg-config libtool perl \
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
    git fetch origin
    git reset --hard "origin/$REPO_REF"
    git submodule update --init --recursive
else
    rm -rf "$STRFRY_BUILD_DIR"
    git clone --branch "$REPO_REF" "$STRFRY_REPO_URL" "$STRFRY_BUILD_DIR"
    cd "$STRFRY_BUILD_DIR"
    git submodule update --init --recursive
    make setup-golpe
fi

make -j"$(nproc)"

if systemctl list-unit-files strfry.service --no-legend 2>/dev/null | grep -q '^strfry\.service'; then
    systemctl stop strfry || true
fi

cp "$STRFRY_BUILD_DIR/strfry" "$STRFRY_BINARY_PATH"
chmod 0755 "$STRFRY_BINARY_PATH"

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

systemctl daemon-reload
systemctl enable strfry
systemctl restart strfry

echo "--- strfry service deployment complete ---"
echo "Binary:  ${STRFRY_BINARY_PATH}"
echo "Config:  ${STRFRY_CONFIG}"
echo "DB:      ${STRFRY_DB_DIR}"
echo "Plugin:  ${STRFRY_PLUGIN_PATH}"
echo "Logs:    journalctl -u strfry -f"
echo "Status:  systemctl status strfry"
