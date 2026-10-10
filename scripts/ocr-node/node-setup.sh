#!/usr/bin/env bash
# Set up an on-prem OCR node (#296, ADR-0015). Runs on the node as root;
# scripts/ocr-node/onboard.sh copies it there and runs it with sudo.
#
#   node-setup.sh --subscription ID --resource-group NAME --location REGION \
#     --name ARC_NAME --exchange-account NAME [--app-insights CONNECTION_STRING] \
#     [--tags K=V,K=V] [--identity-key-store default|tpm] [--force-unsupported] < credentials
#
# Reads the onboarding service principal from stdin, three lines: tenant id,
# app id, secret. onboard.sh makes that principal for the run, with only
# Azure Connected Machine Onboarding on the group, and deletes it after. The
# secret never goes on a command line or to persistent disk: it waits in a 0600
# file under /run (tmpfs) while `azcmagent connect --config` reads it, and
# is deleted straight after. A node that's already connected needs none.
#
# Keeps the journal on disk; installs python3, podman, chrony, cron and
# rsyslog, and htop, iftop and screen from EPEL;
# creates the `usnm` system user; installs the Azure Connected Machine
# agent and, BEFORE it connects, locks it down: no extension but the Azure
# Monitor Linux Agent (it stops if any other is installed), no guest
# configuration, no incoming connections;
# connects; installs the worker's systemd unit and its settings in
# /etc/usnm/ocr-worker.env (the exchange account's URLs, the node id and
# the connection string of the nodes' own Application Insights component,
# usnm-ocr-appi-<env>, which isn't a secret: with local auth off, ingestion
# needs an Entra token too); and checks that
# `usnm` can get a token from the agent. The unit starts only once the
# worker binary is in /usr/local/bin. onboard.sh installs the monitoring
# agent itself, from Azure, after this. Safe to run again, on a connected
# node too: it applies the lockdown and the unit as they are now.
#
# AlmaLinux 9 or RHEL 10 (Arc supports both; it doesn't support Fedora),
# unless --force-unsupported.
set -euo pipefail
usage() { sed -n '5,7s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

SUBSCRIPTION="" GROUP="" LOCATION="" NAME="" EXCHANGE="" CONN="" TAGS="" FORCE=false KEY_STORE=default
while [ $# -gt 0 ]; do
  case "$1" in
    --subscription) SUBSCRIPTION=${2:-}; shift 2 ;;
    --resource-group) GROUP=${2:-}; shift 2 ;;
    --location) LOCATION=${2:-}; shift 2 ;;
    --name) NAME=${2:-}; shift 2 ;;
    --exchange-account) EXCHANGE=${2:-}; shift 2 ;;
    --app-insights) CONN=${2:-}; shift 2 ;;
    --tags) TAGS=${2:-}; shift 2 ;;
    --identity-key-store) KEY_STORE=${2:-}; shift 2 ;;
    --force-unsupported) FORCE=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
case "$KEY_STORE" in default|tpm) ;; *) echo "error: --identity-key-store is default or tpm" >&2; usage ;; esac
for pair in SUBSCRIPTION:subscription GROUP:resource-group LOCATION:location NAME:name \
  EXCHANGE:exchange-account; do
  var=${pair%%:*}
  [ -n "${!var}" ] || { echo "error: missing --${pair#*:}" >&2; usage; }
