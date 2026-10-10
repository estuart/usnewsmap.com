#!/usr/bin/env bash
# Set up an on-prem OCR node (#296, ADR-0015). Runs on the node as root;
# scripts/ocr-node/onboard.sh copies it there and runs it with sudo.
#
#   node-setup.sh --tenant ID --subscription ID --resource-group NAME \
#     --location REGION --name ARC_NAME [--tags K=V,K=V] [--force-unsupported] \
#     < token
#
# Reads a short-lived ARM access token from stdin (one line). It never goes
# on a command line or to disk: it waits in a 0600 file under /run (tmpfs)
# while `azcmagent connect --config` reads it, and is deleted straight after.
# On a node that's already connected the token isn't used.
#
# Installs python3, podman and chrony;
# creates the `usnm` system user; installs the Azure Connected Machine
# agent, turns off extensions, guest configuration and incoming connections
# BEFORE it connects; connects; installs the worker's systemd unit, disabled
# (the worker arrives in a later change); and checks that `usnm` can get a
# storage token from the agent. Safe to run again.
#
# AlmaLinux 9 or RHEL 10 (Arc supports both; it doesn't support Fedora),
# unless --force-unsupported.
set -euo pipefail
usage() { sed -n '5,7s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

TENANT="" SUBSCRIPTION="" GROUP="" LOCATION="" NAME="" TAGS="" FORCE=false
while [ $# -gt 0 ]; do
  case "$1" in
    --tenant) TENANT=${2:-}; shift 2 ;;
    --subscription) SUBSCRIPTION=${2:-}; shift 2 ;;
    --resource-group) GROUP=${2:-}; shift 2 ;;
    --location) LOCATION=${2:-}; shift 2 ;;
    --name) NAME=${2:-}; shift 2 ;;
    --tags) TAGS=${2:-}; shift 2 ;;
    --force-unsupported) FORCE=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
for pair in TENANT:tenant SUBSCRIPTION:subscription GROUP:resource-group LOCATION:location NAME:name; do
  var=${pair%%:*}
  [ -n "${!var}" ] || { echo "error: missing --${pair#*:}" >&2; usage; }
done
[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"

# The token first, before anything else can read stdin.
TOKEN=""
if [ ! -t 0 ]; then
  IFS= read -r TOKEN || true
fi
[ -z "$TOKEN" ] || [[ $TOKEN =~ ^[A-Za-z0-9._-]+$ ]] || die "the token on stdin isn't a JWT"

step "Checking the OS"
# shellcheck source=/dev/null
. /etc/os-release
supported=false
case "${ID:-}/${VERSION_ID:-}" in almalinux/9|almalinux/9.*|rhel/10|rhel/10.*) supported=true ;; esac
if [ "$supported" != true ]; then
  [ "$FORCE" = true ] || die "${PRETTY_NAME:-this OS} is neither AlmaLinux 9 nor RHEL 10 (pass --force-unsupported to try anyway)"
  echo "warning: ${PRETTY_NAME:-unknown OS} is neither AlmaLinux 9 nor RHEL 10; continuing (--force-unsupported)"
fi
echo "${PRETTY_NAME:-unknown}, $(uname -m), $(nproc) CPUs"
# Tesseract and the neural engines have AVX2 code paths; a VM with a generic
# CPU type hides them, and speed measurements are then wrong (#296).
grep -qw avx2 /proc/cpuinfo || echo "warning: no AVX2: on Proxmox, set the VM's CPU type to host"

step "Installing packages"
# No OCR engine here. EL9's AppStream has only Tesseract 4.1.1 (with 4.1.0
# language data) and EPEL 9 has no Tesseract at all; the engines the survey
# compares arrive as pinned container images run with podman, or a source
# build, in a later change. This installs what the agent, the worker and
# the containers need, all from the distribution's own repositories.
dnf -y -q install python3 podman chrony curl
systemctl enable --now chronyd > /dev/null
podman --version

step "SSH key exchange"
# AlmaLinux 9.8's OpenSSH 9.9 supports post-quantum key exchange, but the
# DEFAULT crypto policy leaves it out for sshd, so clients warn. Put the
# hybrid algorithms this sshd supports first, in a drop-in that loads before
# 50-redhat.conf; sshd only, the system crypto policy is unchanged. Skipped
# when sshd already offers them first (RHEL 10's default may).
current=$(sshd -T 2> /dev/null | awk '$1 == "kexalgorithms" { print $2 }')
pq=""
for alg in mlkem768x25519-sha256 sntrup761x25519-sha512 sntrup761x25519-sha512@openssh.com; do
  ! ssh -Q kex | grep -xF "$alg" > /dev/null || pq="${pq:+$pq,}$alg"
done
if [ -z "$current" ] || [ -z "$pq" ]; then
  echo "sshd offers no post-quantum key exchange to add; leaving it"
elif [ "${current#"$pq"}" != "$current" ]; then
  echo "sshd already offers $pq first"
else
  rest=$(tr ',' '\n' <<< "$current" | grep -vxF -e mlkem768x25519-sha256 -e sntrup761x25519-sha512 \
    -e sntrup761x25519-sha512@openssh.com | paste -sd, -)
  conf=/etc/ssh/sshd_config.d/40-pq-kex.conf
  (umask 077; printf 'KexAlgorithms %s\n' "$pq${rest:+,$rest}" > "$conf")
  if sshd -t; then
    systemctl reload sshd
    echo "sshd now offers $pq first ($conf)"
  else
    rm -f "$conf"
    echo "warning: sshd rejected the post-quantum key exchange list; left unchanged"
  fi
