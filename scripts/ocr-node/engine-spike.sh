#!/usr/bin/env bash
# The engine runtime spike (#307 item 0, 13 §13.11 step 1): checks, on a
# node that scripts/ocr-node/node-setup.sh has set up, every point ADR-0016
# needs before engines run under it, and prints PASS or FAIL for each.
#
#   scripts/ocr-node/engine-spike.sh --host claude@node1.example.org \
#     [--ssh-key ~/.ssh/id_ed25519] [--image REF] [--unit-file FILE]
#   sudo engine-spike.sh [--image REF] [--unit-file FILE]   (on the node)
#
# With --host, it copies itself and the worker's unit to the node over ssh
# and runs there with sudo. On the node, as root, it checks:
#   - SELinux is enforcing;
#   - usnmeng exists with its own subordinate ids, lingering on, outside
#     himds, and shares the group usnmocr with usnm;
#   - the cpu, memory and pids controllers are delegated to its manager;
#   - its Podman API socket is /run/usnm-ocr/podman.sock, usnmeng:usnmocr
#     0660, and the scratch at /run/usnm-ocr/scratch is a size-capped tmpfs.
# Then, as usnm, in a transient unit made from the worker's unit
# (--unit-file, default the one beside this script, else the installed
# one) with the same hardening, NoNewPrivileges included, and the podman
# client's environment cleared as the worker clears it (13 §13.6.1):
#   - `podman --remote` works and the unit really has no_new_privs;
#   - a container's stdout reaches the client with the log driver off;
#   - `podman --remote run --interactive` with the 13 §13.6.1 flags passes
#     JSON lines both ways;
#   - a slot labelled with a fixed MCS pair and an ACL on out/<run_id>/
#     (13 §13.3): the container user reads in/ and writes out/<run_id>/ but
#     not in/, its files land on a subordinate id, and usnm reads and
#     deletes them; a container at another MCS level can't read the slot;
#   - the effective memory.max (with no swap), pids.max and cpu.max over
#     the container's cgroup and its ancestors, and what's above its scope;
#   - an instance killed for memory shows OOMKilled before `podman rm`;
#   - writes to the scratch tmpfs and to the container's own /tmp count in
#     its memory.current, and scratch writes can't pass its memory limit.
# The containers run the image --image names (default: the Tesseract
# image's Debian base, pulled by digest through the socket if usnmeng
# doesn't have it); it needs sh and coreutils. Everything the spike makes
# is removed at the end. Exits 1 if any check fails. Post the output on
# #296.
# pass and fail always succeed, so `test && pass || fail` is safe; the
# single-quoted scripts run in containers or on the node.
# shellcheck disable=SC2015,SC2016,SC2029
set -euo pipefail
usage() { sed -n '5,7s/^# \{0,1\}//p' "$0" >&2; exit 2; }
die() { echo "error: $*" >&2; exit 1; }

DEFAULT_IMAGE=mirror.gcr.io/library/debian:trixie-slim@sha256:918311b7b6c4c6f68b232ba516584925f6c78ad82b6fd534b98979df6438e483
SOCK=/run/usnm-ocr/podman.sock
SCRATCH=/run/usnm-ocr/scratch

HOST="" SSH_KEY="" IMAGE="" UNIT_FILE="" INNER=false
while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST=${2:-}; shift 2 ;;
    --ssh-key) SSH_KEY=${2:-}; shift 2 ;;
    --image) IMAGE=${2:-}; shift 2 ;;
    --unit-file) UNIT_FILE=${2:-}; shift 2 ;;
    --inner) INNER=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[ -n "$IMAGE" ] || IMAGE=$DEFAULT_IMAGE
[[ $IMAGE =~ ^[a-z0-9][a-z0-9./:@_-]*$ ]] || die "--image: an image name or id"