done
# The Arc name is also the worker's --node-id: 1 to 32 letters, digits or
# hyphens.
[[ $NAME =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,31}$ ]] || die "--name: 1 to 32 letters, digits or hyphens (the worker's --node-id)"
[[ $EXCHANGE =~ ^[a-z0-9]{3,24}$ ]] || die "--exchange-account: a storage account name"
# Goes into a systemd environment file in double quotes: keys, GUIDs and
# URLs only, no quotes, backslashes, dollars or spaces.
conn_re='^[A-Za-z0-9=;:/._-]+$'
[[ -z $CONN || $CONN =~ $conn_re ]] || die "--app-insights doesn't look like an Application Insights connection string"
[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"

# The credentials first, before anything else can read stdin.
TENANT="" SP_ID="" SP_SECRET=""
if [ ! -t 0 ]; then
  IFS= read -r TENANT || true
  IFS= read -r SP_ID || true
  IFS= read -r SP_SECRET || true
fi
if [ -n "$TENANT$SP_ID$SP_SECRET" ]; then
  uuid='^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
  [[ $TENANT =~ $uuid ]] || die "the first line on stdin isn't a tenant id"
  [[ $SP_ID =~ $uuid ]] || die "the second line on stdin isn't an app id"
  # It goes into JSON as is, so no quotes, backslashes or control characters.
  [[ -n $SP_SECRET && $SP_SECRET =~ ^[[:graph:]]+$ && $SP_SECRET != *[\\\"]* ]] ||
    die "the third line on stdin isn't a client secret"
fi

step "Checking the OS"
# Read os-release in a subshell: sourcing it here would overwrite NAME (the
# Arc machine name) with the distro's NAME, as happened on node 1 (#296).
# shellcheck source=/dev/null
OS_ID=$(. /etc/os-release && printf '%s' "${ID:-}")
# shellcheck source=/dev/null
OS_VERSION=$(. /etc/os-release && printf '%s' "${VERSION_ID:-}")
# shellcheck source=/dev/null
PRETTY_NAME=$(. /etc/os-release && printf '%s' "${PRETTY_NAME:-}")
supported=false
case "$OS_ID/$OS_VERSION" in almalinux/9|almalinux/9.*|rhel/10|rhel/10.*) supported=true ;; esac
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
# build, in a later change. This installs what the agents, the worker and
# the containers need, all from the distribution's own repositories. The
# Azure Monitor Agent reads syslog through rsyslog and needs python3 and
# which; Microsoft also lists initscripts, which RHEL 10 may not ship. The
# Connected Machine agent schedules its automatic upgrades with cron.
dnf -y -q install python3 podman chrony curl cronie rsyslog which
dnf -y -q install initscripts > /dev/null 2>&1 || echo "warning: no initscripts package; the Azure Monitor Agent lists it as a prerequisite"

# Tools for whoever logs in to watch the node: htop (processes), iftop
# (traffic per connection) and screen. On EL9 and EL10 they come from EPEL,
# set up the way Fedora's EPEL guide says: AlmaLinux ships epel-release in
# its extras repository and turns on CRB; RHEL takes the release package
# from Fedora and turns on CodeReady Builder.
if [ "$supported" != true ]; then
  echo "warning: htop, iftop and screen come from EPEL, set up only for AlmaLinux 9 and RHEL 10; skipping them on $PRETTY_NAME"
elif ! rpm -q epel-release > /dev/null 2>&1; then
  case "$OS_ID" in
    almalinux)
      dnf config-manager --set-enabled crb
      dnf -y -q install epel-release ;;
    rhel)
      subscription-manager repos --enable "codeready-builder-for-rhel-10-$(uname -m)-rpms"
      dnf -y -q install https://dl.fedoraproject.org/pub/epel/epel-release-latest-10.noarch.rpm ;;
    *) echo "warning: don't know how to add EPEL on $OS_ID; skipping htop, iftop and screen" ;;
  esac
fi
if [ "$supported" = true ] && rpm -q epel-release > /dev/null 2>&1; then
  dnf -y -q install htop iftop screen
fi
systemctl enable --now chronyd > /dev/null
# The agent schedules its automatic upgrades with cron.
systemctl enable --now crond > /dev/null
systemctl enable --now rsyslog > /dev/null
podman --version

step "Persistent journal"
# The AlmaLinux cloud image keeps the journal in memory only, so a reboot
# loses the logs. Keep it on disk, at most 1 GB.
mkdir -p /var/log/journal /etc/systemd/journald.conf.d
want=$'[Journal]\nStorage=persistent\nSystemMaxUse=1G'
if [ "$(cat /etc/systemd/journald.conf.d/50-usnm.conf 2> /dev/null)" = "$want" ]; then
  echo "already persistent"
