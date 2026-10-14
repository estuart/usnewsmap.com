#!/usr/bin/env bash
# Build, stage, sign and publish one BOINC app version on the live server, in one command:
#
#   scripts/boinc-app-release.sh --env prod --app usnm_ocr --version 1.0 --signing-host HOST --what-if
#   scripts/boinc-app-release.sh --env prod --app usnm_ocr --version 1.0 --signing-host HOST
#
# usnm_surya (#516) and usnm_qwen (#522) have their own worker images and two plan classes, one per version:
#   scripts/boinc-app-release.sh --env prod --app usnm_surya --version 1.0 --plan-class docker_mt \
#     --worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:... --signing-host HOST
#   scripts/boinc-app-release.sh --env prod --app usnm_qwen --version 1.0 --plan-class docker_nvidia_cuda \
#     --worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:... --signing-host HOST
#
# Steps (contrib-compute/docs/app-release.md):
# 1. Worker image. Volunteers pull it from the public GHCR package named, by digest, in
#    images.worker of contrib-compute/infra/boinc/env/<env>.json; .github/workflows/boinc-worker.yml
#    publishes it. This checks that an anonymous pull of that digest works. With images.worker
#    empty it builds the worker in ACR Tasks instead (a build check, cached by input key; ACR
#    has no anonymous pull, so volunteers can't use that copy) and stops.
# 2. Stage. On the signing host, scripts/boinc/install.py runs inside the server image with
#    --network=none and writes the version directory (wrapper, job.toml, a Dockerfile pinned to
#    the worker digest, version.xml).
# 3. Sign. On the signing host, BOINC's sign_executable runs in the server image with
#    --network=none, with the private key mounted read-only. Only the .sig files come back.
#    The key's dataset is passphrase-locked; when it is locked this stops and says how to
#    unlock it. It never unlocks it.
# 4. Publish. ACR Tasks bakes the version directory (staged again from the same inputs, every
#    signature checked against the public key) into usnm-boinc-appver:<app>-<version>;
#    provision-boinc.sh --publish-image deploys the publish job with it, and the job copies the
#    version onto the share and runs bin/update_versions. The digest is then written to
#    images.appver in the settings, to commit: the stack keeps the job on it from then on.
#
# The signing host is reached only over SSH (as root, for zfs and podman), and is named only by
# --signing-host. The server image gets to it with a short-lived ACR pull token. Nothing is
# built, staged or published on this machine: the .sig files and the committed sources are all
# that pass through it. Sources come from the committed HEAD.
#
# --what-if makes the read-only checks (settings, Azure, the worker image, the signing host's
# key status and podman) and prints the plan.
set -euo pipefail
usage() {
  echo "usage: scripts/boinc-app-release.sh --env <prod|dev> --app <usnm_ocr|usnm_assess|usnm_gold|usnm_quality|usnm_surya|usnm_qwen> --version <N.M> --signing-host HOST [--plan-class docker|docker_mt|docker_nvidia_cuda] [--worker-image ghcr.io/tgoodyear/usnm-boinc-worker@sha256:...] [--platform PLATFORM --wrapper FILE --wrapper-sha256 HEX --wrapper-source-pin FILE] [--signing-key-dir DIR] [--signing-dataset DATASET] [--what-if]" >&2
  exit 2
}
ENV_NAME="" APP="" VERSION="" SIGNING_HOST="" PLAN_CLASS=docker WORKER_OVERRIDE="" KEY_DIR=/usbbak/signing/boinc KEY_DATASET=usbbak/signing WHAT_IF=false
PLATFORM=x86_64-pc-linux-gnu WRAPPER="" WRAPPER_SHA="" WRAPPER_PIN=""
while [ $# -gt 0 ]; do
  case $1 in
    --env) [ $# -ge 2 ] || usage; ENV_NAME=$2; shift 2 ;;
    --app) [ $# -ge 2 ] || usage; APP=$2; shift 2 ;;
    --version) [ $# -ge 2 ] || usage; VERSION=$2; shift 2 ;;
    --signing-host) [ $# -ge 2 ] || usage; SIGNING_HOST=$2; shift 2 ;;
    --plan-class) [ $# -ge 2 ] || usage; PLAN_CLASS=$2; shift 2 ;;
    --worker-image) [ $# -ge 2 ] || usage; WORKER_OVERRIDE=$2; shift 2 ;;
    --platform) [ $# -ge 2 ] || usage; PLATFORM=$2; shift 2 ;;
    --wrapper) [ $# -ge 2 ] || usage; WRAPPER=$2; shift 2 ;;
    --wrapper-sha256) [ $# -ge 2 ] || usage; WRAPPER_SHA=$2; shift 2 ;;
    --wrapper-source-pin) [ $# -ge 2 ] || usage; WRAPPER_PIN=$2; shift 2 ;;
    --signing-key-dir) [ $# -ge 2 ] || usage; KEY_DIR=$2; shift 2 ;;
    --signing-dataset) [ $# -ge 2 ] || usage; KEY_DATASET=$2; shift 2 ;;
    --what-if) WHAT_IF=true; shift ;;
    -h | --help) usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done