# From the operator's machine: copy this script and the unit over, run
# there. Works with macOS's bash 3.2; what follows this block runs on the
# node only.
if [ -n "$HOST" ]; then
  unit=${UNIT_FILE:-$(dirname "$0")/usnm-ocr-worker.service}
  [ -r "$unit" ] || die "no $unit"
  [ -z "$SSH_KEY" ] || [ -r "$SSH_KEY" ] || die "--ssh-key: can't read $SSH_KEY"
  SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20)
  [ -z "$SSH_KEY" ] || SSH_OPTS+=(-i "$SSH_KEY" -o IdentitiesOnly=yes)
  remote=$(ssh "${SSH_OPTS[@]}" "$HOST" 'mktemp -d /tmp/usnm-spike.XXXXXX') || die "can't reach $HOST over ssh"
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/engine-spike.sh" < "$0"
  ssh "${SSH_OPTS[@]}" "$HOST" "cat > $remote/usnm-ocr-worker.service" < "$unit"
  rc=0
  ssh "${SSH_OPTS[@]}" "$HOST" "sudo bash $remote/engine-spike.sh --image '$IMAGE' \
    --unit-file $remote/usnm-ocr-worker.service; rc=\$?; rm -rf $remote; exit \$rc" || rc=$?
  exit "$rc"
fi

FAILS=0
pass() { echo "PASS  $1${2:+: $2}"; }
fail() { echo "FAIL  $1${2:+: $2}"; FAILS=$((FAILS + 1)); }
info() { echo "      $*"; }

