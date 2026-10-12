#!/usr/bin/env bash
# The node's half of scripts/ocr-node/install-worker.sh (#296, 12 Step 2).
# install-worker.sh copies it to the node with the verified worker binary,
# the environment file and the unit, and runs it as root. Not meant to be
# run by hand.
#
#   install-worker-node.sh --release TAG --dir DIR --binary-sha256 HEX \
#     --image-id sha256:ID --image-url URL --image-sha256 HEX [--enable]
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
# current one (or at --release), with its Tesseract image.
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
while [ $# -gt 0 ]; do
  case "$1" in
    --release) RELEASE=${2:-}; shift 2 ;;
    --dir) DIR=${2:-}; shift 2 ;;
    --binary-sha256) BIN_SUM=${2:-}; shift 2 ;;
    --image-id) IMAGE_ID=${2:-}; shift 2 ;;
    --image-url) IMAGE_URL=${2:-}; shift 2 ;;
    --image-sha256) IMAGE_SUM=${2:-}; shift 2 ;;
    --enable) ENABLE=true; shift ;;
    --rollback) ROLLBACK=true; shift ;;
    --facts) FACTS=true; shift ;;
    *) die "unknown option $1" ;;
  esac
done
[ -z "$RELEASE" ] || [[ $RELEASE =~ ^ocr-worker-[0-9]{8}-[0-9a-f]{7}$ ]] || die "--release: ocr-worker-<yyyymmdd>-<sha7>"

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

if [ "$ROLLBACK" = true ]; then
  current=$(current_release)
  target=$RELEASE
  if [ -z "$target" ]; then
    for r in $(installed); do
      [ "$r" != "$current" ] || continue
      target=$r
      break
    done
  fi
  [ -n "$target" ] || die "no other release is installed in $LIB"
  [ "$target" != "$current" ] || die "$target is already the current release"
  [ -x "$LIB/$target/usnm-ocr-worker" ] || die "$target isn't installed in $LIB"
  image=$(cat "$LIB/$target/tesseract.image-id")
  engine_podman image exists "$image" || die "$target's Tesseract image $image isn't in usnmeng's podman storage"
  step "Rolling back from ${current:-nothing} to $target"
  sed -i "s|^USNM_OCR_TESSERACT_IMAGE=.*|USNM_OCR_TESSERACT_IMAGE=$image|" "$ENVFILE"
  grep -q "^USNM_OCR_TESSERACT_IMAGE=$image\$" "$ENVFILE" || die "$ENVFILE has no USNM_OCR_TESSERACT_IMAGE line"
  switch_to "$target"
  "$LINK" --version
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
  rule="$LIB/[^/]+/usnm-ocr-worker"
  if ! semanage fcontext -l -C | grep -qF "$rule"; then
    semanage fcontext -a -t bin_t "$rule"
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

step "Settings and unit"
install -d -o root -g root -m 0755 /etc/usnm
install -o root -g usnm -m 0640 "$DIR/ocr-worker.env" "$ENVFILE.new"
mv -f "$ENVFILE.new" "$ENVFILE"
echo "$ENVFILE: written"
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
  step "Checking the engine before enabling the unit"
  # As usnm with the release's image, through the engine runtime as the
  # unit sets it, reading a rendered line.
  as_usnm env USNM_OCR_TESSERACT_IMAGE="$IMAGE_ID" USNM_OCR_PODMAN_URL="unix://$SOCK" \
    USNM_OCR_SCRATCH_DIR="$SCRATCH" "$LINK" check-engine
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
else
  restart_if_active
fi