[ -n "$ENV_NAME" ] && [ -n "$APP" ] && [ -n "$VERSION" ] && [ -n "$SIGNING_HOST" ] || usage
die() { echo "error: $*" >&2; exit 1; }
case "$APP:$PLAN_CLASS" in
  usnm_ocr:docker | usnm_assess:docker | usnm_gold:docker | usnm_quality:docker) ;;
  usnm_surya:docker_mt | usnm_surya:docker_nvidia_cuda | usnm_qwen:docker_mt | usnm_qwen:docker_nvidia_cuda)
    [ -n "$WORKER_OVERRIDE" ] || die "$APP has its own worker image: give --worker-image" ;;
  *) die "--app and --plan-class must be usnm_ocr, usnm_assess, usnm_gold or usnm_quality with docker, or usnm_surya or usnm_qwen with docker_mt or docker_nvidia_cuda (scripts/boinc/install.py)" ;;
esac
[ -z "$WORKER_OVERRIDE" ] || [[ $WORKER_OVERRIDE =~ ^ghcr\.io/tgoodyear/usnm-boinc-worker@sha256:[0-9a-f]{64}$ ]] ||
  die "--worker-image must be ghcr.io/tgoodyear/usnm-boinc-worker@sha256:<64 hex>"
[[ $VERSION =~ ^[1-9][0-9]*\.[0-9]+$ ]] || die "--version must look like 1.0"
[[ $SIGNING_HOST =~ ^[A-Za-z0-9][A-Za-z0-9.-]*$ ]] || die "--signing-host must be a host name"
[[ $KEY_DIR =~ ^/[A-Za-z0-9/._-]+$ ]] || die "--signing-key-dir must be an absolute path"
[[ $KEY_DATASET =~ ^[A-Za-z0-9/._-]+$ ]] || die "--signing-dataset must be a ZFS dataset name"
cd "$(dirname "$0")/.."
case "$PLATFORM" in
  x86_64-pc-linux-gnu) [ -z "$WRAPPER$WRAPPER_SHA$WRAPPER_PIN" ] || die "Linux uses the wrapper built in the pinned server image" ;;
  arm64-apple-darwin|x86_64-apple-darwin)
    [ "$PLAN_CLASS" = docker ] || die "$PLATFORM supports only --plan-class docker"
    [ -f "$WRAPPER" ] && [ -f "$WRAPPER_PIN" ] && [[ $WRAPPER_SHA =~ ^[0-9a-f]{64}$ ]] ||
      die "Mac requires --wrapper, --wrapper-sha256 and --wrapper-source-pin from the reviewed build artifact"
    python3 scripts/boinc/check-mac-wrapper.py --wrapper "$WRAPPER" --platform "$PLATFORM" \
      --sha256 "$WRAPPER_SHA" --source-pin "$WRAPPER_PIN" || die "Mac wrapper verification failed" ;;
  *) die "unsupported --platform" ;;
