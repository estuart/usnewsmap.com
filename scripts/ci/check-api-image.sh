#!/usr/bin/env bash
# The API image (--image) starts on its baked fixtures and serves both the API and
# the site: the app shell with its security headers, a precompressed hashed
# asset, robots.txt and the sitemap, a 404 for an unknown path, and /v1.
set -euo pipefail
usage() { echo "usage: scripts/ci/check-api-image.sh --image IMAGE" >&2; exit 2; }
# The positional form (`check-api-image.sh IMAGE`) works for one
# release, with a warning (#277).
if [ $# -eq 1 ] && [[ $1 != -* ]]; then
  echo "warning: \`scripts/ci/check-api-image.sh IMAGE\` is deprecated; use --image (#277)" >&2
  set -- --image "$1"
fi
image=""
while [ $# -gt 0 ]; do
  case $1 in
    --image) [ $# -ge 2 ] || usage; image=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$image" ] || usage
id=$(docker run -d -p 127.0.0.1:18080:8080 "$image")
trap 'docker logs "$id" | tail -n 50; docker rm -f "$id" > /dev/null' EXIT
base=http://127.0.0.1:18080
for _ in $(seq 1 30); do
  curl -sf "$base/readyz" > /dev/null && break
  sleep 1
done
curl -fsS "$base/v1/meta" | grep -q '"index_version"'
headers=$(curl -fsS -D - -o /dev/null "$base/")
grep -qi '^content-type: text/html' <<< "$headers"
grep -qi "^content-security-policy: .*connect-src 'self'" <<< "$headers"
asset=$(curl -fsS "$base/" | grep -o '/assets/index-[^"]*\.js' | head -n 1)
[ -n "$asset" ] || { echo "no script in the app shell"; exit 1; }
curl -fsS -D - -o /dev/null -H 'accept-encoding: br' "$base$asset" | grep -qi '^content-encoding: br'
curl -fsS -D - -o /dev/null "$base/robots.txt" | grep -qi '^content-type: text/plain'
curl -fsS "$base/sitemap.xml" | grep -q '<loc>https://usnewsmap.com/</loc>'
test "$(curl -sS -o /dev/null -w '%{http_code}' "$base/does-not-exist")" = 404
echo "api image serves /v1 and the site ($asset)"