fi

step "Creating the usnm user"
if id usnm > /dev/null 2>&1; then
  echo "usnm exists"
else
  useradd --system --home-dir /var/lib/usnm --create-home --shell /sbin/nologin usnm
  echo "created usnm"
fi

# A directory on tmpfs for the installer and the token; removed on exit.
WORK=$(mktemp -d /run/usnm-onboard.XXXXXX)
chmod 700 "$WORK"
trap 'rm -rf "$WORK"' EXIT

step "Installing the Azure Connected Machine agent"
if command -v azcmagent > /dev/null; then
  echo "installed: $(azcmagent version 2> /dev/null | head -1)"
else
  # Microsoft's documented Linux install: download the script, then run it.
  curl -fsSL -o "$WORK/install_linux_azcmagent.sh" https://aka.ms/azcmagent
  bash "$WORK/install_linux_azcmagent.sh"
fi

step "Locking the agent down"
# Before connecting, so Azure never gets a moment to install an extension or
# run a command here. Anyone who is root on the node can undo these; nobody
# in Azure can.
#   extensions.enabled false:          no VM extensions (no Run Command, no Custom Script)
#   guestconfiguration.enabled false:  no machine configuration policies
#   incomingconnections.enabled false: no SSH or Windows Admin Center through Arc
azcmagent config set extensions.enabled false
azcmagent config set guestconfiguration.enabled false
azcmagent config set incomingconnections.enabled false

step "Connecting to Azure Arc"
if azcmagent show 2> /dev/null | grep -Eq '^[[:space:]]*Agent Status[[:space:]]*:[[:space:]]*Connected'; then
  echo "already connected; skipping"
else
  [ -n "$TOKEN" ] || die "not connected and no access token on stdin"
  # --config reads flags from a file, so the token stays out of the process
  # list. The file is on tmpfs, readable by root only, and deleted at once.
  conf="$WORK/connect.json"
  (umask 077; printf '{"access-token": "%s"}\n' "$TOKEN" > "$conf")
  connect_args=(
    --resource-group "$GROUP" --tenant-id "$TENANT" --subscription-id "$SUBSCRIPTION"
    --location "$LOCATION" --resource-name "$NAME" --cloud AzureCloud
  )
  [ -z "$TAGS" ] || connect_args+=(--tags "$TAGS")
  status=0
  azcmagent connect "${connect_args[@]}" --config "$conf" || status=$?
  rm -f "$conf"
  [ "$status" -eq 0 ] || die "azcmagent connect failed ($status)"
fi
TOKEN=""

step "Giving usnm the agent's token access"
# The agent's identity endpoint hands a token only to callers that can read
# its challenge files, which belong to the himds group.
getent group himds > /dev/null || die "no himds group; is the agent installed?"
usermod -aG himds usnm

step "Installing usnm-ocr-worker.service (disabled)"
cat > /etc/systemd/system/usnm-ocr-worker.service <<'UNIT'
# The on-prem OCR worker (#296, ADR-0015), installed by
# scripts/ocr-node/node-setup.sh. Disabled until the worker binary is in
# /usr/local/bin.
[Unit]
Description=usnewsmap OCR worker
Wants=network-online.target
After=network-online.target himdsd.service

[Service]
Type=simple
User=usnm
Group=usnm
SupplementaryGroups=himds
Environment=IDENTITY_ENDPOINT=http://localhost:40342/metadata/identity/oauth2/token
Environment=IMDS_ENDPOINT=http://localhost:40342
ExecStart=/usr/local/bin/usnm-ocr-worker
Restart=on-failure
RestartSec=30
StateDirectory=usnm-ocr
CacheDirectory=usnm-ocr
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictSUIDSGID=yes
RestrictNamespaces=yes
RestrictRealtime=yes
LockPersonality=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
echo "usnm-ocr-worker.service: $(systemctl is-enabled usnm-ocr-worker.service 2> /dev/null || true)"

step "Checking that usnm can get a storage token"
# The agent's challenge flow, as usnm: the first request is refused with a
# path to a key file only himds members can read; the second sends its
# contents. Only whether a token came back is printed, never the token.
# A machine's identity can take a minute to be ready after it connects.
ok=false
for attempt in 1 2 3 4 5 6; do
  # shellcheck disable=SC2016 # expanded by the inner shell
  if runuser -u usnm -- bash -c '
      set -eu
      url="http://localhost:40342/metadata/identity/oauth2/token?api-version=2020-06-01&resource=https%3A%2F%2Fstorage.azure.com%2F"
      realm=$(curl -s -o /dev/null -D - -H Metadata:true "$url" | tr -d "\r" |
        sed -n "s/^[Ww][Ww][Ww]-[Aa]uthenticate: *Basic realm=//p")
      [ -n "$realm" ]
      key=$(cat "$realm")
      curl -fs -H Metadata:true -H "Authorization: Basic $key" "$url" | grep -q "\"access_token\""
    ' > /dev/null 2>&1; then
    ok=true
    break
  fi
  sleep 10
done
if [ "$ok" = true ]; then
  echo "usnm got a token for https://storage.azure.com/"
else
  echo "warning: usnm got no storage token (attempt $attempt); check \`azcmagent show\` and the himds group"
fi

step "azcmagent show"
azcmagent show
step "azcmagent config list"
azcmagent config list
