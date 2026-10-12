#!/usr/bin/env bash
# Set up an on-prem OCR node (#296, ADR-0015). Runs on the node as root;
# scripts/ocr-node/onboard.sh copies it there and runs it with sudo.
#
#   node-setup.sh --subscription ID --resource-group NAME --location REGION \
#     --name ARC_NAME --exchange-account NAME [--app-insights CONNECTION_STRING] \
#     [--tags K=V,K=V] [--identity-key-store default|tpm] [--scratch-size SIZE] \
#     [--reboot-day Mon|Tue|Wed|Thu|Fri|Sat|Sun] [--force-unsupported] < credentials
#
# Reads up to five lines from stdin. The first three are the onboarding
# service principal: tenant id, app id, secret. onboard.sh makes that
# principal for the run, with only Azure Connected Machine Onboarding on the
# group, and deletes it after. The secret never goes on a command line or to
# persistent disk: it waits in a 0600 file under /run (tmpfs) while
# `azcmagent connect --config` reads it, and is deleted straight after. A
# node that's already connected needs none (three empty lines). The next two
# are, on RHEL, the Red Hat organization id and activation key, which
# onboard.sh reads from the nodes' Key Vault; they stay in this script's
# memory, never on disk. RHEL is registered first, before anything is
# installed, since its repositories need it; a registered node needs neither.
#
# On RHEL, registers with Red Hat (skipped if registered) and enables
# BaseOS, AppStream and CodeReady Builder (for EPEL).
# Keeps the journal on disk; installs python3, podman, chrony, cron and
# rsyslog, and htop, iftop and screen from EPEL; turns on dnf-automatic,
# which installs every update daily and never reboots, and a weekly reboot
# window (usnm-ocr-reboot.timer, 04:00 US Eastern on --reboot-day) that
# reboots only when an update needs it, after stopping the worker;
# creates the `usnm` system user; sets up the engine runtime of ADR-0016
# (13 §13.3): the engine account `usnmeng` with lingering, subordinate ids
# and cgroup delegation, its Podman API socket at
# /run/usnm-ocr/podman.sock shared with usnm through the group `usnmocr`,
# and the engines' scratch, a tmpfs of --scratch-size (default 2G) at
# /run/usnm-ocr/scratch; installs the Azure Connected Machine
# agent and, BEFORE it connects, locks it down: no extension but the Azure
# Monitor Linux Agent (it stops if any other is installed), no guest
# configuration, no incoming connections;
# connects; installs the worker's systemd unit (usnm-ocr-worker.service,
# copied beside this script) disabled, and its settings in
# /etc/usnm/ocr-worker.env (the exchange account's URLs, the node id and
# the connection string of the nodes' own Application Insights component,
# usnm-ocr-appi-<env>, which isn't a secret: with local auth off, ingestion
# needs an Entra token too) unless install-worker.sh has written them; and
# checks that `usnm` can get a token from the agent. The unit starts only
# once scripts/ocr-node/install-worker.sh has put the worker binary in
# /usr/local/bin. onboard.sh installs the monitoring
# agent itself, from Azure, after this. Safe to run again, on a connected
# node too: it applies the lockdown and the unit as they are now.
#
# The node mounts nothing from pve-big: the worker reads prefetched scans
# from the scan server over HTTP (#368, ADR-0015 decision 9). A mount at
# /srv/ocr-data left by an earlier version of this script is removed.
#
# AlmaLinux 9 or RHEL 10 (Arc supports both; it doesn't support Fedora),
# unless --force-unsupported, which also lets it go on with SELinux not
# enforcing.
set -euo pipefail
usage() { sed -n '5,8s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

SUBSCRIPTION="" GROUP="" LOCATION="" NAME="" EXCHANGE="" CONN="" TAGS="" FORCE=false KEY_STORE=default
SCRATCH_SIZE=2G REBOOT_DAY=""
OCR_DATA_DIR=/srv/ocr-data
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
    --scratch-size) SCRATCH_SIZE=${2:-}; shift 2 ;;
    --reboot-day) REBOOT_DAY=${2:-}; shift 2 ;;
    --force-unsupported) FORCE=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