else
  printf '%s\n' "$want" > /etc/systemd/journald.conf.d/50-usnm.conf
  systemd-tmpfiles --create --prefix /var/log/journal > /dev/null 2>&1 || true
  systemctl restart systemd-journald
  echo "journal kept in /var/log/journal (at most 1 GB)"
fi

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
# Rootless podman maps a container's users onto subordinate ids; a system
# user gets none, so give usnm 65,536 after the highest range in use.
for f in /etc/subuid /etc/subgid; do
  if ! grep -q '^usnm:' "$f" 2> /dev/null; then
    start=$(awk -F: 'BEGIN { m = 100000 } { e = $2 + $3; if (e > m) m = e } END { print m }' "$f" 2> /dev/null || echo 100000)
    if [ "$f" = /etc/subuid ]; then
      usermod --add-subuids "$start-$((start + 65535))" usnm
    else
      usermod --add-subgids "$start-$((start + 65535))" usnm
    fi
    echo "usnm: ${f#/etc/} $start-$((start + 65535))"
  fi
done

# A directory on tmpfs for the installer and the secret; removed on exit.
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
# Connect asks for automatic upgrade (agent 1.57 or later) and the
# TPM-backed key store (seen in 1.68's azcmagent connect); an older agent
# would refuse the flags or never upgrade itself. The install script added
# Microsoft's package repository, so dnf can bring it up to date.
MIN_AGENT=1.68
# Empty output, not a failure, when the agent is missing or prints something
# unexpected, so the fallback below decides (set -e and pipefail are on).
agent_version() { { azcmagent version 2> /dev/null || true; } | grep -oE '[0-9]+\.[0-9]+' | head -1 || true; }
version_ok() { [ "$(printf '%s\n%s\n' "$MIN_AGENT" "$1" | sort -t. -k1,1n -k2,2n | head -1)" = "$MIN_AGENT" ]; }
v=$(agent_version)
if [ -z "$v" ] || ! version_ok "$v"; then
  echo "agent ${v:-of unknown version} is older than $MIN_AGENT; upgrading"
  dnf -y -q upgrade azcmagent
  v=$(agent_version)
  if [ -z "$v" ] || ! version_ok "$v"; then
    die "azcmagent is ${v:-of unknown version}, older than $MIN_AGENT, after dnf upgrade"
  fi
fi
echo "agent $v"

step "Locking the agent down"
# Before connecting, so Azure never gets a moment to install anything else
# or run a command here, and on every run, so a node connected with
# extensions off gets the allow list too. Anyone who is root on the node can
# undo these; nobody in Azure can.
#   extensions.allowlist:              the Azure Monitor Linux Agent only; no
#                                      Run Command, no Custom Script, nothing else
#   extensions.enabled true:           the extension manager runs, for that agent
#   guestconfiguration.enabled false:  no machine configuration policies
#   incomingconnections.enabled false: no SSH or Windows Admin Center through Arc
# The allow list is set and checked before the extension manager is turned
# on, because an empty list allows every extension, and so is the absence of
# any other installed extension (below). Full mode with our own
# list, not monitor mode (config.mode monitor): monitor mode's list is
# Microsoft's, covers the Defender and other security extensions as well,
# and changes with agent updates.
AMA=Microsoft.Azure.Monitor/AzureMonitorLinuxAgent
# A property's value as one line: a list's entries joined by commas, no
# brackets, quotes or spaces. Empty if the agent can't say.
config_get() {
  azcmagent config get "$1" -j 2> /dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin).get("Data", "")
if isinstance(d, list):
    d = ",".join(str(x) for x in d)
print("".join(c for c in str(d) if c not in "[]\" "))
' 2> /dev/null || true
}
# Sets a property only if it differs, so a run on a set-up node restarts
# nothing.
config_set() {
  if [ "$(config_get "$1")" = "$2" ]; then
    echo "$1: $2"
  else
    azcmagent config set "$1" "$2"
  fi
}
mode=$(config_get config.mode)
[ "$mode" = full ] ||
  die "the agent is in ${mode:-an unknown} mode; this script sets its own allow list, which needs full mode (azcmagent config set config.mode full)"
