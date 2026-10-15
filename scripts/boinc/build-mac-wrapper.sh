#!/usr/bin/env bash
# Build both Mac wrappers from the same upstream commit as the server image.
set -euo pipefail
usage() { echo 'usage: build-mac-wrapper.sh --out DIR' >&2; exit 2; }
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) [ $# -ge 2 ] || usage; out=$2; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$out" ] || usage
root=$(cd "$(dirname "$0")/../.." && pwd)
commit=$(sed -n 's/^ARG BOINC_COMMIT=//p' "$root/contrib-compute/images/base/Containerfile")
[[ $commit =~ ^[0-9a-f]{40}$ ]] || { echo 'invalid BOINC source pin' >&2; exit 1; }
mkdir "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git -C "$work" init -q
git -C "$work" remote add origin https://github.com/BOINC/boinc.git
git -C "$work" fetch --depth 1 origin "$commit"
git -C "$work" checkout -q --detach FETCH_HEAD
[ "$(git -C "$work" rev-parse HEAD)" = "$commit" ]
sdk=$(xcrun --sdk macosx --show-sdk-path)
deployment=$(plutil -extract SupportedTargets.macosx.MinimumDeploymentTarget raw -n "$sdk/SDKSettings.plist")
printf 'MACOSX_DEPLOYMENT_TARGET = %s\n' "$deployment" > "$work/mac_build/BOINC.xcconfig"
xcodebuild -project "$work/mac_build/boinc.xcodeproj" -target libboinc -target api_libboinc \
  -configuration Deployment 'ARCHS=arm64 x86_64' ONLY_ACTIVE_ARCH=NO CODE_SIGNING_ALLOWED=NO
MACOSX_DEPLOYMENT_TARGET="$deployment" make -C "$work/samples/docker_wrapper" -f Makefile_mac -j 2
for arch in arm64 x86_64; do
  wrapper="$work/samples/docker_wrapper/docker_wrapper_$arch"
  lipo "$wrapper" -verify_arch "$arch"
  cp "$wrapper" "$out/docker_wrapper_$arch"
done
printf '%s\n' "$commit" > "$out/BOINC_COMMIT"
(cd "$out" && shasum -a 256 docker_wrapper_arm64 docker_wrapper_x86_64 > SHA256SUMS)