case "$KEY_STORE" in default|tpm) ;; *) echo "error: --identity-key-store is default or tpm" >&2; usage ;; esac
[[ $SCRATCH_SIZE =~ ^[1-9][0-9]*[MG]$ ]] || die "--scratch-size: a size such as 2G or 1536M"
case "$REBOOT_DAY" in ""|Mon|Tue|Wed|Thu|Fri|Sat|Sun) ;; *) die "--reboot-day: Mon, Tue, Wed, Thu, Fri, Sat or Sun" ;; esac
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
TENANT="" SP_ID="" SP_SECRET="" RH_ORG="" RH_KEY=""
if [ ! -t 0 ]; then
  IFS= read -r TENANT || true
  IFS= read -r SP_ID || true
  IFS= read -r SP_SECRET || true
  IFS= read -r RH_ORG || true
  IFS= read -r RH_KEY || true
fi
if [ -n "$TENANT$SP_ID$SP_SECRET" ]; then
  uuid='^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
  [[ $TENANT =~ $uuid ]] || die "the first line on stdin isn't a tenant id"
  [[ $SP_ID =~ $uuid ]] || die "the second line on stdin isn't an app id"
  # It goes into JSON as is, so no quotes, backslashes or control characters.
  [[ -n $SP_SECRET && $SP_SECRET =~ ^[[:graph:]]+$ && $SP_SECRET != *[\\\"]* ]] ||
    die "the third line on stdin isn't a client secret"
fi
# Both or neither. They go to subscription-manager as separate arguments,
# never through a shell, so only printable characters are checked for.
if [ -n "$RH_ORG$RH_KEY" ]; then
  [[ $RH_ORG =~ ^[[:graph:]]+$ ]] || die "the fourth line on stdin isn't a Red Hat organization id"
  [[ $RH_KEY =~ ^[[:graph:]]+$ ]] || die "the fifth line on stdin isn't a Red Hat activation key"
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

step "Red Hat registration"
# RHEL's repositories (and so dnf below) need the system registered. With
# an activation key, subscription-manager takes the organization id and key
# as arguments, so for the few seconds it runs they're in its entry in the
# process list, readable by local users; on a new node that's root and the
# admin account (which has sudo), since the usnm user doesn't exist yet. A
# registered node is skipped, so a re-run never needs them.
if [ "$OS_ID" != rhel ]; then
  echo "not RHEL; nothing to register"
else
  command -v subscription-manager > /dev/null || die "RHEL without subscription-manager"
  if subscription-manager identity > /dev/null 2>&1; then
    echo "already registered with Red Hat"
  else
    [ -n "$RH_KEY" ] ||
      die "RHEL isn't registered and no Red Hat organization id and activation key came on stdin" \
        "(onboard.sh reads rhel-org-id and rhel-activation-key from the nodes' Key Vault)"
    subscription-manager register --org "$RH_ORG" --activationkey "$RH_KEY"
  fi
  # Enabling an enabled repository changes nothing. CodeReady Builder is
  # for EPEL, below.
  arch=$(uname -m)
  subscription-manager repos --enable "rhel-10-for-$arch-baseos-rpms" \
    --enable "rhel-10-for-$arch-appstream-rpms" --enable "codeready-builder-for-rhel-10-$arch-rpms"
fi
RH_ORG="" RH_KEY=""

step "Red Hat Lightspeed (insights-client)"
# A registered RHEL node also reports to Red Hat Lightspeed (Insights) for
# its advisories, vulnerabilities and compliance (the maintainer's request,
# 12 October 2026). insights-client uses the RHSM registration above, so it
# needs no credentials of its own, and enables its own daily upload timer.
# A node already registered with Insights is left alone.
if [ "$OS_ID" != rhel ]; then
  echo "not RHEL; no insights-client"
else
  dnf install -y insights-client
  if insights-client --status > /dev/null 2>&1; then
    echo "already registered with Red Hat Lightspeed"
  else
    insights-client --register
  fi
  # Either way, make sure the daily upload runs: offboard.sh disables it,
  # and a re-run of this script should leave a registered node reporting.
  systemctl enable --now insights-client.timer
fi

step "Installing packages"
# No OCR engine here. EL9's AppStream has only Tesseract 4.1.1 (with 4.1.0
# language data) and EPEL 9 has no Tesseract at all; the engines the survey
# compares arrive as pinned container images run with podman.
# install-worker.sh loads the worker's Tesseract image. This installs what the agents, the worker and
# the containers need, all from the distribution's own repositories. The
# Azure Monitor Agent reads syslog through rsyslog and needs python3 and
# which; Microsoft also lists initscripts, which RHEL 10 may not ship. The
# Connected Machine agent schedules its automatic upgrades with cron.
# acl (setfacl), and semanage from policycoreutils-python-utils, for the
# engine runtime.
dnf -y -q install python3 podman chrony curl cronie rsyslog which acl policycoreutils-python-utils
dnf -y -q install initscripts > /dev/null 2>&1 || echo "warning: no initscripts package; the Azure Monitor Agent lists it as a prerequisite"
# On a Proxmox VM, the guest agent reports the address and shuts the VM
# down cleanly. AlmaLinux's cloud-init installs it; an image that can't
# install packages before registration (RHEL) may not have it.
case "$(systemd-detect-virt --vm 2> /dev/null || true)" in kvm|qemu) vm=true ;; *) vm=false ;; esac
if [ "$vm" = true ]; then
  rpm -q qemu-guest-agent > /dev/null 2>&1 || dnf -y -q install qemu-guest-agent
  systemctl enable --now qemu-guest-agent > /dev/null 2>&1 ||
    echo "warning: qemu-guest-agent didn't start (is the VM's guest agent option on?)"
fi

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
      # CodeReady Builder was enabled at registration, above.
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

# Writes $2 to the file $1 (root's, mode 0644) if it differs; returns 1 if
# it was already so.
put_conf() {
  if [ "$(cat "$1" 2> /dev/null)" = "$2" ]; then
    echo "$1: unchanged"
    return 1
  fi
  mkdir -p "$(dirname "$1")"
  printf '%s\n' "$2" > "$1.new"
  chmod 0644 "$1.new"
  mv -f "$1.new" "$1"
  echo "$1: written"
}

step "Automatic updates"
# dnf-automatic installs every update, not only security ones, once a day
# (its timer: 06:00 UTC plus up to an hour), and never reboots: a reboot
# would kill pages in flight at any hour. The Connected Machine agent is
# left out; it upgrades itself (--enable-automatic-upgrade, below). Its
# runs go to the journal and /var/log/dnf.log.
# needs-restarting, from dnf-plugins-core, tells the reboot window below
# whether an update needs a reboot.
dnf -y -q install dnf-automatic dnf-plugins-core
put_conf /etc/dnf/automatic.conf '# Written by scripts/ocr-node/node-setup.sh (#296).
[commands]
upgrade_type = default
random_sleep = 0
network_online_timeout = 60
download_updates = yes
apply_updates = yes
reboot = never

[emitters]
emit_via = stdio

[base]
debuglevel = 1
excludepkgs = azcmagent' || true
systemctl enable --now dnf-automatic.timer > /dev/null
echo "dnf-automatic.timer: next $(systemctl show -P NextElapseUSecRealtime dnf-automatic.timer)"

step "Weekly reboot window"
# usnm-ocr-reboot.timer runs usnm-ocr-reboot.service at 04:00 US Eastern
# (DST included) on the node's day; the service runs
# /usr/local/sbin/usnm-ocr-reboot-if-needed (scripts/ocr-node/reboot-if-needed.sh,
# which onboard.sh copies beside this script), which reboots only when
# `dnf needs-restarting -r` says an update needs it, after stopping the
# worker (and the Serial Set fetch, where it runs); at boot,
# usnm-ocr-reboot-resume.service starts again what it stopped that
# wouldn't start by itself. Each node has its own day, so they are never
# down together.
# Without --reboot-day, a node keeps the day it has; a new node needs one.
reboot_timer=/etc/systemd/system/usnm-ocr-reboot.timer
if [ -z "$REBOOT_DAY" ]; then
  REBOOT_DAY=$(sed -n 's/^OnCalendar=\([A-Z][a-z][a-z]\) .*/\1/p' "$reboot_timer" 2> /dev/null || true)
  [ -n "$REBOOT_DAY" ] || die "no --reboot-day, and this node has no reboot window yet: give it a day no other node has"
  echo "keeping this node's reboot day, $REBOOT_DAY"
fi
reboot_src="$(dirname "$0")/reboot-if-needed.sh"
[ -r "$reboot_src" ] || die "no $reboot_src; onboard.sh copies it beside node-setup.sh"
if cmp -s "$reboot_src" /usr/local/sbin/usnm-ocr-reboot-if-needed; then
  echo "/usr/local/sbin/usnm-ocr-reboot-if-needed: unchanged"
else
  install -o root -g root -m 0755 "$reboot_src" /usr/local/sbin/usnm-ocr-reboot-if-needed
  echo "/usr/local/sbin/usnm-ocr-reboot-if-needed: written"
fi
# A program in /usr/local/sbin, labelled like one (bin_t).
! command -v restorecon > /dev/null || restorecon /usr/local/sbin/usnm-ocr-reboot-if-needed
reboot_changed=false
put_conf /etc/systemd/system/usnm-ocr-reboot.service '# Written by scripts/ocr-node/node-setup.sh (#296).
[Unit]
Description=Reboot the OCR node if an update needs it, after stopping the worker
After=dnf-automatic.service

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/usnm-ocr-reboot-if-needed --drain-timeout 1260
# A oneshot has no start timeout by default; the script bounds itself at
# 51 minutes (30 waiting for dnf-automatic, 21 stopping), so this is a
# backstop above that.
TimeoutStartSec=1h
SyslogIdentifier=usnm-ocr-reboot' && reboot_changed=true
put_conf "$reboot_timer" "# Written by scripts/ocr-node/node-setup.sh (#296).
[Unit]
Description=Weekly reboot window of the OCR node

[Timer]
OnCalendar=$REBOOT_DAY *-*-* 04:00:00 America/New_York
AccuracySec=1min
# A window missed while the node was off isn't made up at boot.
Persistent=false

[Install]
WantedBy=timers.target" && reboot_changed=true
put_conf /etc/systemd/system/usnm-ocr-reboot-resume.service '# Written by scripts/ocr-node/node-setup.sh (#296).
[Unit]
Description=Start again what the OCR node stopped for its reboot window
ConditionPathExists=/var/lib/usnm-ocr-reboot/resume
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/usnm-ocr-reboot-if-needed --resume
SyslogIdentifier=usnm-ocr-reboot

[Install]
WantedBy=multi-user.target' || true
systemctl daemon-reload
systemctl enable usnm-ocr-reboot.timer usnm-ocr-reboot-resume.service > /dev/null 2>&1
if [ "$reboot_changed" = true ]; then
  systemctl restart usnm-ocr-reboot.timer
else
  systemctl start usnm-ocr-reboot.timer
fi
echo "usnm-ocr-reboot.timer: $REBOOT_DAY 04:00 US Eastern; next $(systemctl show -P NextElapseUSecRealtime usnm-ocr-reboot.timer)"

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
# usnm needs no subordinate ids: it never runs podman itself (the engine
# runtime below). Nodes set up before kept the ones they were given.

# Rootless podman maps a container's users onto subordinate ids; a system
# user gets none, so this gives $1 65,536 after the highest range in use.
add_subids() {
  local f start
  for f in /etc/subuid /etc/subgid; do
    if ! grep -q "^$1:" "$f" 2> /dev/null; then
      start=$(awk -F: 'BEGIN { m = 100000 } { e = $2 + $3; if (e > m) m = e } END { print m }' "$f" 2> /dev/null || echo 100000)
      if [ "$f" = /etc/subuid ]; then
        usermod --add-subuids "$start-$((start + 65535))" "$1"
      else
        usermod --add-subgids "$start-$((start + 65535))" "$1"
      fi
      echo "$1: ${f#/etc/} $start-$((start + 65535))"
    fi
  done
}

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

step "The engine runtime (ADR-0016)"
# Engines run as usnmeng, a second system user with its own subordinate
# ids and its own rootless podman. It isn't in himds, so nothing it runs
# can read the Arc agent's token challenge. Its user manager (lingering
# on) runs the Podman API service, with the socket at
# /run/usnm-ocr/podman.sock, mode 0660, group usnmocr; the worker (usnm,
# also in usnmocr) runs engine containers through it with
# `podman --remote`, so it never needs user namespaces and its unit can
# set NoNewPrivileges (13 §13.3). Engines' scratch is a size-capped tmpfs
# at /run/usnm-ocr/scratch, owned by usnm: memory used by files on a tmpfs
# is charged to the cgroup of the process that writes them, so an
# engine's output counts against its own memory limit and can't fill the
# disk. scripts/ocr-node/engine-spike.sh checks all of it.
SOCK=/run/usnm-ocr/podman.sock
SCRATCH=/run/usnm-ocr/scratch
unitfile=/etc/systemd/system/usnm-ocr-worker.service
# A worker unit from before this owned /run/usnm-ocr as its runtime
# directory and removed it on every stop. It couldn't run an engine
# anyway; it's stopped here and started again with the new unit below.
was_active=false
if grep -qx 'RuntimeDirectory=usnm-ocr' "$unitfile" 2> /dev/null; then
  if systemctl is-active --quiet usnm-ocr-worker.service; then
    systemctl stop usnm-ocr-worker.service
    was_active=true
    echo "stopped usnm-ocr-worker.service (its old unit owns /run/usnm-ocr)"
  fi
fi
getent group usnmocr > /dev/null || { groupadd --system usnmocr; echo "created group usnmocr"; }
if id usnmeng > /dev/null 2>&1; then
  echo "usnmeng exists"
else
  useradd --system --user-group --home-dir /var/lib/usnmeng --create-home --shell /sbin/nologin usnmeng
  echo "created usnmeng"
fi
usermod -aG usnmocr usnmeng
usermod -aG usnmocr usnm
if id -nG usnmeng | tr ' ' '\n' | grep -qx himds; then
  gpasswd -d usnmeng himds
fi
add_subids usnmeng
ENG_UID=$(id -u usnmeng)
USNM_UID=$(id -u usnm)
OCR_GID=$(getent group usnmocr | cut -d: -f3)

# usnmeng's container storage, labelled like /var/lib/containers (podman's
# advice for storage outside the usual places): its home is under
# /var/lib, where SELinux's home directory rules don't reach.
store=/var/lib/usnmeng/.local/share/containers
install -d -o usnmeng -g usnmeng -m 0700 /var/lib/usnmeng/.local /var/lib/usnmeng/.local/share "$store"
# The engines' isolation rests on SELinux labels (13 §13.3).
selinux=$(getenforce 2> /dev/null || echo Disabled)
if [ "$selinux" != Enforcing ]; then
  [ "$FORCE" = true ] || die "SELinux is $selinux; the engine runtime needs it enforcing (setenforce 1, and SELINUX=enforcing in /etc/selinux/config)"
  echo "warning: SELinux is $selinux, not enforcing; continuing (--force-unsupported)"
fi
if [ "$selinux" != Disabled ]; then
  if semanage fcontext -l -C | grep -qF "$store = /var/lib/containers"; then
    echo "$store is labelled like /var/lib/containers"
  else
    semanage fcontext -a -e /var/lib/containers "$store"
    echo "$store: labelled like /var/lib/containers"
  fi
  restorecon -R "$store"
fi

# cgroup delegation: systemd gives user managers only the memory and pids
# controllers; engines' --cpus needs cpu too. For usnmeng's manager only.
# A change restarts that manager, which stops any engine running.
delegate=/etc/systemd/system/user@$ENG_UID.service.d/50-usnm-delegate.conf
restart_user=false
if put_conf "$delegate" $'# Written by scripts/ocr-node/node-setup.sh (ADR-0016).\n[Service]\nDelegate=cpu cpuset io memory pids'; then
  restart_user=true
fi

# /run/usnm-ocr: usnmeng's, setgid usnmocr, so the socket its user manager
# makes there gets the group; usnm can go through it but not write there.
# The scratch tmpfs is mounted inside.
put_conf /etc/tmpfiles.d/usnm-ocr.conf $'# Written by scripts/ocr-node/node-setup.sh (ADR-0016).\nd /run/usnm-ocr 2750 usnmeng usnmocr -' || true
systemd-tmpfiles --create /etc/tmpfiles.d/usnm-ocr.conf
mount_unit=$(systemd-escape --path --suffix=mount "$SCRATCH")
remount=false
# Not ordered before local-fs.target like other local mounts, so it can
# come after systemd-tmpfiles-setup has made /run/usnm-ocr.
if put_conf "/etc/systemd/system/$mount_unit" "# Written by scripts/ocr-node/node-setup.sh (ADR-0016).
[Unit]
Description=usnewsmap OCR engine scratch
DefaultDependencies=no
After=systemd-tmpfiles-setup.service
Conflicts=umount.target
Before=umount.target

[Mount]
What=tmpfs
Where=$SCRATCH
Type=tmpfs
Options=size=$SCRATCH_SIZE,mode=0750,uid=$USNM_UID,gid=$OCR_GID,nosuid,nodev,noexec

[Install]
WantedBy=multi-user.target"; then
  remount=true
fi

# The Podman API service in usnmeng's user manager. The units are in
# /etc/systemd/user, where only root can change them, and run for usnmeng
# alone.
api_changed=false
put_conf /etc/systemd/user/usnm-podman.socket "# Written by scripts/ocr-node/node-setup.sh (ADR-0016).
[Unit]
Description=Podman API socket for the OCR worker
ConditionUser=usnmeng

[Socket]
ListenStream=$SOCK
SocketMode=0660

[Install]
WantedBy=sockets.target" && api_changed=true
put_conf /etc/systemd/user/usnm-podman.service "# Written by scripts/ocr-node/node-setup.sh (ADR-0016).
[Unit]
Description=Podman API service for the OCR worker
Requires=usnm-podman.socket
After=usnm-podman.socket
ConditionUser=usnmeng

[Service]
Type=exec
KillMode=process
Delegate=true
ExecStart=/usr/bin/podman --log-level=info system service" && api_changed=true
systemctl daemon-reload
systemctl --global enable usnm-podman.socket > /dev/null 2>&1
if [ "$remount" = true ] && mountpoint -q "$SCRATCH"; then
  # Only the size and owners change; a remount keeps the files. The
  # security flags are repeated so the remount can't drop them.
  mount -o "remount,size=$SCRATCH_SIZE,mode=0750,uid=$USNM_UID,gid=$OCR_GID,nosuid,nodev,noexec" "$SCRATCH"
fi
systemctl enable --now "$mount_unit" > /dev/null
loginctl enable-linger usnmeng
if [ "$restart_user" = true ] && systemctl is-active --quiet "user@$ENG_UID.service"; then
  systemctl restart "user@$ENG_UID.service"
fi
for _ in $(seq 1 30); do
  systemctl is-active --quiet "user@$ENG_UID.service" && break
  sleep 1
done
systemctl is-active --quiet "user@$ENG_UID.service" || die "usnmeng's user manager (user@$ENG_UID.service) didn't start"
systemctl --user -M usnmeng@ daemon-reload
if [ "$api_changed" = true ]; then
  systemctl --user -M usnmeng@ stop usnm-podman.service > /dev/null 2>&1 || true
  systemctl --user -M usnmeng@ restart usnm-podman.socket
else
  systemctl --user -M usnmeng@ start usnm-podman.socket
fi
[ -S "$SOCK" ] || die "no socket at $SOCK"
[ "$(stat -c %U:%G:%a "$SOCK")" = usnmeng:usnmocr:660 ] ||
  die "$SOCK is $(stat -c %U:%G:%a "$SOCK"), not usnmeng:usnmocr:660"
cg=/sys/fs/cgroup/user.slice/user-$ENG_UID.slice/user@$ENG_UID.service/cgroup.controllers
for c in cpu memory pids; do
  grep -qw "$c" "$cg" || die "the $c controller isn't delegated to usnmeng ($cg: $(cat "$cg"))"
done
# The socket as usnm sees it, with podman's own view of the controllers.
rt=$(mktemp -d /run/usnm-ocr-check.XXXXXX)
chown usnm:usnm "$rt"
chmod 0700 "$rt"
seen=$(cd / && runuser -u usnm -- env HOME=/var/lib/usnm XDG_RUNTIME_DIR="$rt" \
  podman --remote --url "unix://$SOCK" info --format '{{.Host.CgroupControllers}}' 2>&1) || seen="error: $seen"
rm -rf "$rt"
case "$seen" in
  *cpu*memory*pids*) echo "usnm reaches usnmeng's podman at $SOCK; controllers $seen" ;;
  *) die "usnm can't use usnmeng's podman at $SOCK: $seen" ;;