esac
# shellcheck source=scripts/lib/boinc.sh disable=SC1091
. scripts/lib/boinc.sh
boinc_load_settings
boinc_az_check
[ -z "$WORKER_OVERRIDE" ] || WORKER_IMAGE=$WORKER_OVERRIDE
[ -n "$SERVER_IMAGE" ] || die "no server image in $BOINC_SETTINGS: scripts/build-boinc-images.sh --env $ENV_NAME first"
PUBLIC_KEY=$(jq -r .codeSignPublicKey "$BOINC_SETTINGS")
[ -n "$PUBLIC_KEY" ] || die "no codeSignPublicKey in $BOINC_SETTINGS"
[ -z "$(git status --porcelain -- "$BOINC_SETTINGS")" ] || die "$BOINC_SETTINGS has uncommitted changes: commit them first (this run deploys the stack and writes images.appver)"
# A real run deploys this checkout's provision-boinc.sh and Bicep, not only HEAD: refuse a tree
# that differs from HEAD. --what-if only reads, so it warns.
if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
  $WHAT_IF || die "the working tree differs from HEAD (git status); a release deploys this checkout's Bicep, so commit or clean first"
  echo "warning: the working tree differs from HEAD; a real run refuses this" >&2
fi
sha=$(git rev-parse HEAD)
registry=$(jq -er .registry infra/names/"$ENV_NAME".json) || die "no registry in infra/names/$ENV_NAME.json"
APPVER_REPO=usnm-boinc-appver
APPVER_TAG="$APP-$VERSION"
[ "$PLAN_CLASS" = docker ] || APPVER_TAG="$APPVER_TAG-${PLAN_CLASS//_/-}"
[ "$PLATFORM" = x86_64-pc-linux-gnu ] || APPVER_TAG="$APPVER_TAG-$PLATFORM"
VERSION_SUBDIR="apps/$APP/$VERSION/${PLATFORM}__${PLAN_CLASS}"
ssh_host() { ssh -o BatchMode=yes -o ConnectTimeout=15 "root@$SIGNING_HOST" "$@"; }

# --- 1. Worker image -------------------------------------------------------------------------

