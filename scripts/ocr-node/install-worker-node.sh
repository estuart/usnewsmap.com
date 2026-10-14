#!/usr/bin/env bash
# The node's half of scripts/ocr-node/install-worker.sh (#296, 12 Step 2).
# install-worker.sh copies it to the node with the verified worker binary,
# the environment file and the unit, and runs it as root. Not meant to be
# run by hand.
#
#   install-worker-node.sh --release TAG --dir DIR --binary-sha256 HEX \
#     --image-id sha256:ID --image-url URL --image-sha256 HEX [--enable | --update [--stay-up-secs N]] \
#     [--auto-update-delay HOURS [--auto-update-leader URL] | --auto-update-off]
#   install-worker-node.sh --rollback [--release TAG]
#   install-worker-node.sh --facts
#
# Install: checks the binary's sha256, copies it to
# /usr/local/lib/usnm-ocr-worker/TAG/, downloads the Tesseract image archive
# straight from the release to the node (it never passes through the
# operator's machine), checks its sha256 and loads it into the engine
# account's podman storage (usnmeng's, through the Podman API socket the
# worker uses, ADR-0016) unless that image id is there already, writes
# /etc/usnm/ocr-worker.env (root:usnm, 0640) and the unit, points
# /usr/local/bin/usnm-ocr-worker at the new binary, and restarts the unit
# if it was running. --enable first reads a rendered line with the engine
# as usnm (`usnm-ocr-worker check-engine`), then enables and starts the
# unit, and disables it again if it doesn't stay up.
# --rollback points the link back at the release installed before the
# current one that isn't rejected (or at --release), with its Tesseract
# image, and adds the
# release it leaves to /var/lib/usnm-ocr-update/rejected, so the nightly
# update doesn't install it again.
# --update is the nightly update's install (update-worker.sh, #471): it
# reads a rendered line with the new binary and image (check-engine)
# before switching, and exits 4 without switching if that fails; after
# switching it restarts the worker and, if the unit isn't active with no
# restarts --stay-up-secs later (default 300), rolls back to the release
# before and exits 3. Either way the release is rejected. After a
# successful update it removes all but the three newest installed releases
# and the Tesseract images only the removed ones used.
# --auto-update-delay HOURS turns the nightly update on (usnm-ocr-update.timer,
# 03:15 US Eastern): it installs update-worker.sh (from DIR) as
# /usr/local/sbin/usnm-ocr-update-worker and this script as
# /usr/local/libexec/usnm-ocr/install-worker-node.sh, with gh and jq from
# dnf. A release reaches the node HOURS after it is published at the
# earliest; --auto-update-leader URL makes it wait for that node's worker
# too. --auto-update-off turns it off. Without either, a node keeps its
# timer as it is, and the two scripts are refreshed if it has them.
# Installs and rollbacks take /run/lock/usnm-ocr-maintenance.lock (waiting
# up to 30 minutes), which the reboot window and the nightly update share.
# --facts prints what's installed, read-only.
set -euo pipefail
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

LIB=/usr/local/lib/usnm-ocr-worker
LINK=/usr/local/bin/usnm-ocr-worker
ENVFILE=/etc/usnm/ocr-worker.env
UNIT=/etc/systemd/system/usnm-ocr-worker.service
USNM_HOME=/var/lib/usnm
# The engine runtime node-setup.sh sets up, as in the unit.
SOCK=/run/usnm-ocr/podman.sock
SCRATCH=/run/usnm-ocr/scratch

