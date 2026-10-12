#!/usr/bin/env bash
# The weekly reboot window of an on-prem OCR node (#296). node-setup.sh
# installs this as /usr/local/sbin/usnm-ocr-reboot-if-needed and runs it
# from usnm-ocr-reboot.service, which usnm-ocr-reboot.timer starts once a
# week on the node's own day (node-setup.sh --reboot-day).
#
#   usnm-ocr-reboot-if-needed [--drain-timeout SECS] [--dnf-wait SECS] [--dry-run] [--resume]
#
# dnf-automatic installs every update daily but never reboots
# (node-setup.sh sets reboot = never). This reboots only when
# `dnf needs-restarting -r` says an update needs it (the kernel, glibc,
# systemd and the like). It waits for a dnf-automatic run still going (up
# to --dnf-wait, default 1800 s; after that it skips this week), then stops
# usnm-ocr-worker.service, and usnm-sset.service (the on-prem Serial Set
# fetch, #229) if it's running, and waits for them to exit, up to
# --drain-timeout (default 1260 s: the 1200 s page timeout and a minute),
# then reboots. Both are enabled and start at boot by themselves (the
# Serial Set fetch until it has finished: its unit has a completion
# marker); the fetch resumes from its raw store and takes over its own
# lock at once. A stopped unit that isn't enabled (an older install of
# the fetch's unit) is listed in $RESUME, and
# usnm-ocr-reboot-resume.service runs this with --resume at boot, which
# starts it again.
#
# On SIGTERM the worker stops taking pages, gives those in flight 60 s (its
# --shutdown-grace-secs) to finish, then abandons the rest without output;
# they reappear on the queue when their visibility runs out (30 minutes
# after they were taken) for either node, at the cost of one of their five
# receives. The unit's TimeoutStopSec (120 s) kills anything left, so
# `systemctl stop` normally returns within two minutes; --drain-timeout is
# the outer bound. Everything goes to the journal under usnm-ocr-reboot;
# a reboot and any problem are logged at Warning, which the Azure Monitor
# Agent collects from the node's syslog, the rest at Notice.
# --dry-run says what it would do and changes nothing.
set -euo pipefail
usage() { sed -n '7s/^# \{0,1\}//p' "$0" >&2; exit 2; }
# Under systemd, a <N> prefix sets a line's syslog priority.
pri() { if [ -n "${JOURNAL_STREAM:-}" ]; then printf '<%s>%s\n' "$1" "$2"; else printf '%s\n' "$2"; fi; }
log() { pri 5 "$*"; }
warn() { pri 4 "$*"; }

DRAIN_TIMEOUT=1260 DNF_WAIT=1800 DRY_RUN=false RESUME_MODE=false
while [ $# -gt 0 ]; do
  case "$1" in
    --drain-timeout) DRAIN_TIMEOUT=${2:-}; shift 2 ;;
    --dnf-wait) DNF_WAIT=${2:-}; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    --resume) RESUME_MODE=true; shift ;;
    -h|--help) usage ;;
    *) echo "error: unknown option $1" >&2; usage ;;
  esac
done
[[ $DRAIN_TIMEOUT =~ ^[1-9][0-9]*$ ]] || { echo "error: --drain-timeout: seconds" >&2; usage; }
[[ $DNF_WAIT =~ ^[0-9]+$ ]] || { echo "error: --dnf-wait: seconds" >&2; usage; }
[ "$(id -u)" -eq 0 ] || { echo "error: run as root" >&2; exit 1; }

# Stopped before a reboot when running: the OCR worker, and the Serial Set
# fetch where a node runs it.
UNITS="usnm-ocr-worker.service usnm-sset.service"
RESUME=/var/lib/usnm-ocr-reboot/resume

if [ "$RESUME_MODE" = true ]; then
  [ -s "$RESUME" ] || { log "nothing to start again"; exit 0; }
  failed=false
  while read -r unit; do
    case " $UNITS " in *" $unit "*) ;; *) warn "warning: $unit isn't one of $UNITS; not started"; continue ;; esac
    if [ "$DRY_RUN" = true ]; then
      log "dry run: would start $unit"
    else
      # --no-block: a unit that waits on the network doesn't hold up boot.
      if systemctl start --no-block "$unit"; then
        log "started $unit again"
      else
        warn "warning: couldn't start $unit"
        failed=true
      fi
    fi
  done < "$RESUME"
  # Kept when a start was refused, so the next boot (or a rerun) tries again.
  if [ "$failed" = true ]; then
    warn "warning: $RESUME kept; start what's listed there by hand"
    exit 1
  fi
  [ "$DRY_RUN" = true ] || rm -f "$RESUME"
  exit 0
fi

# Exit status 1 means a reboot is needed, 0 not; anything else is a failure.
status=0
why=$(dnf -q needs-restarting -r 2>&1) || status=$?
case "$status" in
  0) log "no reboot needed"; exit 0 ;;
  1) warn "reboot needed: $(grep '^ *\*' <<< "$why" | tr -d ' *' | paste -sd, -)" ;;
  *) warn "error: dnf needs-restarting -r failed ($status): $why"; exit 1 ;;
esac

# Never reboot in the middle of a dnf transaction.
waited=0
while systemctl is-active --quiet dnf-automatic.service; do
  if [ "$waited" -ge "$DNF_WAIT" ]; then
    warn "warning: dnf-automatic is still running after $DNF_WAIT s; no reboot this week"
    exit 1
  fi
  [ "$waited" -gt 0 ] || log "waiting for dnf-automatic to finish"
  step=$((DNF_WAIT - waited < 10 ? DNF_WAIT - waited : 10))
  sleep "$step"
  waited=$((waited + step))
done

# Running, or waiting to restart (activating), so a unit between tries
# comes back too.
running=""
for unit in $UNITS; do
  case "$(systemctl is-active "$unit" 2> /dev/null || true)" in
    active|activating|reloading) running="${running:+$running }$unit" ;;
  esac
done
if [ "$DRY_RUN" = true ]; then
  log "dry run: would stop ${running:-nothing} (up to $DRAIN_TIMEOUT s), reboot and start it again"
  exit 0
fi

if [ -n "$running" ]; then
  # Only what won't start at boot by itself needs starting again.
  resume=""
  for unit in $running; do
    systemctl is-enabled --quiet "$unit" 2> /dev/null || resume="${resume:+$resume }$unit"
  done
  if [ -n "$resume" ]; then
    mkdir -p "$(dirname "$RESUME")"
    tr ' ' '\n' <<< "$resume" > "$RESUME"
  fi
  log "stopping $running (the worker's in-flight pages get its grace; up to $DRAIN_TIMEOUT s in all)"
  start=$(date +%s)
  # shellcheck disable=SC2086 # a list of unit names
  timeout "$DRAIN_TIMEOUT" systemctl stop $running ||
    warn "warning: systemctl stop $running returned $? after $(($(date +%s) - start)) s"
  left=false
  for unit in $running; do
    if systemctl is-active --quiet "$unit"; then
      warn "warning: $unit still $(systemctl is-active "$unit" || true) after $DRAIN_TIMEOUT s; rebooting anyway"
      left=true
    fi
  done
  [ "$left" = true ] || log "stopped after $(($(date +%s) - start)) s"
else
  log "none of $UNITS is running"
fi
warn "rebooting"
systemctl reboot