# ghcr_public REF: true when an anonymous client can read REF's manifest (what a volunteer's
# Docker does first).
ghcr_public() {
  local ref=$1 repo digest token
  repo=${ref#ghcr.io/}; repo=${repo%@*}; digest=${ref#*@}
  token=$(curl -fsS "https://ghcr.io/token?scope=repository:$repo:pull" | jq -er .token) || return 1
  curl -fsS -H "Authorization: Bearer $token" \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json' \
    "https://ghcr.io/v2/$repo/manifests/$digest" | \
    python3 scripts/boinc/check-worker-platform.py --platform "$PLATFORM"
}

# have REPOSITORY TAG / digest REPOSITORY TAG: as in build-boinc-images.sh.
have() { baz acr repository show --name "$registry" --image "$1:$2" -o none 2> /dev/null; }
digest() {
  local d
  d=$(baz acr repository show --name "$registry" --image "$1:$2" --query digest -o tsv) || die "no $1:$2 in $registry"
  [[ $d =~ ^sha256:[0-9a-f]{64}$ ]] || die "unexpected digest for $1:$2"
  printf '%s' "$d"
}

# build_worker_in_acr: the engine (ocr/engines/tesseract) and the worker (ocr/boinc/Containerfile)
# in ACR Tasks, each keyed on the git hashes of its inputs and built only when the key is new.
build_worker_in_acr() {
  local engine_key worker_key engine_ref inputs
  # The builds upload the working tree but are keyed on HEAD: a tree that differs from HEAD
  # (tracked or untracked) would be cached under a key that doesn't name what was built.
  [ -z "$(git status --porcelain --untracked-files=normal)" ] ||
    die "the working tree differs from HEAD (git status); the worker build check is keyed on HEAD, so commit or clean first"
  engine_key=$(git rev-parse HEAD:ocr/engines/tesseract | cut -c1-16)
  inputs=(HEAD:crates HEAD:Cargo.toml HEAD:Cargo.lock HEAD:.dockerignore HEAD:ocr/boinc/Containerfile)
  # The toolchain picks the compiler of the worker's cargo build, as in build-boinc-images.sh.
  if git rev-parse -q --verify HEAD:rust-toolchain.toml > /dev/null; then inputs+=(HEAD:rust-toolchain.toml); fi
  worker_key=$({ git rev-parse "${inputs[@]}"; echo "$engine_key"; } | git hash-object --stdin | cut -c1-16)
  if have usnm-boinc-tesseract "$engine_key"; then
    echo "usnm-boinc-tesseract:$engine_key is in $registry"
  else
    echo "building usnm-boinc-tesseract:$engine_key in $registry"
    baz acr build --registry "$registry" --image "usnm-boinc-tesseract:$engine_key" --platform linux/amd64 \
      --timeout 7200 --file ocr/engines/tesseract/Containerfile ocr/engines/tesseract || die "the engine image build failed"
  fi
  engine_ref="$registry.azurecr.io/usnm-boinc-tesseract@$(digest usnm-boinc-tesseract "$engine_key")"
  if have usnm-boinc-worker "$worker_key"; then
    echo "usnm-boinc-worker:$worker_key is in $registry"
  else
    echo "building usnm-boinc-worker:$worker_key in $registry (usnm-boinc release build: about 10 minutes)"
    baz acr build --registry "$registry" --image "usnm-boinc-worker:$worker_key" --platform linux/amd64 \
      --timeout 7200 --build-arg "TESSERACT_IMAGE=$engine_ref" --file ocr/boinc/Containerfile . ||
      die "the worker image build failed"
  fi
  echo "worker image builds: $registry.azurecr.io/usnm-boinc-worker@$(digest usnm-boinc-worker "$worker_key") (ACR, not pullable by volunteers)"
}

if [ -z "$WORKER_IMAGE" ] && $WHAT_IF; then
  echo "1. no images.worker in $BOINC_SETTINGS: a real run builds the worker in ACR as a check and stops there (publish it with .github/workflows/boinc-worker.yml)"
elif [ -z "$WORKER_IMAGE" ]; then
  build_worker_in_acr
  die "no images.worker in $BOINC_SETTINGS. Publish the worker to GHCR (gh workflow run boinc-worker.yml --ref main; docs/boinc.md, \"Publish the worker image\"), make the package public once, put its ghcr.io/tgoodyear/usnm-boinc-worker@sha256:... in images.worker, commit, and run this again"
else
  ghcr_public "$WORKER_IMAGE" ||
    die "$WORKER_IMAGE is not anonymously readable or lacks the required native Linux architecture for $PLATFORM"
  echo "1. worker image $WORKER_IMAGE: anonymous pull works"
fi

# --- Signing host checks (read-only) ---------------------------------------------------------

keystatus=$(ssh_host "zfs get -H -o value keystatus $KEY_DATASET") || die "can't read the key status of $KEY_DATASET on the signing host over SSH"
if [ "$keystatus" != available ]; then
  die "the signing key's dataset $KEY_DATASET is locked ($keystatus). On the signing host, run:
    zfs load-key $KEY_DATASET && zfs mount $KEY_DATASET
then run this again. This script never unlocks it."
fi
[ "$(ssh_host "zfs get -H -o value mounted $KEY_DATASET")" = yes ] ||
  die "$KEY_DATASET has its key loaded but is not mounted: on the signing host, run zfs mount $KEY_DATASET"
ssh_host "test -r $KEY_DIR/code_sign_private" || die "no readable $KEY_DIR/code_sign_private on the signing host"
ssh_host "command -v podman > /dev/null" || die "podman is not installed on the signing host; stage and sign run in the server image there"
echo "signing host: $KEY_DATASET unlocked, key present, podman available"

if $WHAT_IF; then
  cat << PLAN
would, from commit $sha:
2. stage $APP $VERSION ($PLATFORM, $PLAN_CLASS) on the signing host: pull $SERVER_IMAGE (short-lived ACR token), run
   scripts/boinc/install.py in it with --network=none, pinned to $WORKER_IMAGE
3. sign each file with sign_executable (--network=none, key $KEY_DIR read-only); bring back the .sig files
4. build $registry.azurecr.io/$APPVER_REPO:$APPVER_TAG in ACR Tasks (every signature verified),
   scripts/provision-boinc.sh --env $ENV_NAME --publish-image <it>, then start the publish job
PLAN
  exit 0
fi

work=$(mktemp -d)
remote=""
cleanup() {
  [ -z "$remote" ] || ssh_host "rm -rf $remote" || echo "warning: could not remove $remote on the signing host" >&2
  rm -rf "$work"
}
trap cleanup EXIT
if [ "$PLATFORM" != x86_64-pc-linux-gnu ]; then
  cp "$WRAPPER" "$work/wrapper"
  python3 scripts/boinc/check-mac-wrapper.py --wrapper "$work/wrapper" --platform "$PLATFORM" \
    --sha256 "$WRAPPER_SHA" --source-pin "$WRAPPER_PIN" || die "Mac wrapper changed since verification"
fi

# --- 2. Stage --------------------------------------------------------------------------------

remote=$(ssh_host "mktemp -d /var/tmp/usnm-appver.XXXXXX") || die "can't make a work directory on the signing host"
[[ $remote =~ ^/var/tmp/usnm-appver\.[A-Za-z0-9]+$ ]] || die "unexpected work directory $remote"
echo "2. staging on the signing host ($remote)"
baz acr login --name "$registry" --expose-token --query accessToken -o tsv |
  ssh_host "podman login --authfile $remote/auth.json -u 00000000-0000-0000-0000-000000000000 --password-stdin $registry.azurecr.io > /dev/null" ||
  die "can't log the signing host in to $registry"
ssh_host "podman pull -q --authfile $remote/auth.json $SERVER_IMAGE > /dev/null; rc=\$?; rm -f $remote/auth.json; exit \$rc" ||
  die "the signing host can't pull $SERVER_IMAGE"
git archive --format=tar HEAD scripts/boinc/install.py ocr/boinc contrib-compute/boinc/project.xml | ssh_host "mkdir $remote/src && tar -x -C $remote/src" ||
  die "can't copy the sources to the signing host"
wrapper_path=/usr/local/lib/usnm-boinc/docker_wrapper
wrapper_mount=""
if [ "$PLATFORM" != x86_64-pc-linux-gnu ]; then
  ssh_host "cat > $remote/wrapper" < "$work/wrapper" || die "can't copy the verified Mac wrapper"
  wrapper_path=/wrapper
  wrapper_mount="-v $remote/wrapper:/wrapper:ro"
fi
ssh_host "mkdir -p $remote/proj && touch $remote/proj/config.xml &&
  podman run --rm --network=none $wrapper_mount -v $remote/src:/src:ro -v $remote/proj:/proj --entrypoint python3 $SERVER_IMAGE \
    /src/scripts/boinc/install.py --project /proj --wrapper $wrapper_path \
    --image $WORKER_IMAGE --version $VERSION --app $APP --plan-class $PLAN_CLASS --platform $PLATFORM > /dev/null" || die "staging failed on the signing host"

# --- 3. Sign ---------------------------------------------------------------------------------

echo "3. signing on the signing host"
# shellcheck disable=SC2016 # expanded in the container
ssh_host "podman run --rm --network=none -v $KEY_DIR:/key:ro -v $remote/proj/$VERSION_SUBDIR:/v --entrypoint /bin/sh $SERVER_IMAGE -ec '
  cd /v
  for f in *; do
    case \$f in version.xml | *.sig) continue ;; esac
    /usr/local/boinc/tools/sign_executable \"\$f\" /key/code_sign_private > \"\$f.sig\"
    test -s \"\$f.sig\"
  done'" || die "signing failed on the signing host"