# ---------------------------------------------------------------------------
# Inside the transient unit, as usnm.
if [ "$INNER" = true ]; then
  # The podman client as the worker runs it: a cleared environment with
  # only PATH, LANG, HOME and XDG_RUNTIME_DIR, through the socket.
  pod() {
    env -i PATH="$PATH" LANG=C.UTF-8 HOME="$HOME" XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
      podman --remote --url "unix://$SOCK" "$@"
  }
  RUN_ID=r$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')
  NAME=usnm-spike-$RUN_ID
  LEVEL=s0:c1000,c1001
  OTHER_LEVEL=s0:c1020,c1021
  SLOT=$SCRATCH/spike-$RUN_ID
  # shellcheck disable=SC2329 # the EXIT trap
  cleanup_inner() {
    pod container rm --force --ignore --time 0 \
      "$NAME-out" "$NAME-json" "$NAME-rw" "$NAME-mcs" "$NAME-lim" "$NAME-chg" \
      "$NAME-tmp" "$NAME-big" "$NAME-oom" > /dev/null 2>&1 || true
    rm -rf "$SLOT"
  }
  trap cleanup_inner EXIT
  # A write to a container that has gone fails instead of killing the spike.
  trap '' PIPE

  # The 13 §13.6.1 flags; each container adds its name, level, limits,
  # volumes, entrypoint and command.
  # shellcheck disable=SC2054 # the commas are --tmpfs's options
  common=(--pull=never --label usnm.spike=1 --network=none --http-proxy=false --no-hosts
    --read-only --tmpfs /tmp:rw,nosuid,nodev,size=128m,mode=1777 --user 10001:10001
    --cap-drop=all --security-opt=no-new-privileges --cpus=1 --pids-limit=64 --shm-size=64m
    --ulimit nofile=1024:1024 --ulimit core=0 --log-driver=none --stop-timeout=10
    --env HOME=/tmp)
  slot=(--volume "$SLOT/in:/in:ro" --volume "$SLOT/out:/out:rw")
  mem() { echo "--memory=$1" "--memory-swap=$1"; }

  # The smallest of a cgroup file over $1 and its ancestors up to its
  # libpod scope ("max" if none is set), and the files above the scope.
  CG_EFFECTIVE="" CG_ABOVE=""
  cg_read() {
    local dir=/sys/fs/cgroup$1 file=$2 best=max v below=true
    CG_ABOVE=""
    while [ "$dir" != /sys/fs/cgroup ] && [ -n "$dir" ]; do
      if [ -r "$dir/$file" ]; then
        v=$(cut -d' ' -f1 < "$dir/$file")
        if [ "$below" = true ]; then
          if [ "$v" != max ] && { [ "$best" = max ] || [ "$v" -lt "$best" ]; }; then best=$v; fi
        else
          CG_ABOVE="$CG_ABOVE ${dir#/sys/fs/cgroup}=$v"
        fi
      fi
      case "${dir##*/}" in libpod-*.scope) below=false ;; esac
      dir=${dir%/*}
    done
    CG_EFFECTIVE=$best
  }
  cgroup_of() { pod container inspect --format '{{.State.CgroupPath}}' "$1" 2> /dev/null; }
  # memory.current of the container's scope (its whole cgroup tree).
  scope_current() {
    local dir=/sys/fs/cgroup$1
    while [ "$dir" != /sys/fs/cgroup ]; do
      case "${dir##*/}" in libpod-*.scope) cat "$dir/memory.current"; return ;; esac
      dir=${dir%/*}
    done
    cat "/sys/fs/cgroup$1/memory.current"
  }
  wait_size() { # file bytes seconds
    local i
    for ((i = 0; i < $3 * 2; i++)); do
      [ "$(stat -c %s "$1" 2> /dev/null || echo 0)" -ge "$2" ] && return 0
      sleep 0.5
    done
    return 1
  }

  echo "== As $(id -un) in the hardened unit"
  nnp=$(awk '/^NoNewPrivs:/ { print $2 }' /proc/self/status)
  [ "$nnp" = 1 ] && pass "no_new_privs is set in the unit" || fail "no_new_privs is set in the unit" "NoNewPrivs: $nnp"
  caps=$(awk '/^CapBnd:/ { print $2 }' /proc/self/status)
  [ "$caps" = 0000000000000000 ] && pass "empty capability bounding set" || fail "empty capability bounding set" "CapBnd: $caps"

  if ctl=$(pod info --format '{{.Host.CgroupControllers}}' 2>&1); then
    case "$ctl" in
      *cpu*memory*pids*) pass "podman --remote info as usnm" "controllers $ctl" ;;
      *) fail "cgroup controllers seen by podman" "$ctl" ;;
    esac
  else
    fail "podman --remote info as usnm" "$ctl"
    exit 1
  fi
  sel=$(pod info --format '{{.Host.Security.SELinuxEnabled}}' 2>&1 || true)
  [ "$sel" = true ] && pass "the Podman service sees SELinux" || fail "the Podman service sees SELinux" "$sel"
  # The host uid the image's user 10001 maps to.
  X=$(pod info --format '{{range .Host.IDMappings.UIDMap}}{{.ContainerID}} {{.HostID}} {{.Size}}{{"\n"}}{{end}}' |
    awk '$1 <= 10001 && 10001 < $1 + $3 { print $2 + 10001 - $1 }' || true)
  # And the uid the runtime itself runs as (container root: usnmeng).
  R=$(pod info --format '{{range .Host.IDMappings.UIDMap}}{{.ContainerID}} {{.HostID}} {{.Size}}{{"\n"}}{{end}}' |
    awk '$1 == 0 { print $2 }' || true)
  ENG_UID=$(id -u usnmeng)
  if [ -n "$X" ] && [ "$X" != "$ENG_UID" ]; then
    pass "container user 10001 maps to a subordinate id" "$X (usnmeng is $ENG_UID)"
  else
    fail "container user 10001 maps to a subordinate id" "${X:-none}"
    exit 1
  fi

  # The slot (13 §13.3): in/ and out/, 0700 like the worker's, labelled
  # with the slot's MCS pair by usnm, which owns them, with ACL entries
  # for X (read in/, go through out/) and search-only for the runtime
  # (R), which mounts them; out/<run_id>/ 0700 with an ACL for X.
  if mkdir -m 0700 "$SLOT" "$SLOT/in" "$SLOT/out" &&
    { [ -z "$R" ] || setfacl -m "u:$R:x" "$SLOT"; } &&
    setfacl -m "u:$X:rx${R:+,u:$R:x}" "$SLOT/in" &&
    setfacl -m "u:$X:x${R:+,u:$R:x}" "$SLOT/out" &&
    chcon -t container_file_t -l "$LEVEL" "$SLOT/in" "$SLOT/out" &&
    printf 'spike %s\n' "$RUN_ID" > "$SLOT/in/page.txt" &&
    mkdir -m 0700 "$SLOT/out/$RUN_ID" &&
    setfacl -m "u:$X:rwx,d:u:$X:rwx,d:u:$(id -u):rwx" "$SLOT/out/$RUN_ID"; then
    pass "usnm labels the slot $LEVEL and sets the ACL" "$(stat -c %C "$SLOT/in")"
  else
    fail "usnm labels the slot $LEVEL and sets the ACL"
    exit 1
  fi

  # A one-shot container's stdout with the log driver off (the pilot's
  # per-page Tesseract path reads `tesseract --version` this way).
  # shellcheck disable=SC2046 # mem's words
  out=$(pod run --name "$NAME-out" "${common[@]}" $(mem 256m) --security-opt "label=level:$LEVEL" \
    "${slot[@]}" --entrypoint cat "$IMAGE" /in/page.txt < /dev/null 2>&1) || true
  pod container rm --force --ignore "$NAME-out" > /dev/null 2>&1 || true
  [ "$out" = "spike $RUN_ID" ] && pass "stdout with --log-driver=none; the container reads in/" ||
    fail "stdout with --log-driver=none; the container reads in/" "$out"

  # JSON lines both ways through `podman --remote run --interactive`, one
  # request at a time, as the engine host will talk to an adapter.
  # Through two FIFOs, not a coproc, so this file still parses in macOS's
  # bash 3.2 for --host.
  pipes=$(mktemp -d)
  errf=$pipes/err
  mkfifo "$pipes/in" "$pipes/out"
  # shellcheck disable=SC2046 # mem's words
  pod run --interactive --init --name "$NAME-json" "${common[@]}" $(mem 256m) \
    --security-opt "label=level:$LEVEL" "${slot[@]}" --entrypoint sh "$IMAGE" \
    -c 'while IFS= read -r l; do printf "%s\n" "$l"; done' < "$pipes/in" > "$pipes/out" 2> "$errf" &
  eng_pid=$!
  exec 7> "$pipes/in" 8< "$pipes/out"
  ok=true got=""
  for msg in '{"type":"hello","contract":1}' '{"type":"page","run_id":"'"$RUN_ID"'"}' '{"type":"page","n":3}'; do
    printf '%s\n' "$msg" >&7 || { ok=false; break; }
    if IFS= read -r -t 60 reply <&8; then
      got="$got $reply"
      [ "$reply" = "$msg" ] || ok=false
    else
      ok=false
      break
    fi
  done
  # EOF on its stdin ends the loop, and the container.
  exec 7>&-
  status=0
  wait "$eng_pid" || status=$?
  exec 8<&-
  pod container rm --force --ignore "$NAME-json" > /dev/null 2>&1 || true
  if [ "$ok" = true ] && [ "$status" -eq 0 ]; then
    pass "JSON lines both ways over run --interactive, log driver off, --init" "3 round trips, exit 0 on EOF"
  else
    fail "JSON lines both ways over run --interactive" "got [${got# }], exit $status, $(tail -c 300 "$errf")"
  fi
  rm -rf "$pipes"

  # Writes through the ACL; in/ is read-only.
  # shellcheck disable=SC2046 # mem's words
  out=$(pod run --name "$NAME-rw" "${common[@]}" $(mem 256m) --security-opt "label=level:$LEVEL" \
    "${slot[@]}" --entrypoint sh "$IMAGE" -c '
      printf "{\"ok\":true}\n" > "/out/$1/result.json" || exit 3
      if touch /in/x 2> /dev/null; then exit 4; fi
      id -u' sh "$RUN_ID" < /dev/null 2>&1) || true
  pod container rm --force --ignore "$NAME-rw" > /dev/null 2>&1 || true
  f=$SLOT/out/$RUN_ID/result.json
  if [ "$out" = 10001 ] && [ -f "$f" ]; then
    pass "the container user writes out/<run_id>/ through the ACL, not in/"
    owner=$(stat -c %u "$f")
    [ "$owner" = "$X" ] && pass "its files belong to the subordinate id" "$owner" ||
      fail "its files belong to the subordinate id" "$owner, not $X"
    if [ "$(cat "$f")" = '{"ok":true}' ] && rm -f "$f" && rmdir "$SLOT/out/$RUN_ID"; then
      pass "usnm reads and deletes them"
    else
      fail "usnm reads and deletes them"
    fi
  else
    fail "the container user writes out/<run_id>/ through the ACL, not in/" "$out"
  fi

  # A container at another level can't read the slot.
  # shellcheck disable=SC2046 # mem's words
  out=$(pod run --name "$NAME-mcs" "${common[@]}" $(mem 256m) --security-opt "label=level:$OTHER_LEVEL" \
    "${slot[@]}" --entrypoint cat "$IMAGE" /in/page.txt < /dev/null 2>&1) || true
  pod container rm --force --ignore "$NAME-mcs" > /dev/null 2>&1 || true
  case "$out" in
    *"spike $RUN_ID"*) fail "a container at $OTHER_LEVEL can't read the slot" "it read it" ;;
    *) pass "a container at $OTHER_LEVEL can't read the slot" "$(printf '%s' "$out" | tail -n1)" ;;
  esac

  mkdir -p "$SLOT/out/$RUN_ID"
  chmod 0700 "$SLOT/out/$RUN_ID"
  setfacl -m "u:$X:rwx,d:u:$X:rwx,d:u:$(id -u):rwx" "$SLOT/out/$RUN_ID"

  # Effective limits.
  # shellcheck disable=SC2046 # mem's words
  if pod run --detach --name "$NAME-lim" "${common[@]}" $(mem 256m) --entrypoint sleep "$IMAGE" 120 > /dev/null 2>&1; then
    cg=$(cgroup_of "$NAME-lim" || true)
    info "CgroupPath $cg"
    cg_read "$cg" memory.max
    [ "$CG_EFFECTIVE" = 268435456 ] && pass "effective memory.max" "$CG_EFFECTIVE" ||
      fail "effective memory.max" "$CG_EFFECTIVE, not 268435456"
    info "memory.max above the scope:$CG_ABOVE"
    for v in $CG_ABOVE; do [ "${v#*=}" = max ] || info "warning: ${v%=*} caps memory at ${v#*=}"; done
    cg_read "$cg" memory.swap.max
    [ "$CG_EFFECTIVE" = 0 ] && pass "no swap (memory.swap.max)" "0" ||
      fail "no swap (memory.swap.max)" "$CG_EFFECTIVE"
    cg_read "$cg" pids.max
    [ "$CG_EFFECTIVE" = 64 ] && pass "effective pids.max" "64" || fail "effective pids.max" "$CG_EFFECTIVE, not 64"
    info "pids.max above the scope:$CG_ABOVE"
    cpu=""
    dir=/sys/fs/cgroup$cg
    while [ "$dir" != /sys/fs/cgroup ]; do
      v=$(cat "$dir/cpu.max" 2> /dev/null || true)
      case "$v" in max*|"") ;; *) cpu=$v; break ;; esac
      dir=${dir%/*}
    done
    [ "$cpu" = "100000 100000" ] && pass "cpu.max (--cpus=1)" "$cpu" || fail "cpu.max (--cpus=1)" "${cpu:-none}"
  else
    fail "a detached container with limits starts"
  fi
  pod container rm --force --ignore --time 0 "$NAME-lim" > /dev/null 2>&1 || true

  # tmpfs writes count against the container: 64 MiB to the scratch, then
  # 64 MiB to its own /tmp.
  for where in out tmp; do
    if [ "$where" = out ]; then target=/out/$RUN_ID/blob hostf=$SLOT/out/$RUN_ID/blob; else target=/tmp/blob hostf=""; fi
    # shellcheck disable=SC2046 # mem's words
    pod run --detach --name "$NAME-chg" "${common[@]}" $(mem 256m) --security-opt "label=level:$LEVEL" \
      "${slot[@]}" --entrypoint sh "$IMAGE" -c 'head -c 67108864 /dev/zero > "$1" && touch /tmp/done && sleep 60' \
      sh "$target" > /dev/null 2>&1 || true
    if [ -n "$hostf" ]; then wait_size "$hostf" 67108864 30 || true; else sleep 5; fi
    cg=$(cgroup_of "$NAME-chg" || true)
    cur=$([ -n "$cg" ] && scope_current "$cg" || echo 0)
    if [ "$cur" -ge 67108864 ]; then
      pass "writes to ${where/out/the scratch tmpfs} count in the container's memory" "memory.current $((cur / 1048576)) MiB after 64 MiB"
    else
      fail "writes to ${where/out/the scratch tmpfs} count in the container's memory" "memory.current $((cur / 1048576)) MiB after 64 MiB"
    fi
    pod container rm --force --ignore --time 0 "$NAME-chg" > /dev/null 2>&1 || true
    [ -z "$hostf" ] || rm -f "$hostf"
  done

  # The scratch can't take more than the container's memory limit.
  # shellcheck disable=SC2046 # mem's words
  pod run --name "$NAME-big" "${common[@]}" $(mem 128m) --security-opt "label=level:$LEVEL" \
    "${slot[@]}" --entrypoint sh "$IMAGE" -c 'head -c 268435456 /dev/zero > "/out/$1/big"' sh "$RUN_ID" \
    < /dev/null > /dev/null 2>&1 || true
  oom=$(pod container inspect --format '{{.State.OOMKilled}} exit {{.State.ExitCode}}' "$NAME-big" 2> /dev/null || true)
  pod container rm --force --ignore "$NAME-big" > /dev/null 2>&1 || true
  size=$(stat -c %s "$SLOT/out/$RUN_ID/big" 2> /dev/null || echo 0)
  rm -f "$SLOT/out/$RUN_ID/big"
  if [ "$size" -lt 268435456 ]; then
    pass "256 MiB to the scratch under a 128 MiB limit is stopped" "$((size / 1048576)) MiB written; OOMKilled $oom"
  else
    fail "256 MiB to the scratch under a 128 MiB limit is stopped" "all of it was written"
  fi

  # An instance killed for memory: OOMKilled, read before podman rm.
  # shellcheck disable=SC2046 # mem's words
  pod run --name "$NAME-oom" "${common[@]}" $(mem 64m) --entrypoint tail "$IMAGE" /dev/zero \
    < /dev/null > /dev/null 2>&1 || true
  oom=$(pod container inspect --format '{{.State.OOMKilled}} {{.State.ExitCode}}' "$NAME-oom" 2>&1 || true)
  pod container rm --force --ignore "$NAME-oom" > /dev/null 2>&1 || true
  case "$oom" in
    "true "*) pass "OOMKilled read before podman rm" "OOMKilled, exit ${oom#true }" ;;
    *) fail "OOMKilled read before podman rm" "$oom" ;;
  esac
  [ "$FAILS" -eq 0 ]
  exit
fi

# ---------------------------------------------------------------------------
# On the node, as root.
[ "$(id -u)" -eq 0 ] || die "run as root (sudo), or from your machine with --host"
[ -n "$UNIT_FILE" ] || UNIT_FILE=$(dirname "$0")/usnm-ocr-worker.service
[ -r "$UNIT_FILE" ] || UNIT_FILE=/etc/systemd/system/usnm-ocr-worker.service
[ -r "$UNIT_FILE" ] || die "no worker unit to copy the hardening from"

# shellcheck source=/dev/null
echo "$(. /etc/os-release && echo "$PRETTY_NAME"), kernel $(uname -r), $(podman --version)," \
  "$(rpm -q crun container-selinux systemd | paste -sd' ')"
echo "unit: $UNIT_FILE; image: $IMAGE"
echo
echo "== Node setup"
mode=$(getenforce 2> /dev/null || echo unknown)
[ "$mode" = Enforcing ] && pass "SELinux enforcing" || fail "SELinux enforcing" "$mode"
if id usnmeng > /dev/null 2>&1; then
  ENG_UID=$(id -u usnmeng)
  pass "usnmeng exists" "uid $ENG_UID"
  groups=" $(id -nG usnmeng) "
  [[ $groups != *" himds "* ]] && pass "usnmeng isn't in himds" || fail "usnmeng isn't in himds" "$groups"
  [[ $groups == *" usnmocr "* ]] && [[ " $(id -nG usnm) " == *" usnmocr "* ]] &&
    pass "usnm and usnmeng share usnmocr" || fail "usnm and usnmeng share usnmocr"
  for f in /etc/subuid /etc/subgid; do
    r=$(awk -F: '$1 == "usnmeng" { print $2 ":" $3 }' "$f")
    [ -n "$r" ] && [ "${r#*:}" -ge 65536 ] && pass "usnmeng has subordinate ids (${f#/etc/})" "$r" ||
      fail "usnmeng has subordinate ids (${f#/etc/})" "${r:-none}"
  done
  linger=$(loginctl show-user usnmeng -p Linger --value 2> /dev/null || true)
  [ "$linger" = yes ] && pass "lingering on for usnmeng" || fail "lingering on for usnmeng" "${linger:-no manager}"
  cg=/sys/fs/cgroup/user.slice/user-$ENG_UID.slice/user@$ENG_UID.service/cgroup.subtree_control
  ctl=$(cat "$cg" 2> /dev/null || true)
  [[ " $ctl " == *" cpu "* && " $ctl " == *" memory "* && " $ctl " == *" pids "* ]] &&
    pass "cgroup delegation to usnmeng's manager" "$ctl" || fail "cgroup delegation to usnmeng's manager" "${ctl:-none}"
else
  fail "usnmeng exists" "run scripts/ocr-node/node-setup.sh (onboard.sh) first"
  exit 1
fi
if [ -S "$SOCK" ]; then
  s=$(stat -c %U:%G:%a "$SOCK")
  [ "$s" = usnmeng:usnmocr:660 ] && pass "the Podman API socket" "$SOCK $s" || fail "the Podman API socket" "$SOCK $s"
else
  fail "the Podman API socket" "no $SOCK"
  exit 1
fi
m=$(findmnt -no FSTYPE,OPTIONS "$SCRATCH" 2> /dev/null || true)
o=$(stat -c %U:%G:%a "$SCRATCH" 2> /dev/null || true)
[[ $m == tmpfs*size=* ]] && [ "$o" = usnm:usnmocr:750 ] && pass "the scratch tmpfs" "$o ${m#tmpfs }" ||
  fail "the scratch tmpfs" "${m:-not mounted} $o"

# The image, through the socket as root.
if ! HOME=/root podman --remote --url "unix://$SOCK" image exists "$IMAGE"; then
  echo "pulling $IMAGE as usnmeng"
  HOME=/root podman --remote --url "unix://$SOCK" pull --quiet "$IMAGE" > /dev/null || die "can't pull $IMAGE"
fi

# The transient unit: the worker's, with the spike as its command, its own
# runtime directory and no environment file.
WORK=$(mktemp -d /run/usnm-ocr-spike.XXXXXX)
chmod 0755 "$WORK"
SPIKE_UNIT=usnm-ocr-spike.service
cleanup() {
  systemctl stop "$SPIKE_UNIT" > /dev/null 2>&1 || true
  systemctl reset-failed "$SPIKE_UNIT" > /dev/null 2>&1 || true
  rm -f "/run/systemd/system/$SPIKE_UNIT"
  systemctl daemon-reload || true
  rm -rf "$WORK"
}
trap cleanup EXIT
install -m 0644 "$0" "$WORK/engine-spike.sh"
sed -e '/^\[Install\]/,$d' \
  -e '/^\(ExecStart\|Restart\|RestartSec\|ConditionFileIsExecutable\|EnvironmentFile\|Type\|KillMode\|SyslogIdentifier\|SyslogFacility\)=/d' \
  -e 's/^RuntimeDirectory=usnm-ocr-worker$/RuntimeDirectory=usnm-ocr-spike/' \
  -e 's|^Environment=XDG_RUNTIME_DIR=/run/usnm-ocr-worker$|Environment=XDG_RUNTIME_DIR=/run/usnm-ocr-spike|' \
  "$UNIT_FILE" > "/run/systemd/system/$SPIKE_UNIT"
cat >> "/run/systemd/system/$SPIKE_UNIT" << UNIT
[Service]
Type=oneshot
TimeoutStartSec=600
ExecStart=/bin/bash $WORK/engine-spike.sh --inner --image $IMAGE
StandardOutput=file:$WORK/inner.log
StandardError=inherit
UNIT
grep -qx 'NoNewPrivileges=yes' "/run/systemd/system/$SPIKE_UNIT" || fail "the worker unit sets NoNewPrivileges=yes" "$UNIT_FILE"
systemctl daemon-reload
echo
inner_ok=true
systemctl start "$SPIKE_UNIT" || inner_ok=false
cat "$WORK/inner.log" 2> /dev/null || true
n=$(grep -c '^FAIL' "$WORK/inner.log" 2> /dev/null || true)
FAILS=$((FAILS + ${n:-0}))
if [ "$inner_ok" = false ] && ! grep -q '^FAIL' "$WORK/inner.log" 2> /dev/null; then
  fail "the hardened unit ran" "$(journalctl -u "$SPIKE_UNIT" -n 10 --no-pager 2> /dev/null | tail -n 5)"
fi
echo
if [ "$FAILS" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$FAILS check(s) failed"
  exit 1
fi