esac
echo "scratch: $(findmnt -no SOURCE,OPTIONS "$SCRATCH")"

# The NFS share of pve-big's tank/ocr that an earlier version mounted here:
# the scans now come over HTTP, so the mount and its fstab line go.
if grep -q "[[:space:]]${OCR_DATA_DIR}[[:space:]]" /etc/fstab; then
  step "Removing the old OCR data mount at $OCR_DATA_DIR"
  grep -v "[[:space:]]${OCR_DATA_DIR}[[:space:]]" /etc/fstab > /etc/fstab.usnm-tmp || true
  cat /etc/fstab.usnm-tmp > /etc/fstab
  rm -f /etc/fstab.usnm-tmp
  systemctl daemon-reload
fi
# Every run, so a cleanup that failed part way is finished later. The
# automount outlives its fstab line until it is stopped.
if [ -d "$OCR_DATA_DIR" ] || mountpoint -q "$OCR_DATA_DIR" 2> /dev/null; then
  systemctl stop "$(systemd-escape --path "$OCR_DATA_DIR").automount" 2> /dev/null || true
  umount -l "$OCR_DATA_DIR/scans" 2> /dev/null || true
  umount -l "$OCR_DATA_DIR" 2> /dev/null || true
  rmdir "$OCR_DATA_DIR" 2> /dev/null || true
  ! mountpoint -q "$OCR_DATA_DIR" 2> /dev/null || die "$OCR_DATA_DIR is still mounted"