# A list that needs changing is changed with extensions off, so nothing runs
# under the old (or an empty) list meanwhile and a failed set leaves them
# off. An agent that has never been set up has extensions on and no list. A
# list that's already right restarts nothing.
if [ "$(config_get extensions.allowlist)" = "$AMA" ]; then
  echo "extensions.allowlist: $AMA"
else
  config_set extensions.enabled false
  azcmagent config set extensions.allowlist "$AMA"
  allow=$(config_get extensions.allowlist)
  [ "$allow" = "$AMA" ] || die "extensions.allowlist reads \"$allow\", not $AMA; extensions left off"
fi
# The allow list stops new installs, updates and upgrades, but doesn't remove
# an extension that was installed before it, and turning the extension
# manager on would run that one again (Microsoft). Extensions install to
# /var/lib/waagent/<publisher>.<type>-<version>; anything there but the
# Azure Monitor Agent stops this, with extensions off. onboard.sh checks
# Azure's side.
others=""
for ext in /var/lib/waagent/*-[0-9]*/; do
  [ -d "$ext" ] || continue
  ext=${ext%/}
  ext=${ext##*/}
  case "$ext" in Microsoft.Azure.Monitor.AzureMonitorLinuxAgent-*) continue ;; esac
  others="${others:+$others,}$ext"
done
if [ -n "$others" ]; then
  azcmagent config set extensions.enabled false
  die "extensions other than the Azure Monitor Agent are installed: $others (in /var/lib/waagent);" \
    "extensions turned off. Remove each one (systemctl stop extd, then azcmagent extension remove" \
    "--name <type>), delete it in Azure too, and run this again"
fi
config_set extensions.enabled true
config_set guestconfiguration.enabled false
config_set incomingconnections.enabled false

step "Connecting to Azure Arc"
if azcmagent show 2> /dev/null | grep -Eq '^[[:space:]]*Agent Status[[:space:]]*:[[:space:]]*Connected'; then
  echo "already connected; skipping"
else
  [ -n "$SP_SECRET" ] || die "not connected and no service principal on stdin"
  # --config reads flags from a file, so the secret stays out of the process
  # list (Microsoft's advice for service principal secrets). The file is on
  # tmpfs, readable by root only, and deleted as soon as the connect ends.
  conf="$WORK/connect.json"
  (umask 077; printf '{"service-principal-id": "%s", "service-principal-secret": "%s"}\n' \
    "$SP_ID" "$SP_SECRET" > "$conf")
  connect_args=(
    --resource-group "$GROUP" --tenant-id "$TENANT" --subscription-id "$SUBSCRIPTION"
    --location "$LOCATION" --resource-name "$NAME" --cloud AzureCloud
    --enable-automatic-upgrade
  )
  [ -z "$TAGS" ] || connect_args+=(--tags "$TAGS")
  # --identity-key-store tpm keeps the machine identity's private key in the
  # TPM (the agent's TPM-backed identity, in preview), so it can't be copied
  # off the disk. Azure accepts it only once the subscription's
  # Microsoft.HybridCompute/HybridComputePreview feature is registered, which
  # needs Microsoft's approval; onboard.sh asks for tpm only then. The key
  # store is chosen at connect; changing it later means reconnecting, which
  # makes a new machine and identity. proxmox-create-vm.sh gives VMs a vTPM 2.0.
  if [ "$KEY_STORE" = tpm ]; then
    [ -c /dev/tpmrm0 ] || die "--identity-key-store tpm, but this node has no TPM (/dev/tpmrm0)"
    connect_args+=(--identity-key-store tpm)
    echo "the identity's key goes in the TPM"
  else
    echo "the identity's key stays in the agent's default store on disk"
  fi
  # A new secret and role assignment can take a few minutes to reach Entra
  # and Azure everywhere, so a refused connect is tried again.
  status=0
  for attempt in 1 2 3 4 5 6; do
    status=0
    # PIPESTATUS is read in the || branch, before any other command replaces it.
    azcmagent connect "${connect_args[@]}" --config "$conf" 2>&1 | tee "$WORK/connect.log" ||
      status=${PIPESTATUS[0]}
    [ "$status" -ne 0 ] || break
    [ "$attempt" -lt 6 ] || break
    # A 400 from the Hybrid Compute service (HCRP400) is a refusal, such as an
    # unregistered preview feature; waiting won't change it.
    if grep -q HCRP400 "$WORK/connect.log"; then break; fi
    echo "azcmagent connect failed ($status), attempt $attempt; trying again in 30 s"
    sleep 30
  done
  rm -f "$conf"
  [ "$status" -eq 0 ] || die "azcmagent connect failed ($status) after $attempt attempts"
