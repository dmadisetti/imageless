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
inner_host=unix://$base/docker.sock
# Empty runs the DAEMONLESS profile: imageless-runc materializes in-process
# under the private root daemon, reading the policy file the helper writes as
# root. Naming an account instead runs the resolver-daemon profile with that
# account as the unprivileged evaluator — see the ceiling check below.
worker_user=${IMAGELESS_DOCKER_E2E_WORKER_USER:-}

outer() {
  docker --host "$outer_host" "$@"
}

show_logs() {
  # Nothing in here may fail the trap it runs from. These logs exist to explain
  # a failure, and losing the cleanup that follows — a privileged container and
  # a root-owned state tree — costs more than the diagnostics are worth.
  for file in "$base"/*.log; do
    test -f "$file" || continue
    echo "== $file" >&2
    tail -n 200 "$file" >&2 2>/dev/null || echo "   (unreadable)" >&2
  done
  return 0
}

cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    show_logs
  fi
  outer rm -f "$helper" >/dev/null 2>&1 || true
  # Everything the private daemon wrote lives on a tmpfs inside the helper's
  # mount namespace and died with it. What is left on the host is a handful of
  # root-owned files inside a directory THIS user owns, so reclaiming them
  # needs no privileged container.
  rm -rf "$base"
  exit "$status"
}
trap cleanup EXIT

if [[ -n $worker_user ]]; then
  # imageless-dev-resolver caps the worker at 1024 processes and applies the
  # cap BEFORE setuid, so execve returns EAGAIN when the account is already
  # past it — which every interactive login is. Refuse here rather than
  # surfacing it a minute later as an opaque container-start failure.
  if ! id -u "$worker_user" >/dev/null 2>&1; then
    echo "development worker user $worker_user does not exist on this host" >&2
    exit 1
  fi
  # `|| threads=0` because ps exits 1 on an empty selection, and an account
  # with no processes is the GOOD case here — a dedicated idle evaluator is
  # exactly what this check is meant to wave through.
  threads=$(ps -L -U "$worker_user" -o user= 2>/dev/null | wc -l) || threads=0
  if [[ $threads -ge 1024 ]]; then
    echo "development worker user $worker_user already runs $threads threads, at or over" >&2
    echo "imageless-dev-resolver's RLIMIT_NPROC ceiling of 1024: evaluation cannot exec." >&2
    exit 1
  fi
fi

outer info >/dev/null
outer load --input "$IMAGELESS_DOCKER_HELPER_ARCHIVE" >/dev/null
outer rm -f "$helper" >/dev/null 2>&1 || true
rm -rf "$base"
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
    # 022, not 077: root writes these logs and the invoking user has to be able
    # to read them when the run fails. The runtime accepts the policy file at
    # 644 — its ownership check refuses a file that is group- or world-WRITABLE,
    # which this is not.
    umask 022
    printf "%s\n" \
      "{\"system\":\"x86_64-linux\",\"cache_only\":false,\"eval_allowed_uri_prefixes\":[\"path:\"],\"issuers\":{}}" \
      > "$host_base/policy.json"
    # The daemon writes thousands of small files per layer under vfs. On the
    # host filesystem that cost more to delete afterwards than the whole test
    # cost to run, and it needed a privileged container to do the deleting.
    # The bind of / is rprivate, so these mounts are invisible to the host and
    # die with this container.
    /bin/busybox mkdir -p "$host_base/data" "$host_base/exec"
    /bin/busybox mount -t tmpfs tmpfs "$host_base/data"
    /bin/busybox mount -t tmpfs tmpfs "$host_base/exec"
    export PATH=$CONTAINERD_BIN:${STOCK_RUNC%/*}:/run/current-system/sw/bin
    export IMAGELESS_RUNC=$STOCK_RUNC
    export IMAGELESS_RUNC_ERROR_LOG=$BASE/imageless-runc.log
    export IMAGELESS_REALIZATION_TIMEOUT_SECONDS=60
    if [ -n "$WORKER_USER" ]; then
      export IMAGELESS_RESOLVER_SOCKET=$BASE/resolver.sock
      /bin/busybox chroot /host "$RESOLVER" \
        --socket-path "$IMAGELESS_RESOLVER_SOCKET" \
        --max-realizations 2 \
        --realization-timeout-seconds 60 \
        --policy-file "$BASE/policy.json" \
        --development-worker "$DEV_RESOLVER" \
        --development-worker-user "$WORKER_USER" \
        > "$host_base/resolver.log" 2>&1 &
    else
      # The runtime runs as this root daemon, so the ownership check is
      # satisfied by a file root just wrote. That means the PRODUCTION build
      # works here — no inline-policy feature, unlike the by-hand daemon in
      # dev/docker/README.md, which cannot own a file root will accept.
      export IMAGELESS_POLICY=$BASE/policy.json
    fi
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

# Ten seconds is a timeout, not an estimate: on a tmpfs data-root the socket
# opens in under a third of a second. A helper that has already exited never
# opens it at all, so say so while dockerd.log still explains why.
ready=''
for _ in $(seq 1 200); do
  if docker --host "$inner_host" info >/dev/null 2>&1; then
    ready=yes
    break
  fi
  if [[ $(outer inspect --format '{{.State.Running}}' "$helper" 2>/dev/null) != true ]]; then
    echo "the private dockerd exited before it opened $inner_host" >&2
    exit 1
  fi
  sleep 0.05
done
# Budget exhausted: let the client report the connection error rather than
# failing below on an assertion about a daemon that was never there.
[[ -n $ready ]] || docker --host "$inner_host" info >/dev/null

docker --host "$inner_host" info --format '{{json .Runtimes}}' \
  | jq -e '.imageless.path != null' >/dev/null

DOCKER_HOST=$inner_host \
IMAGELESS_DOCKER_RUNTIME=imageless \
IMAGELESS_DOCKER_NETWORK=none \
  bash "$IMAGELESS_DOCKER_SCENARIO"