fi

step "The worker's settings and unit"
# /etc/usnm/ocr-worker.env holds the worker's settings as environment
# variables: the exchange account's URLs, the node id, the pages in flight
# (one per CPU; the worker gives Tesseract one thread per page) and the
# nodes' Application Insights connection string (--app-insights;
# onboard.sh passes OCR_NODE_APPINSIGHTS). scripts/ocr-node/install-worker.sh
# owns it once it has run (it adds the LoC gaps for the number of nodes
# and the Tesseract image), so this writes it only if install-worker.sh
# hasn't. Local changes go in /etc/usnm/ocr-worker.local.env, read after
# it and never touched here (RUST_LOG, say). It is root's, readable by
# the usnm group: nothing in it is a secret.
install -d -o root -g root -m 0755 /etc/usnm
envfile=/etc/usnm/ocr-worker.env
want_env=$(cat <<ENV
# Written by scripts/ocr-node/node-setup.sh until install-worker.sh runs;
# local changes go in /etc/usnm/ocr-worker.local.env.
USNM_OCR_QUEUE_URL=https://$EXCHANGE.queue.core.windows.net
USNM_OCR_OUT_URL=https://$EXCHANGE.blob.core.windows.net/ocr-out
USNM_OCR_NODE_ID=$NAME
USNM_OCR_CONCURRENCY=$(nproc)
APPLICATIONINSIGHTS_CONNECTION_STRING="$CONN"
ENV
)
[ -n "$CONN" ] || echo "warning: no --app-insights; the worker will log to the journal only"