fi
SP_SECRET=""

step "Giving usnm the agent's token access"
# The agent's identity endpoint hands a token only to callers that can read
# its challenge files, which belong to the himds group.
getent group himds > /dev/null || die "no himds group; is the agent installed?"
usermod -aG himds usnm

step "The worker's settings and unit"
# /etc/usnm/ocr-worker.env holds what this script knows: the exchange
# account's URLs, the node id, the pages in flight (one per CPU; the worker
# gives Tesseract one thread per page) and the nodes' Application Insights
# connection string (--app-insights; onboard.sh passes
# OCR_NODE_APPINSIGHTS). It is rewritten on every run. Local changes go in
# /etc/usnm/ocr-worker.local.env, read after it and never touched here:
# USNM_OCR_ARGS (all the extra flags, e.g. "--concurrency 2 --api-gap-ms
# 7000" with two nodes sharing loc.gov's rate) or RUST_LOG. Both are
# root's, readable by all: nothing in them is a secret.
install -d -o root -g root -m 0755 /etc/usnm
envfile=/etc/usnm/ocr-worker.env
want_env=$(cat <<ENV
# Written by scripts/ocr-node/node-setup.sh on every run; local changes go
# in /etc/usnm/ocr-worker.local.env.
USNM_OCR_QUEUE_URL=https://$EXCHANGE.queue.core.windows.net
USNM_OCR_OUT_URL=https://$EXCHANGE.blob.core.windows.net/ocr-out
USNM_OCR_NODE_ID=$NAME
USNM_OCR_ARGS="--concurrency $(nproc)"
APPLICATIONINSIGHTS_CONNECTION_STRING="$CONN"
ENV
)
[ -n "$CONN" ] || echo "warning: no --app-insights; the worker will log to the journal only"

