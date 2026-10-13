#!/usr/bin/env bash
# The BOINC server image (--image, contrib-compute/images/server) has the stock daemons PR #441
# runs, the project tools, the wrapper, Apache with PHP, and can make an upload key pair. It does
# not start the project: that needs MySQL, Key Vault and a share (UNVERIFIED, docs/images.md).
set -euo pipefail
usage() { echo "usage: scripts/ci/check-boinc-server-image.sh --image IMAGE" >&2; exit 2; }
image=""
while [ $# -gt 0 ]; do
  case $1 in
    --image) [ $# -ge 2 ] || usage; image=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$image" ] || usage
docker run --rm --entrypoint bash "$image" -c '
set -e
cd /usr/local/boinc
for f in sched/feeder sched/transitioner sched/file_deleter sched/db_purge sched/sample_trivial_validator \
         sched/batch_collect_assimilator sched/cgi sched/file_upload_handler tools/create_work \
         tools/dir_hier_path tools/sign_executable lib/crypt_prog; do
  [ -x "$f" ] || { echo "missing $f" >&2; exit 1; }
done
for f in tools/make_project tools/xadd tools/update_versions tools/upgrade; do
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done
[ -x /usr/local/lib/usnm-boinc/docker_wrapper ] || { echo "missing docker_wrapper" >&2; exit 1; }
[ -x /usr/local/bin/bridge-job ] || { echo "missing bridge-job" >&2; exit 1; }
php -m | grep -qx mysqli
apache2ctl -M 2>/dev/null | grep -q php
python3 -c "import MySQLdb"
# The signing tool must be here; the code-signing PRIVATE key must not be.
if find / -xdev -name code_sign_private 2>/dev/null | grep -q .; then echo "code_sign_private is in the image" >&2; exit 1; fi
echo "tools ok"'
out=$(mktemp -d)
chmod 777 "$out"
docker run --rm -v "$out:/out" "$image" --make-upload-keys --dir /out > /dev/null
[ -s "$out/upload_private" ] && [ -s "$out/upload_public" ] || { echo "make-upload-keys wrote no keys" >&2; exit 1; }
echo "upload keys ok"