# The unit (scripts/ocr-node/usnm-ocr-worker.service, which onboard.sh
# copies beside this script). ConditionFileIsExecutable keeps it from
# starting, enabled or not, until the worker binary is installed;
# install-worker.sh --enable enables it.
unit_src="$(dirname "$0")/usnm-ocr-worker.service"
[ -r "$unit_src" ] || die "no $unit_src; onboard.sh copies it beside node-setup.sh"
want_unit=$(cat "$unit_src")
changed=false
# Writes $2 to $1 (owner root, group $3, mode $4) if it differs; a new file
# is put in place with one rename.
put_file() {
  if [ "$(cat "$1" 2> /dev/null)" = "$2" ] && [ "$(stat -c %G:%a "$1")" = "$3:$4" ]; then
    echo "$1: unchanged"
    return 0
  fi
  local tmp
  tmp=$(mktemp "$1.XXXXXX")
  printf '%s\n' "$2" > "$tmp"
  chown "root:$3" "$tmp"
  chmod "$4" "$tmp"
  mv -f "$tmp" "$1"
  echo "$1: written"
  changed=true
}
if grep -q '^# Written by scripts/ocr-node/install-worker.sh' "$envfile" 2> /dev/null; then
  echo "$envfile: written by install-worker.sh; left as it is"
else
  put_file "$envfile" "$want_env" usnm 640
fi
put_file "$unitfile" "$want_unit" root 644
systemctl daemon-reload
# A running worker picks the changes up with a restart; its in-flight pages
# get the grace period and otherwise reappear later.
if [ "$changed" = true ] && systemctl is-active --quiet usnm-ocr-worker.service; then
  systemctl restart usnm-ocr-worker.service
  echo "restarted usnm-ocr-worker.service"
elif [ "$was_active" = true ]; then
  systemctl start usnm-ocr-worker.service
  echo "started usnm-ocr-worker.service again"
fi
if [ -x /usr/local/bin/usnm-ocr-worker ]; then
  echo "usnm-ocr-worker.service: $(systemctl is-enabled usnm-ocr-worker.service 2> /dev/null || true)," \
    "$(systemctl is-active usnm-ocr-worker.service 2> /dev/null || true)"
else
  echo "usnm-ocr-worker.service: waiting for /usr/local/bin/usnm-ocr-worker" \
    "(scripts/ocr-node/install-worker.sh; $(systemctl is-enabled usnm-ocr-worker.service 2> /dev/null || true))"
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