# The unit. ConditionFileIsExecutable keeps it from starting, enabled or
# not, until the worker binary is installed; enable it then:
#   systemctl enable --now usnm-ocr-worker
want_unit=$(cat <<'UNIT'
# The on-prem OCR worker (#296, ADR-0015), written by
# scripts/ocr-node/node-setup.sh on every run. It starts only once the
# worker binary is in /usr/local/bin. Its settings are in /etc/usnm.
[Unit]
Description=usnewsmap OCR worker
Wants=network-online.target
After=network-online.target himdsd.service
ConditionFileIsExecutable=/usr/local/bin/usnm-ocr-worker

[Service]
Type=simple
User=usnm
Group=usnm
SupplementaryGroups=himds
Environment=IDENTITY_ENDPOINT=http://localhost:40342/metadata/identity/oauth2/token
Environment=IMDS_ENDPOINT=http://localhost:40342
EnvironmentFile=/etc/usnm/ocr-worker.env
EnvironmentFile=-/etc/usnm/ocr-worker.local.env
ExecStart=/usr/local/bin/usnm-ocr-worker run --queue-url ${USNM_OCR_QUEUE_URL} --out-url ${USNM_OCR_OUT_URL} --node-id ${USNM_OCR_NODE_ID} --shutdown-grace-secs 60 $USNM_OCR_ARGS
# On stop, SIGTERM goes to the worker alone (mixed), so Tesseract keeps
# running while the worker gives in-flight pages 60 s, then flushes
# telemetry for at most 30 s. 120 s covers both; then systemd kills what's
# left, and those pages reappear when their visibility runs out.
KillMode=mixed
TimeoutStopSec=120
Restart=on-failure
RestartSec=30
# Its JSON lines reach syslog on local0, which the Azure Monitor Agent
# collects at Info and above (infra/modules/ocr-node-monitoring.bicep).
SyslogIdentifier=usnm-ocr-worker
SyslogFacility=local0
# The worker runs Tesseract as a child process; --tesseract-bin can name a
# wrapper that runs a pinned engine image with rootless podman, which needs
# user namespaces and newuidmap (setuid / file capabilities). So the unit
# sets no NoNewPrivileges, RestrictNamespaces or empty
# CapabilityBoundingSet, and lets usnm write its home, where podman keeps
# its storage. Rootless podman also needs a runtime directory of the user's
# own, which a system user with no login session never gets
# (/run/user/<uid>), so the unit makes /run/usnm-ocr (0700) and points
# XDG_RUNTIME_DIR at it. The wrapper, and podman's settings under this unit
# (its cgroup manager), come with the engine images, in a later change that
# tests them here.
StateDirectory=usnm-ocr
CacheDirectory=usnm-ocr
RuntimeDirectory=usnm-ocr
RuntimeDirectoryMode=0700
Environment=XDG_RUNTIME_DIR=/run/usnm-ocr
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/usnm
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictSUIDSGID=yes
RestrictRealtime=yes
LockPersonality=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX

[Install]
WantedBy=multi-user.target
UNIT
)
unitfile=/etc/systemd/system/usnm-ocr-worker.service
changed=false
# Writes $2 to $1 (root's, mode 0644) if it differs; a new file is put in
# place with one rename.
put_file() {
  if [ "$(cat "$1" 2> /dev/null)" = "$2" ]; then
    echo "$1: unchanged"
    return 0
  fi
  local tmp
  tmp=$(mktemp "$1.XXXXXX")
  printf '%s\n' "$2" > "$tmp"
  chown root:root "$tmp"
  chmod 0644 "$tmp"
  mv -f "$tmp" "$1"
  echo "$1: written"
  changed=true
}
put_file "$envfile" "$want_env"
put_file "$unitfile" "$want_unit"
systemctl daemon-reload
# A running worker picks the changes up with a restart; its in-flight pages
# get the grace period and otherwise reappear later.
if [ "$changed" = true ] && systemctl is-active --quiet usnm-ocr-worker.service; then
  systemctl restart usnm-ocr-worker.service
  echo "restarted usnm-ocr-worker.service"
fi
if [ -x /usr/local/bin/usnm-ocr-worker ]; then
  echo "usnm-ocr-worker.service: $(systemctl is-enabled usnm-ocr-worker.service 2> /dev/null || true)," \
    "$(systemctl is-active usnm-ocr-worker.service 2> /dev/null || true)"
else
  echo "usnm-ocr-worker.service: waiting for /usr/local/bin/usnm-ocr-worker" \
    "($(systemctl is-enabled usnm-ocr-worker.service 2> /dev/null || true))"
fi

step "Checking that usnm can get a token"
# The agent's challenge flow, as usnm: the first request is refused with a
# path to a key file only himds members can read; the second sends its
# contents. Only whether a token came back is printed, never the token.
# The token is for Resource Manager, not storage: managed identity tokens
# are cached per resource for up to about 24 hours, and a storage token
# fetched now, before onboard.sh adds the node to its group, would lack the
# group and keep the node out of the exchange account until it expired.
# A machine's identity can take a minute to be ready after it connects.
ok=false
for attempt in 1 2 3 4 5 6; do
  # shellcheck disable=SC2016 # expanded by the inner shell
  if runuser -u usnm -- bash -c '
      set -eu
      url="http://localhost:40342/metadata/identity/oauth2/token?api-version=2020-06-01&resource=https%3A%2F%2Fmanagement.azure.com%2F"
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
  echo "usnm got a token for https://management.azure.com/"
else
  echo "warning: usnm got no token (attempt $attempt); check \`azcmagent show\` and the himds group"
fi

step "azcmagent show"
azcmagent show
step "azcmagent config list"
azcmagent config list
