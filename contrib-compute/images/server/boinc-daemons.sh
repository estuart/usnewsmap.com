#!/usr/bin/env bash
# Run under supervisord as root; the project's own tools run as boincadm. bin/start daemonizes
# the project daemons and returns, so this stays in the foreground, runs the periodic tasks
# and restarts anything that stopped (what the project's cron line would do).
set -euo pipefail
: "${PROJECT_DIR:?}"
proj="$PROJECT_DIR/${PROJECT:-usnmocr}"
run() { su -s /bin/bash boincadm -c "cd '$proj' && $1"; }
until [ -f "$proj/config.xml" ]; do sleep 5; done
run 'bin/start'
while sleep 300; do run 'bin/start --cron' || true; done
