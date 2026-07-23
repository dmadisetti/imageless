set -euo pipefail

: "${IMAGELESS_DOCKER_IMAGE_ARCHIVE:?required}"
: "${IMAGELESS_DOCKER_HELPER_ARCHIVE:?required}"
: "${IMAGELESS_DOCKERD:?required}"
: "${IMAGELESS_CONTAINERD_BIN:?required}"
: "${IMAGELESS_STOCK_RUNC:?required}"
: "${IMAGELESS_RESOLVER:?required}"
: "${IMAGELESS_DEV_RESOLVER:?required}"
: "${IMAGELESS_RUNC_CLIENT:?required}"
: "${IMAGELESS_DOCKER_SCENARIO:?required}"

outer_host=${DOCKER_HOST:-unix:///var/run/docker.sock}
base=${IMAGELESS_DOCKER_E2E_STATE_DIR:-/tmp/imageless-docker-e2e-$(id -u)}
helper=imageless-docker-e2e-$(id -u)
helper_image=localhost/imageless-docker-helper:e2e
worker_user=$(id -un)
inner_host=unix://$base/docker.sock

outer() {
  docker --host "$outer_host" "$@"
}

root_cleanup() {
  outer run --rm --privileged --pid host --cgroupns host --network none \
    --mount type=bind,src=/,dst=/host \
    "$helper_image" rm -rf "/host$base" >/dev/null 2>&1 || true
}

show_logs() {
  outer run --rm --network none --mount type=bind,src=/,dst=/host \
    "$helper_image" sh -c \
    'for file in "$1"/*.log; do test ! -f "$file" || { echo "== $file"; /bin/busybox tail -n 200 "$file"; }; done' \
    sh "/host$base" >&2 || true
}

cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    show_logs
  fi
  outer rm -f "$helper" >/dev/null 2>&1 || true
  root_cleanup
  exit "$status"
}
trap cleanup EXIT

outer info >/dev/null
outer load --input "$IMAGELESS_DOCKER_HELPER_ARCHIVE" >/dev/null
outer rm -f "$helper" >/dev/null 2>&1 || true
root_cleanup
mkdir -p "$base"

outer run --detach --name "$helper" --privileged --pid host --cgroupns host \
  --network none --mount type=bind,src=/,dst=/host \
  --env BASE="$base" \
  --env DOCKERD="$IMAGELESS_DOCKERD" \
  --env CONTAINERD_BIN="$IMAGELESS_CONTAINERD_BIN" \
  --env STOCK_RUNC="$IMAGELESS_STOCK_RUNC" \
  --env RESOLVER="$IMAGELESS_RESOLVER" \
  --env DEV_RESOLVER="$IMAGELESS_DEV_RESOLVER" \
  --env RUNC_CLIENT="$IMAGELESS_RUNC_CLIENT" \
  --env WORKER_USER="$worker_user" \
  "$helper_image" sh -c '
    set -eu
    host_base=/host$BASE
    umask 077
    printf "%s\n" \
      "{\"system\":\"x86_64-linux\",\"cache_only\":false,\"eval_allowed_uri_prefixes\":[\"path:\"],\"issuers\":{}}" \
      > "$host_base/policy.json"
    export PATH=$CONTAINERD_BIN:${STOCK_RUNC%/*}:/run/current-system/sw/bin
    export IMAGELESS_RUNC=$STOCK_RUNC
    export IMAGELESS_RESOLVER_SOCKET=$BASE/resolver.sock
    export IMAGELESS_RUNC_ERROR_LOG=$BASE/imageless-runc.log
    export IMAGELESS_REALIZATION_TIMEOUT_SECONDS=60
    /bin/busybox chroot /host "$RESOLVER" \
      --socket-path "$IMAGELESS_RESOLVER_SOCKET" \
      --max-realizations 2 \
      --realization-timeout-seconds 60 \
      --policy-file "$BASE/policy.json" \
      --development-worker "$DEV_RESOLVER" \
      --development-worker-user "$WORKER_USER" \
      > "$host_base/resolver.log" 2>&1 &
    exec /bin/busybox chroot /host "$DOCKERD" \
      --host unix://$BASE/docker.sock \
      --data-root $BASE/data \
      --exec-root $BASE/exec \
      --pidfile $BASE/docker.pid \
      --storage-driver vfs \
      --add-runtime imageless=$RUNC_CLIENT \
      --exec-opt native.cgroupdriver=cgroupfs \
      --iptables=false --ip6tables=false --bridge=none \
      --ip-forward=false --ip-masq=false --userland-proxy=false \
      > "$host_base/dockerd.log" 2>&1
  ' >/dev/null

for _ in $(seq 1 400); do
  if docker --host "$inner_host" info >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
docker --host "$inner_host" info --format '{{json .Runtimes}}' \
  | jq -e '.imageless.path != null' >/dev/null

DOCKER_HOST=$inner_host \
IMAGELESS_DOCKER_RUNTIME=imageless \
IMAGELESS_DOCKER_NETWORK=none \
  bash "$IMAGELESS_DOCKER_SCENARIO"