RELEASE="" DIR="" BIN_SUM="" IMAGE_ID="" IMAGE_URL="" IMAGE_SUM="" ENABLE=false ROLLBACK=false FACTS=false
UPDATE=false STAY_UP=300 AUTO_DELAY="" AUTO_LEADER="" AUTO_OFF=false
while [ $# -gt 0 ]; do
  case "$1" in
    --release) RELEASE=${2:-}; shift 2 ;;
    --dir) DIR=${2:-}; shift 2 ;;
    --binary-sha256) BIN_SUM=${2:-}; shift 2 ;;
    --image-id) IMAGE_ID=${2:-}; shift 2 ;;
    --image-url) IMAGE_URL=${2:-}; shift 2 ;;
    --image-sha256) IMAGE_SUM=${2:-}; shift 2 ;;
    --enable) ENABLE=true; shift ;;
    --update) UPDATE=true; shift ;;
    --stay-up-secs) STAY_UP=${2:-}; shift 2 ;;
    --auto-update-delay) AUTO_DELAY=${2:-}; shift 2 ;;
    --auto-update-leader) AUTO_LEADER=${2:-}; shift 2 ;;
    --auto-update-off) AUTO_OFF=true; shift ;;
    --rollback) ROLLBACK=true; shift ;;
    --facts) FACTS=true; shift ;;
    *) die "unknown option $1" ;;
  esac