mkdir "$work/sigs"
ssh_host "cd $remote/proj/$VERSION_SUBDIR && tar -c *.sig" | tar -x -C "$work/sigs" || die "can't fetch the .sig files"
for f in "$work"/sigs/*; do [[ $(basename "$f") == *.sig ]] || die "unexpected file $(basename "$f") from the signing host"; done
echo "   signatures: $(cd "$work/sigs" && echo *)"

# --- 4. Publish ------------------------------------------------------------------------------

echo "4. building $APPVER_REPO:$APPVER_TAG in $registry"
ctx="$work/ctx"
mkdir -p "$ctx/src"
git archive --format=tar HEAD scripts/boinc/install.py ocr/boinc contrib-compute/boinc/project.xml | tar -x -C "$ctx/src"
git show "HEAD:contrib-compute/images/appver/Containerfile" > "$ctx/Containerfile"
git show "HEAD:contrib-compute/images/appver/publish-appver.sh" > "$ctx/publish-appver.sh"
git show "HEAD:contrib-compute/images/appver/check_appver.py" > "$ctx/check_appver.py"
mv "$work/sigs" "$ctx/sigs"
mkdir "$ctx/wrapper-candidate"
appver_wrapper=/usr/local/lib/usnm-boinc/docker_wrapper
if [ "$PLATFORM" != x86_64-pc-linux-gnu ]; then
  cp "$work/wrapper" "$ctx/wrapper-candidate/docker_wrapper"
  appver_wrapper=/tmp/wrapper-candidate/docker_wrapper
fi
printf '%s' "$PUBLIC_KEY" > "$ctx/code_sign_public"
baz acr build --registry "$registry" --image "$APPVER_REPO:$APPVER_TAG" --platform linux/amd64 --timeout 1800 \
  --build-arg "SERVER_IMAGE=$SERVER_IMAGE" --build-arg "APP=$APP" --build-arg "VERSION=$VERSION" \
  --build-arg "WORKER_IMAGE=$WORKER_IMAGE" --build-arg "PLAN_CLASS=$PLAN_CLASS" --build-arg "PLATFORM=$PLATFORM" \
  --build-arg "WRAPPER_PATH=$appver_wrapper" --file "$ctx/Containerfile" "$ctx" ||
  die "the app version image build failed (a signature that doesn't verify fails it)"
appver_image="$registry.azurecr.io/$APPVER_REPO@$(digest "$APPVER_REPO" "$APPVER_TAG")"
echo "   $appver_image"

echo "   deploying the publish job"
scripts/provision-boinc.sh --env "$ENV_NAME" --publish-image "$appver_image"
job=$(boinc_output publishJobName)
[ -n "$job" ] || die "the stack has no publishJobName output"
execution=$(baz containerapp job start --name "$job" --resource-group "$BOINC_RG" --query name -o tsv) || die "could not start $job"
echo "   started $job ($execution)"
status=""
for _ in $(seq 1 90); do
  status=$(baz containerapp job execution show --name "$job" --resource-group "$BOINC_RG" \
    --job-execution-name "$execution" --query properties.status -o tsv) || die "could not read $execution"
  case "$status" in Succeeded | Failed | Stopped | Degraded) break ;; esac
  sleep 10
done
[ "$status" = Succeeded ] ||
  die "$job execution $execution ended $status: see its logs (ContainerAppConsoleLogs_CL, ContainerJobName_s == \"$job\")"
echo "published $APP $VERSION ($execution)"
if curl -fsS "${PROJECT_URL}usnmocr/apps.php" | grep -q "$VERSION"; then
  echo "${PROJECT_URL}usnmocr/apps.php lists version $VERSION"
else
  echo "warning: ${PROJECT_URL}usnmocr/apps.php does not list $VERSION yet" >&2
fi
# Recorded like build-boinc-images.sh records the server and bridge: the stack keeps the publish
# job on the last published version once this is committed.
tmp=$(mktemp)
jq --indent 2 --arg a "$appver_image" '.images.appver = $a' "$BOINC_SETTINGS" > "$tmp" && mv "$tmp" "$BOINC_SETTINGS"
echo "wrote images.appver to $BOINC_SETTINGS: commit it through a pull request, so the next provision-boinc.sh keeps the publish job on this version"
echo "next: the end-to-end client test (contrib-compute/docs/app-release.md, \"Test client\")"