done
[ -z "$RELEASE" ] || [[ $RELEASE =~ ^ocr-worker-[0-9]{8}-[0-9a-f]{7}$ ]] || die "--release: ocr-worker-<yyyymmdd>-<sha7>"
[ "$ENABLE" = false ] || [ "$UPDATE" = false ] || die "--enable and --update don't go together"
[[ $STAY_UP =~ ^[1-9][0-9]{0,4}$ ]] || die "--stay-up-secs: 1 to 99999"
[ -z "$AUTO_DELAY" ] || [[ $AUTO_DELAY =~ ^[0-9]{1,3}$ ]] || die "--auto-update-delay: hours, 0 to 999"
[ -z "$AUTO_LEADER" ] || [ -n "$AUTO_DELAY" ] || die "--auto-update-leader needs --auto-update-delay"
[ -z "$AUTO_LEADER" ] || [[ $AUTO_LEADER =~ ^https?://[A-Za-z0-9.-]+(:[0-9]+)?/[A-Za-z0-9/_-]*$ ]] ||
  die "--auto-update-leader: http://HOST:PORT/status"
[ "$AUTO_OFF" = false ] || [ -z "$AUTO_DELAY" ] || die "--auto-update-delay and --auto-update-off don't go together"

# Commands as usnm, in its home. usnm is a system user with no login
# session, so it has no runtime directory of its own (/run/user/<uid>);
# the podman client wants one, as the unit's RuntimeDirectory gives it.
# This run makes its own, removed at exit.
RUNTIME=""
cleanup() { [ -z "$RUNTIME" ] || rm -rf "$RUNTIME"; }
trap cleanup EXIT
as_usnm() {
  if [ -z "$RUNTIME" ]; then
    RUNTIME=$(mktemp -d /run/usnm-ocr-install.XXXXXX)
    chown usnm:usnm "$RUNTIME"
    chmod 0700 "$RUNTIME"
  fi
  (cd "$USNM_HOME" && runuser -u usnm -- env HOME="$USNM_HOME" XDG_RUNTIME_DIR="$RUNTIME" "$@")
}

# Podman as the worker runs it: usnm's client, usnmeng's podman.
engine_podman() { as_usnm podman --remote --url "unix://$SOCK" "$@"; }

current_release() {
  local target
  target=$(readlink "$LINK" 2> /dev/null || true)
  case "$target" in
    "$LIB"/*/usnm-ocr-worker) target=${target#"$LIB"/}; echo "${target%/usnm-ocr-worker}" ;;
  esac
}

# Installed releases, newest install first. Their names are release tags.
installed() {
  # shellcheck disable=SC2010,SC2012 # the names are release tags: letters, digits, hyphens
  ls -1t "$LIB" 2> /dev/null | grep -E '^ocr-worker-[0-9]{8}-[0-9a-f]{7}$' || true
}

unit_state() {
  echo "$(systemctl is-enabled usnm-ocr-worker.service 2> /dev/null || true)," \
    "$(systemctl is-active usnm-ocr-worker.service 2> /dev/null || true)"
}

if [ "$FACTS" = true ]; then
  echo "nproc=$(nproc)"
  echo "current=$(current_release)"
  # Newest first, by when each was installed.
  echo "installed=$(installed | tr '\n' ' ')"
  echo "unit=$(unit_state | tr -d ' ')"
  echo "podman=$(command -v podman || true)"
  echo "usnm=$(id -u usnm 2> /dev/null || true)"
  echo "engine_socket=$([ -S "$SOCK" ] && echo yes || true)"
  exit 0
fi

# One install, rollback or update at a time, and none during the reboot
# window. The nightly update takes the lock itself and says so.
if [ "${USNM_OCR_MAINTENANCE_LOCKED:-}" != 1 ]; then
  mkdir -p /run/lock
  exec 9> /run/lock/usnm-ocr-maintenance.lock
  if ! flock -n 9; then
    echo "waiting for /run/lock/usnm-ocr-maintenance.lock (the nightly update or the reboot window)"
    flock -w 1800 9 || die "/run/lock/usnm-ocr-maintenance.lock is still held after 30 minutes"
  fi
fi

REJECTED=/var/lib/usnm-ocr-update/rejected
reject() {
  [ -n "$1" ] || return 0
  mkdir -p "$(dirname "$REJECTED")"
  grep -qxF "$1" "$REJECTED" 2> /dev/null || echo "$1" >> "$REJECTED"
  echo "$1 added to $REJECTED"
}

# Points the link at $1's binary in one rename.
switch_to() {
  ln -sfn "$LIB/$1/usnm-ocr-worker" "$LINK.new"
  mv -Tf "$LINK.new" "$LINK"
  echo "$LINK -> $LIB/$1/usnm-ocr-worker"
}

restart_if_active() {
  if systemctl is-active --quiet usnm-ocr-worker.service; then
    systemctl restart usnm-ocr-worker.service
    echo "restarted usnm-ocr-worker.service"
  fi
  echo "usnm-ocr-worker.service: $(unit_state)"
}

# Points the link and the image setting back at $1, an installed release.
rollback_to() {
  local image
  [ -x "$LIB/$1/usnm-ocr-worker" ] || die "$1 isn't installed in $LIB"
  image=$(cat "$LIB/$1/tesseract.image-id")
  engine_podman image exists "$image" || die "$1's Tesseract image $image isn't in usnmeng's podman storage"
  step "Rolling back from $(current_release) to $1"
  sed -i "s|^USNM_OCR_TESSERACT_IMAGE=.*|USNM_OCR_TESSERACT_IMAGE=$image|" "$ENVFILE"
  grep -q "^USNM_OCR_TESSERACT_IMAGE=$image\$" "$ENVFILE" || die "$ENVFILE has no USNM_OCR_TESSERACT_IMAGE line"
  switch_to "$1"
  "$LINK" --version
}

if [ "$ROLLBACK" = true ]; then
  current=$(current_release)
  target=$RELEASE
  if [ -z "$target" ]; then
    for r in $(installed); do
      [ "$r" != "$current" ] || continue
      # Never back to a release that failed here: name it with --release.
      ! grep -qxF "$r" "$REJECTED" 2> /dev/null || continue
      target=$r
      break
    done
  fi
  [ -n "$target" ] || die "no other release is installed in $LIB"
  [ "$target" != "$current" ] || die "$target is already the current release"
  rollback_to "$target"
  reject "$current"
  restart_if_active
  exit 0
fi

[ -n "$RELEASE" ] && [ -n "$DIR" ] && [ -n "$BIN_SUM" ] && [ -n "$IMAGE_ID" ] && [ -n "$IMAGE_URL" ] && [ -n "$IMAGE_SUM" ] ||
  die "install needs --release, --dir, --binary-sha256, --image-id, --image-url and --image-sha256"
[[ $IMAGE_ID =~ ^sha256:[0-9a-f]{64}$ ]] || die "--image-id: sha256:<64 hex digits>"
[[ $BIN_SUM =~ ^[0-9a-f]{64}$ ]] && [[ $IMAGE_SUM =~ ^[0-9a-f]{64}$ ]] || die "sha256 values are 64 hex digits"
[[ $IMAGE_URL =~ ^https://github\.com/tgoodyear/usnewsmap\.com/releases/download/$RELEASE/usnm-ocr-tesseract\.oci\.tar\.gz$ ]] ||
  die "--image-url must be the release's usnm-ocr-tesseract.oci.tar.gz"
id usnm > /dev/null 2>&1 || die "no usnm user; run scripts/ocr-node/onboard.sh first"
command -v podman > /dev/null || die "no podman; run scripts/ocr-node/onboard.sh first"
[ -S "$SOCK" ] || die "no engine runtime ($SOCK); run scripts/ocr-node/onboard.sh first"
for f in usnm-ocr-worker ocr-worker.env usnm-ocr-worker.service; do
  [ -s "$DIR/$f" ] || die "$DIR/$f is missing"
done

step "The worker binary ($RELEASE)"
echo "$BIN_SUM  $DIR/usnm-ocr-worker" | sha256sum --check --quiet || die "the binary's sha256 differs from the release's"
install -d -o root -g root -m 0755 "$LIB" "$LIB/$RELEASE"
install -o root -g root -m 0755 "$DIR/usnm-ocr-worker" "$LIB/$RELEASE/usnm-ocr-worker"
# Under SELinux a file in /usr/local/lib is lib_t, and systemd runs a lib_t
# program in init_t rather than unconfined_service_t; there the worker can't
# reach the engine's Podman socket or the Arc agent's port (dontaudit, so no
# AVC is logged). Every release's binary is labelled bin_t, as a program in
# /usr/local/bin would be, and the rule survives a relabel.
if command -v selinuxenabled > /dev/null && selinuxenabled; then
  # semanage takes the policy store's lock, which another package's install
  # may hold for a few minutes (on a new node, the monitoring agent's
  # collector loads a policy module): wait for it, up to five minutes.
  semanage_wait() {
    local out try
    for try in $(seq 0 30); do
      if out=$(semanage "$@" 2>&1); then
        printf '%s\n' "$out"
        return 0
      fi
      grep -q "transaction lock" <<< "$out" || break
      [ "$try" -lt 30 ] || break
      echo "the SELinux policy store is busy; waiting" >&2
      sleep 10
    done
    printf '%s\n' "$out" >&2
    return 1
  }
  rule="$LIB/[^/]+/usnm-ocr-worker"
  rules=$(semanage_wait fcontext -l -C)
  if ! grep -qF "$rule" <<< "$rules"; then
    semanage_wait fcontext -a -t bin_t "$rule" > /dev/null
  fi
  # Every installed release, so a later --rollback runs bin_t too.
  restorecon -R "$LIB"
  ls -Z "$LIB/$RELEASE/usnm-ocr-worker"
fi
printf '%s\n' "$IMAGE_ID" > "$LIB/$RELEASE/tesseract.image-id"
chmod 0644 "$LIB/$RELEASE/tesseract.image-id"
"$LIB/$RELEASE/usnm-ocr-worker" --version

step "The Tesseract image ($IMAGE_ID)"
if engine_podman image exists "$IMAGE_ID"; then
  echo "already in usnmeng's podman storage"
else
  # Downloaded by the node itself: a few hundred MB that needn't cross the
  # operator's connection. Its sha256 comes from the release's attested
  # SHA256SUMS, which install-worker.sh verified.
  tmp=$(mktemp -d /var/tmp/usnm-ocr-image.XXXXXX)
  trap 'rm -rf "$tmp"; cleanup' EXIT
  chown usnm:usnm "$tmp"
  curl -fsSL --retry 3 --proto '=https' -o "$tmp/image.tar.gz" "$IMAGE_URL"
  echo "$IMAGE_SUM  $tmp/image.tar.gz" | sha256sum --check --quiet || die "the image archive's sha256 differs from the release's"
  chown usnm:usnm "$tmp/image.tar.gz"
  engine_podman load --quiet --input "$tmp/image.tar.gz"
  engine_podman image exists "$IMAGE_ID" || die "the archive didn't load as $IMAGE_ID"
  rm -rf "$tmp"
  echo "loaded"
fi

if [ "$ENABLE" = true ] || [ "$UPDATE" = true ]; then
  step "Checking the engine"
  # As usnm with the new binary and image, through the engine runtime as
  # the unit sets it, reading a rendered line, before anything switches.
  if ! as_usnm env USNM_OCR_TESSERACT_IMAGE="$IMAGE_ID" USNM_OCR_PODMAN_URL="unix://$SOCK" \
    USNM_OCR_SCRATCH_DIR="$SCRATCH" "$LIB/$RELEASE/usnm-ocr-worker" check-engine; then
    if [ "$UPDATE" = true ]; then
      reject "$RELEASE"
      echo "error: check-engine failed with $RELEASE; nothing switched" >&2
      exit 4
    fi
    die "check-engine failed with $RELEASE; nothing switched"
  fi
fi

step "Settings and unit"
previous=$(current_release)
install -d -o root -g root -m 0755 /etc/usnm
install -o root -g usnm -m 0640 "$DIR/ocr-worker.env" "$ENVFILE.new"
mv -f "$ENVFILE.new" "$ENVFILE"
echo "$ENVFILE: written"
# The status port for the LAN dashboard (USNM_OCR_STATUS_LISTEN), from the
# dashboard's address only (USNM_OCR_STATUS_ALLOW): an nftables table of
# its own, loaded by a oneshot unit at boot. Without the settings, both go.
status_listen=$(sed -n 's/^USNM_OCR_STATUS_LISTEN=//p' "$ENVFILE")
status_allow=$(sed -n 's/^USNM_OCR_STATUS_ALLOW=//p' "$ENVFILE")
if [ -n "$status_listen" ]; then
  port=${status_listen##*:}
  [[ $port =~ ^[0-9]+$ ]] || die "USNM_OCR_STATUS_LISTEN: <address>:<port>"
  [[ $status_allow =~ ^[A-Za-z0-9.-]+(,[A-Za-z0-9.-]+)*$ ]] || die "USNM_OCR_STATUS_ALLOW: host names or IPv4 addresses, comma-separated"
  # Names to addresses for the rule (the LAN's DNS); the worker resolves
  # them again each time its status server starts.
  addrs=""
  for h in ${status_allow//,/ }; do
    for a in $(getent ahostsv4 "$h" | awk '{print $1}' | sort -u); do addrs+="${addrs:+, }$a"; done
  done
  [ -n "$addrs" ] || die "USNM_OCR_STATUS_ALLOW: $status_allow doesn't resolve"
  status_allow=$addrs
  install -d -m 0755 /etc/nftables
  cat > /etc/nftables/usnm-ocr-status.nft <<NFT
# Written by scripts/ocr-node/install-worker-node.sh: the worker's status
# port only from the LAN dashboard.
table inet usnm_ocr_status
delete table inet usnm_ocr_status
table inet usnm_ocr_status {
  chain input {
    type filter hook input priority 0; policy accept;
    tcp dport $port ip saddr != { $status_allow } drop
    tcp dport $port meta nfproto ipv6 drop
  }
}
NFT
  cat > /etc/systemd/system/usnm-ocr-status-firewall.service <<'FW'
# Written by scripts/ocr-node/install-worker-node.sh: loads the status
# port's nftables rule before the worker starts.
[Unit]
Description=usnewsmap OCR worker status port firewall
Before=usnm-ocr-worker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/sbin/nft -f /etc/nftables/usnm-ocr-status.nft

[Install]
WantedBy=multi-user.target
FW
  systemctl daemon-reload
  systemctl enable usnm-ocr-status-firewall.service > /dev/null 2>&1
  systemctl restart usnm-ocr-status-firewall.service
  echo "status port $port: only from $status_allow"
elif [ -e /etc/systemd/system/usnm-ocr-status-firewall.service ]; then
  systemctl disable --now usnm-ocr-status-firewall.service > /dev/null 2>&1 || true
  rm -f /etc/systemd/system/usnm-ocr-status-firewall.service /etc/nftables/usnm-ocr-status.nft
  nft delete table inet usnm_ocr_status 2> /dev/null || true
  echo "status port: off"
fi
if ! cmp -s "$DIR/usnm-ocr-worker.service" "$UNIT"; then
  install -o root -g root -m 0644 "$DIR/usnm-ocr-worker.service" "$UNIT.new"
  mv -f "$UNIT.new" "$UNIT"
  echo "$UNIT: written"
fi
systemctl daemon-reload
switch_to "$RELEASE"
systemd-analyze verify "$UNIT"
echo "systemd-analyze verify: ok"

if [ "$ENABLE" = true ]; then
  step "Enabling the unit"
  # Enabled or not, running or not: (re)start it on this release and check
  # that it stays up. The worker checks its engine at start and exits on
  # failure, before it takes a message; give it time to do that.
  ok=true
  systemctl enable usnm-ocr-worker.service || ok=false
  [ "$ok" = false ] || systemctl restart usnm-ocr-worker.service || ok=false
  [ "$ok" = false ] || sleep 20
  if [ "$ok" = true ] && systemctl is-active --quiet usnm-ocr-worker.service; then
    echo "usnm-ocr-worker.service: $(unit_state)"
  else
    journalctl -u usnm-ocr-worker.service -n 20 --no-pager || true
    systemctl disable --now usnm-ocr-worker.service || true
    die "the unit didn't stay up, so it is disabled again (see the journal above)"
  fi
elif [ "$UPDATE" = true ]; then
  step "Restarting the worker on $RELEASE"
  # Active, with no automatic restart (Restart=on-failure after a crash),
  # for the whole wait. An explicit restart sets NRestarts back to 0.
  ok=true
  systemctl restart usnm-ocr-worker.service || ok=false
  waited=0
  while [ "$ok" = true ] && [ "$waited" -lt "$STAY_UP" ]; do
    sleep 10
    waited=$((waited + 10))
    if ! systemctl is-active --quiet usnm-ocr-worker.service ||
      [ "$(systemctl show -P NRestarts usnm-ocr-worker.service)" != 0 ]; then
      ok=false
    fi
  done
  if [ "$ok" = true ]; then
    echo "usnm-ocr-worker.service stayed up for $STAY_UP s on $RELEASE"
    # Unattended updates keep the three newest installs (this one and two
    # to roll back to), and the Tesseract images only those use.
    keep=$(installed | head -n 3)
    for r in $(installed | tail -n +4); do
      [ "$r" != "$RELEASE" ] && [ "$r" != "$previous" ] || continue
      image=$(cat "$LIB/$r/tesseract.image-id" 2> /dev/null || true)
      rm -rf "${LIB:?}/$r"
      echo "removed $r"
      [ -n "$image" ] || continue
      used=false
      for k in $keep; do
        [ "$(cat "$LIB/$k/tesseract.image-id" 2> /dev/null || true)" != "$image" ] || used=true
      done
      [ "$used" = true ] || engine_podman rmi "$image" > /dev/null 2>&1 || true
    done
  else
    journalctl -u usnm-ocr-worker.service -n 20 --no-pager || true
    reject "$RELEASE"
    [ -n "$previous" ] && [ "$previous" != "$RELEASE" ] ||
      die "the unit didn't stay up on $RELEASE, and there is no release to roll back to"
    rollback_to "$previous"
    systemctl restart usnm-ocr-worker.service || true
    echo "error: the unit didn't stay up on $RELEASE; rolled back to $previous" >&2
    exit 3
  fi
else
  restart_if_active
fi

# The nightly update (#471).
UPDATER=/usr/local/sbin/usnm-ocr-update-worker
LIBEXEC=/usr/local/libexec/usnm-ocr
UPDATE_UNIT=/etc/systemd/system/usnm-ocr-update.service
UPDATE_TIMER=/etc/systemd/system/usnm-ocr-update.timer
# The scripts come from the operator's checkout, by install-worker.sh; the
# nightly update's own install (--update) brings none.
refresh_scripts() {
  [ -s "$DIR/update-worker.sh" ] && [ -s "$DIR/install-worker-node.sh" ] || return 0
  install -d -o root -g root -m 0755 "$LIBEXEC"
  install -o root -g root -m 0755 "$DIR/update-worker.sh" "$UPDATER"
  install -o root -g root -m 0755 "$DIR/install-worker-node.sh" "$LIBEXEC/install-worker-node.sh"
  ! command -v restorecon > /dev/null || restorecon "$UPDATER" "$LIBEXEC/install-worker-node.sh"
  echo "$UPDATER and $LIBEXEC/install-worker-node.sh: written"
}
if [ "$AUTO_OFF" = true ]; then
  step "Nightly update: off"
  systemctl disable --now usnm-ocr-update.timer > /dev/null 2>&1 || true
  rm -f "$UPDATE_TIMER" "$UPDATE_UNIT"
  systemctl daemon-reload
elif [ -n "$AUTO_DELAY" ]; then
  step "Nightly update"
  [ -s "$DIR/update-worker.sh" ] && [ -s "$DIR/install-worker-node.sh" ] ||
    die "$DIR lacks update-worker.sh or install-worker-node.sh"
  # gh verifies the attestations (never signed in); jq reads the API.
  if ! command -v gh > /dev/null || ! command -v jq > /dev/null; then
    # The GitHub CLI's own repository, as https://cli.github.com/packages/rpm/gh-cli.repo
    # gives it; dnf-automatic keeps gh up to date from there.
    [ -e /etc/yum.repos.d/gh-cli.repo ] || cat > /etc/yum.repos.d/gh-cli.repo <<'REPO'
[gh-cli]
name=packages for the GitHub CLI
baseurl=https://cli.github.com/packages/rpm
enabled=1
gpgcheck=1
gpgkey=https://cli.github.com/packages/githubcli-archive-keyring.asc
REPO
    dnf install -y gh jq
  fi
  refresh_scripts
  leader_args=""
  [ -z "$AUTO_LEADER" ] || leader_args=" --leader-status $AUTO_LEADER"
  cat > "$UPDATE_UNIT.new" <<UNIT_EOF
# Written by scripts/ocr-node/install-worker-node.sh (#471).
[Unit]
Description=Nightly update of the OCR worker to the newest attested release
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=$UPDATER --delay-hours $AUTO_DELAY$leader_args
SyslogIdentifier=usnm-ocr-update
TimeoutStartSec=2h
UNIT_EOF
  mv -f "$UPDATE_UNIT.new" "$UPDATE_UNIT"
  cat > "$UPDATE_TIMER.new" <<'TIMER_EOF'
# Written by scripts/ocr-node/install-worker-node.sh (#471).
[Unit]
Description=Nightly update of the OCR worker

[Timer]
# After dnf-automatic's run (06:00 UTC plus up to an hour) and before the
# weekly reboot window (04:00), which waits for it.
OnCalendar=*-*-* 03:15:00 America/New_York

[Install]
WantedBy=timers.target
TIMER_EOF
  mv -f "$UPDATE_TIMER.new" "$UPDATE_TIMER"
  systemctl daemon-reload
  systemd-analyze verify "$UPDATE_UNIT" "$UPDATE_TIMER"
  systemctl enable usnm-ocr-update.timer > /dev/null 2>&1
  systemctl restart usnm-ocr-update.timer
  echo "usnm-ocr-update.timer: 03:15 US Eastern, releases $AUTO_DELAY h after publication${AUTO_LEADER:+, once $AUTO_LEADER runs them}; next $(systemctl show -P NextElapseUSecRealtime usnm-ocr-update.timer)"
elif [ -e "$UPDATE_TIMER" ]; then
  refresh_scripts
fi
