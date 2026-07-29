set -euo pipefail

MODE=${1:-}
if [[ $MODE != pre-reboot && $MODE != post-reboot ]]; then
  echo "usage: imageless-cri-smoke {pre-reboot|post-reboot}" >&2
  exit 2
fi
if [[ $(id -u) != 0 ]]; then
  echo "imageless-cri-smoke must run as root" >&2
  exit 1
fi

STATE_DIR=${IMAGELESS_SMOKE_STATE_DIR:-/var/lib/imageless-cri-smoke}
ENDPOINT=${CONTAINER_RUNTIME_ENDPOINT:-unix:///run/containerd/containerd.sock}
HANDLER=${IMAGELESS_RUNTIME_HANDLER:-imageless}
LOCAL_IMAGE=localhost/imageless-smoke:phase0
EMBEDDED_IMAGE=localhost/imageless-cri-embedded:phase0
CTR_ADDRESS=${ENDPOINT#unix://}
CRICTL=(crictl --runtime-endpoint "$ENDPOINT" --image-endpoint "$ENDPOINT")
mkdir -p "$STATE_DIR"

import_artifacts() {
  # dockerTools emits gzipped archives, and `ctr images import` learned
  # transparent decompression only in containerd 2.x — feed every
  # generation a plain tar (zcat -f passes uncompressed data through).
  zcat -f "$IMAGELESS_SMOKE_IMAGE_ARCHIVE" \
    | ctr --address "$CTR_ADDRESS" --namespace k8s.io images import - >/dev/null
  zcat -f "$IMAGELESS_SMOKE_EMBEDDED_IMAGE_ARCHIVE" \
    | ctr --address "$CTR_ADDRESS" --namespace k8s.io images import - >/dev/null
  local sandbox_image
  sandbox_image=$("${CRICTL[@]}" info | jq -r 'first(.. | objects | .sandboxImage? // empty)')
  if [[ -n $sandbox_image ]] && ! ctr --address "$CTR_ADDRESS" --namespace k8s.io images list --quiet | grep -Fxq "$sandbox_image"; then
    # --force keeps this idempotent: after a reboot the tag survives in the
    # image store, and the list-based guard above can race the import that
    # just recreated it (containerd 1.7 refuses to overwrite without force).
    ctr --address "$CTR_ADDRESS" --namespace k8s.io images tag --force "$LOCAL_IMAGE" "$sandbox_image" >/dev/null
  fi
}

make_rootfs() {
  ROOTFS=$(nix-store --realise "$IMAGELESS_SMOKE_ROOTFS_DRV" | tail -n 1)
}

write_pod_config() {
  local file=$1 name=$2 uid=$3
  jq -n \
    --arg name "$name" --arg uid "$uid" --arg release "$IMAGELESS_SMOKE_RELEASE_REFERENCE" \
    '{metadata:{name:$name,namespace:"imageless-smoke",uid:$uid,attempt:1},
      annotations:{"imageless.run/release-v1":$release,
        "imageless.run/containers-v1":"init,main,failed"},
      linux:{security_context:{namespace_options:{network:2,pid:1,ipc:1}}}}' >"$file"
}

write_container_config() {
  local file=$1 name=$2 command=$3
  jq -n \
    --arg name "$name" --arg image "$LOCAL_IMAGE" --arg release "$IMAGELESS_SMOKE_RELEASE_REFERENCE" --arg command "$command" \
    '{metadata:{name:$name,attempt:1},image:{image:$image},command:["/bin/sh","-c",$command],
      annotations:{"imageless.run/release-v1":$release,
        "imageless.run/containers-v1":"init,main,failed",
        "io.kubernetes.cri.container-type":"container","io.kubernetes.cri.container-name":$name},
      linux:{security_context:{readonly_rootfs:true}}}' >"$file"
}

root_for_container() {
  local id=$1
  find /run/containerd/io.containerd.runtime.v2.task/k8s.io/"$id" \
    -maxdepth 1 -name .imageless-rootfs-gcroot -type l -print -quit 2>/dev/null || true
}

cleanup_container() {
  local id=${1:-}
  [[ -z $id ]] || "${CRICTL[@]}" stop --timeout 1 "$id" >/dev/null 2>&1 || true
  [[ -z $id ]] || "${CRICTL[@]}" rm "$id" >/dev/null 2>&1 || true
}

cleanup_pod() {
  local id=${1:-}
  [[ -z $id ]] || "${CRICTL[@]}" stopp "$id" >/dev/null 2>&1 || true
  [[ -z $id ]] || "${CRICTL[@]}" rmp "$id" >/dev/null 2>&1 || true
}

fresh_selected_workload() {
  local label=$1 keep=${2:-no}
  local work pod main root_link
  work=$(mktemp -d)
  write_pod_config "$work/pod.json" "imageless-$label" "imageless-$label-$(date +%s%N)"
  write_container_config "$work/main.json" main "/bin/sleep 600"
  pod=$("${CRICTL[@]}" runp --runtime "$HANDLER" "$work/pod.json")
  main=$("${CRICTL[@]}" create --no-pull "$pod" "$work/main.json" "$work/pod.json")
  "${CRICTL[@]}" start "$main" >/dev/null
  root_link=$(root_for_container "$main")
  [[ -L $root_link ]] || { echo "selected workload has no bundle GC root" >&2; return 1; }
  [[ $(readlink "$root_link") == "$ROOTFS" ]]
  if [[ $keep == yes ]]; then
    printf '%s\n' "$ROOTFS" >"$STATE_DIR/old-rootfs"
    printf '%s\n' "$root_link" >"$STATE_DIR/old-root-link"
    printf '%s\n' "$pod" >"$STATE_DIR/old-pod"
    printf '%s\n' "$main" >"$STATE_DIR/old-container"
  else
    cleanup_container "$main"
    cleanup_pod "$pod"
  fi
  rm -rf "$work"
}

# Embedded-layer bootstrap (SPEC.md §2.1 under CRI): a pod with NO imageless
# annotations whose container image carries etc/imageless/flake.nix — and no
# top-level /bin — in its layer. The flake alone must select the container,
# and the workload can only run out of the materialized rootfs.
embedded_bootstrap() {
  local label=$1
  local work pod main state root_link target
  work=$(mktemp -d)
  jq -n \
    --arg name "imageless-embedded-$label" --arg uid "imageless-embedded-$label-$(date +%s%N)" \
    '{metadata:{name:$name,namespace:"imageless-smoke",uid:$uid,attempt:1},
      linux:{security_context:{namespace_options:{network:2,pid:1,ipc:1}}}}' >"$work/pod.json"
  jq -n \
    --arg name embedded --arg image "$EMBEDDED_IMAGE" \
    '{metadata:{name:$name,attempt:1},image:{image:$image},
      command:["/bin/busybox","sleep","600"],
      annotations:{"io.kubernetes.cri.container-type":"container","io.kubernetes.cri.container-name":$name},
      linux:{security_context:{readonly_rootfs:true}}}' >"$work/main.json"
  pod=$("${CRICTL[@]}" runp --runtime "$HANDLER" "$work/pod.json")
  main=$("${CRICTL[@]}" create --no-pull "$pod" "$work/main.json" "$work/pod.json")
  "${CRICTL[@]}" start "$main" >/dev/null
  # A passthrough bundle would have no /bin/busybox and exit on the exec
  # (asynchronously on newer containerd — settle before inspecting).
  sleep 1
  state=$("${CRICTL[@]}" inspect --output json "$main" | jq -r '.status.state')
  [[ $state == CONTAINER_RUNNING ]] || {
    echo "embedded workload is not running (state: $state)" >&2
    return 1
  }
  root_link=$(root_for_container "$main")
  [[ -L $root_link ]] || { echo "embedded workload has no bundle GC root" >&2; return 1; }
  target=$(readlink "$root_link")
  [[ $target == /nix/store/* && $target != "$ROOTFS" ]] || {
    echo "embedded rootfs resolved to an unexpected path: $target" >&2
    return 1
  }
  [[ -f $target/etc/imageless-cri-embedded ]] || {
    echo "materialized root is missing the embedded marker" >&2
    return 1
  }
  cleanup_container "$main"
  cleanup_pod "$pod"
  rm -rf "$work"
}

# External flake reference (SPEC §3): the same evaluation opt-in as embedded
# bootstrap, but the flake reaches the node over a remote transport instead of
# an image layer. The VM is hermetic, so the smoke serves the tarball itself on
# guest loopback — the only network the reference needs is one the gate
# controls. The positive case runs a pinned (?narHash=) reference through the
# resolver; the negative cases prove both deny paths: a remote prefix the
# policy never allow-listed, and a node-local scheme rejected before policy.
external_reference() {
  local label=$1
  local work pod main state root_link target narhash source httpd_pid=''
  work=$(mktemp -d)
  cp "$IMAGELESS_SMOKE_EXTERNAL_TARBALL" "$work/flake.tar.gz"
  "$IMAGELESS_SMOKE_HTTPD" httpd -f -p 127.0.0.1:8081 -h "$work" &
  httpd_pid=$!
  # shellcheck disable=SC2064
  trap "kill $httpd_pid 2>/dev/null || true" RETURN
  narhash=''
  for _ in $(seq 1 20); do
    narhash=$(nix --extra-experimental-features 'nix-command flakes' \
      flake metadata --json "tarball+http://127.0.0.1:8081/flake.tar.gz" \
      2>/dev/null | jq -r '.locked.narHash // empty') && [[ -n $narhash ]] && break
    sleep 0.25
  done
  [[ -n $narhash ]] || { echo "could not fetch the loopback flake tarball" >&2; return 1; }
  source="tarball+http://127.0.0.1:8081/flake.tar.gz?narHash=$narhash"

  jq -n \
    --arg name "imageless-external-$label" --arg uid "imageless-external-$label-$(date +%s%N)" \
    --arg source "$source" \
    '{metadata:{name:$name,namespace:"imageless-smoke",uid:$uid,attempt:1},
      annotations:{"run.imageless.source":$source,"run.imageless.containers":"external"},
      linux:{security_context:{namespace_options:{network:2,pid:1,ipc:1}}}}' >"$work/pod.json"
  jq -n \
    --arg name external --arg image "$LOCAL_IMAGE" --arg source "$source" \
    '{metadata:{name:$name,attempt:1},image:{image:$image},
      command:["/bin/busybox","sleep","600"],
      annotations:{"run.imageless.source":$source,"run.imageless.containers":"external",
        "io.kubernetes.cri.container-type":"container","io.kubernetes.cri.container-name":$name},
      linux:{security_context:{readonly_rootfs:true}}}' >"$work/main.json"
  pod=$("${CRICTL[@]}" runp --runtime "$HANDLER" "$work/pod.json")
  main=$("${CRICTL[@]}" create --no-pull "$pod" "$work/main.json" "$work/pod.json")
  "${CRICTL[@]}" start "$main" >/dev/null
  sleep 1
  state=$("${CRICTL[@]}" inspect --output json "$main" | jq -r '.status.state')
  [[ $state == CONTAINER_RUNNING ]] || {
    echo "external-reference workload is not running (state: $state)" >&2
    return 1
  }
  root_link=$(root_for_container "$main")
  [[ -L $root_link ]] || { echo "external-reference workload has no bundle GC root" >&2; return 1; }
  target=$(readlink "$root_link")
  [[ $target == /nix/store/* && $target != "$ROOTFS" ]] || {
    echo "external rootfs resolved to an unexpected path: $target" >&2
    return 1
  }
  [[ -f $target/etc/imageless-cri-external ]] || {
    echo "materialized root is missing the external marker" >&2
    return 1
  }
  cleanup_container "$main"
  cleanup_pod "$pod"

  # Deny paths. CRI's CreateContainer only records metadata — the shim (and
  # with it the planner and the resolver) runs at task-create, i.e. during
  # `crictl start` (StartContainer -> shim Create -> runc create), identically
  # on both containerd generations. A resolution failure fails that Create RPC
  # synchronously, so `create` succeeds and `start` is the step that must err —
  # unlike the failed-exec workload below, whose error is post-fifo and async.
  local cursor denied
  cursor=$(journalctl -u imageless-resolver -n0 --show-cursor --no-pager 2>/dev/null \
    | sed -n 's/^-- cursor: //p' || true)
  resolver_journal() {
    if [[ -n $cursor ]]; then
      journalctl -u imageless-resolver --after-cursor "$cursor" --no-pager 2>/dev/null
    else
      journalctl -u imageless-resolver --no-pager 2>/dev/null
    fi
  }

  # Deny path 1: a remote-scheme reference whose prefix the node never
  # allow-listed must fail task-create (PolicyDenied at the resolver).
  jq -n \
    --arg name "imageless-denied-$label" --arg uid "imageless-denied-$label-$(date +%s%N)" \
    '{metadata:{name:$name,namespace:"imageless-smoke",uid:$uid,attempt:1},
      annotations:{"run.imageless.source":"github:imageless-smoke/denied",
        "run.imageless.containers":"denied"},
      linux:{security_context:{namespace_options:{network:2,pid:1,ipc:1}}}}' >"$work/denied-pod.json"
  jq -n \
    --arg name denied --arg image "$LOCAL_IMAGE" \
    '{metadata:{name:$name,attempt:1},image:{image:$image},
      command:["/bin/busybox","sleep","600"],
      annotations:{"run.imageless.source":"github:imageless-smoke/denied",
        "run.imageless.containers":"denied",
        "io.kubernetes.cri.container-type":"container","io.kubernetes.cri.container-name":$name},
      linux:{security_context:{readonly_rootfs:true}}}' >"$work/denied.json"
  pod=$("${CRICTL[@]}" runp --runtime "$HANDLER" "$work/denied-pod.json")
  denied=$("${CRICTL[@]}" create --no-pull "$pod" "$work/denied.json" "$work/denied-pod.json")
  if "${CRICTL[@]}" start "$denied" >/dev/null 2>&1; then
    echo "starting a workload with a non-allow-listed external reference must fail" >&2
    return 1
  fi
  for _ in $(seq 1 40); do
    resolver_journal | grep -q "not authorized by node policy" && break
    sleep 0.25
  done
  resolver_journal | grep -q "not authorized by node policy" || {
    echo "resolver journal never recorded the policy denial" >&2
    return 1
  }
  cleanup_container "$denied"
  cleanup_pod "$pod"

  # Deny path 2: a node-local scheme is rejected in the shim's planner, before
  # any resolver is consulted (SPEC §3 — the in-image `/` form is the only way
  # an annotation names node-local content). Same seam: task-create.
  jq -n \
    --arg name "imageless-local-$label" --arg uid "imageless-local-$label-$(date +%s%N)" \
    '{metadata:{name:$name,namespace:"imageless-smoke",uid:$uid,attempt:1},
      annotations:{"run.imageless.source":"file:///etc",
        "run.imageless.containers":"local"},
      linux:{security_context:{namespace_options:{network:2,pid:1,ipc:1}}}}' >"$work/local-pod.json"
  jq -n \
    --arg name local --arg image "$LOCAL_IMAGE" \
    '{metadata:{name:$name,attempt:1},image:{image:$image},
      command:["/bin/busybox","sleep","600"],
      annotations:{"run.imageless.source":"file:///etc",
        "run.imageless.containers":"local",
        "io.kubernetes.cri.container-type":"container","io.kubernetes.cri.container-name":$name},
      linux:{security_context:{readonly_rootfs:true}}}' >"$work/local.json"
  pod=$("${CRICTL[@]}" runp --runtime "$HANDLER" "$work/local-pod.json")
  denied=$("${CRICTL[@]}" create --no-pull "$pod" "$work/local.json" "$work/local-pod.json")
  if "${CRICTL[@]}" start "$denied" >/dev/null 2>&1; then
    echo "starting a workload with a node-local scheme must fail" >&2
    return 1
  fi
  cleanup_container "$denied"
  cleanup_pod "$pod"
  rm -rf "$work"
}

import_artifacts

if [[ $MODE == post-reboot ]]; then
  [[ -s $STATE_DIR/old-rootfs && -s $STATE_DIR/old-root-link ]] || {
    echo "pre-reboot state is missing" >&2
    exit 1
  }
  OLD_ROOTFS=$(<"$STATE_DIR/old-rootfs")
  OLD_ROOT_LINK=$(<"$STATE_DIR/old-root-link")
  [[ ! -e $OLD_ROOT_LINK && ! -L $OLD_ROOT_LINK ]] || {
    echo "old bundle GC root is still active after reboot: $OLD_ROOT_LINK" >&2
    exit 1
  }
  cleanup_container "$(<"$STATE_DIR/old-container")"
  cleanup_pod "$(<"$STATE_DIR/old-pod")"
  nix-store --gc >/dev/null
  [[ ! -e $OLD_ROOTFS ]] || { echo "GC retained disposable old rootfs: $OLD_ROOTFS" >&2; exit 1; }
  make_rootfs
  fresh_selected_workload post-reboot
  embedded_bootstrap post-reboot
  rm -f "$STATE_DIR"/old-rootfs "$STATE_DIR"/old-root-link "$STATE_DIR"/old-pod "$STATE_DIR"/old-container
  echo "post-reboot imageless CRI smoke passed"
  exit 0
fi

rm -f "$STATE_DIR"/old-rootfs "$STATE_DIR"/old-root-link "$STATE_DIR"/old-pod "$STATE_DIR"/old-container
make_rootfs
WORK=$(mktemp -d)
POD=''
SIDECAR=''
MAIN=''
trap 'cleanup_container "$MAIN"; cleanup_container "$SIDECAR"; cleanup_pod "$POD"; rm -rf "$WORK"' EXIT
write_pod_config "$WORK/pod.json" imageless-lifecycle "imageless-$(date +%s%N)"
write_container_config "$WORK/init.json" init "/bin/sleep 600"
write_container_config "$WORK/sidecar.json" sidecar "/bin/sleep 600"
write_container_config "$WORK/main.json" main "/bin/sleep 600"
write_container_config "$WORK/failed.json" failed "exec /does-not-exist"

# The annotated sandbox must pass without trying to resolve its rootfs.
POD=$("${CRICTL[@]}" runp --runtime "$HANDLER" "$WORK/pod.json")

# Selected init and unselected sidecar exercise both selector branches. Assert
# the gc root on a RUNNING container: the root is planted when CRI creates the
# task (StartContainer -> runc create), and CRI deletes the task — and its
# bundle, with the gc root inside it — the moment a container EXITS
# (handleContainerExit -> task.Delete). Checking after exit races that delete
# against the CONTAINER_EXITED status update, which is why it was flaky on newer
# containerd. A long-lived command keeps the bundle observable.
INIT=$("${CRICTL[@]}" create --no-pull "$POD" "$WORK/init.json" "$WORK/pod.json")
"${CRICTL[@]}" start "$INIT" >/dev/null
INIT_ROOT=$(root_for_container "$INIT")
[[ -L $INIT_ROOT ]]
[[ $(readlink "$INIT_ROOT") == "$ROOTFS" ]]
cleanup_container "$INIT"

SIDECAR=$("${CRICTL[@]}" create --no-pull "$POD" "$WORK/sidecar.json" "$WORK/pod.json")
"${CRICTL[@]}" start "$SIDECAR" >/dev/null
[[ -z $(root_for_container "$SIDECAR") ]]

MAIN=$("${CRICTL[@]}" create --no-pull "$POD" "$WORK/main.json" "$WORK/pod.json")
"${CRICTL[@]}" start "$MAIN" >/dev/null
[[ -L $(root_for_container "$MAIN") ]]
nix-store --gc >/dev/null
[[ -e $ROOTFS ]] || { echo "GC collected a live rootfs" >&2; exit 1; }

# Restart by CRI recreation, not by relying on an implementation-specific task restart.
cleanup_container "$MAIN"
MAIN=$("${CRICTL[@]}" create --no-pull "$POD" "$WORK/main.json" "$WORK/pod.json")
"${CRICTL[@]}" start "$MAIN" >/dev/null
[[ -L $(root_for_container "$MAIN") ]]

# A selected container whose workload cannot exec is still resolved (the
# selector/materializer ran, as for init/main) but must not end up running.
# Assert on the resulting container STATE, not on `start`'s exit code: newer
# containerd unblocks the container via the exec fifo and returns success from
# `start` before the final execve, so a missing binary surfaces as an immediate
# non-zero EXIT rather than a start error (older containerd reported it
# synchronously — that timing difference is what made this flaky).
FAILED=$("${CRICTL[@]}" create --no-pull "$POD" "$WORK/failed.json" "$WORK/pod.json")
"${CRICTL[@]}" start "$FAILED" >/dev/null 2>&1 || true
FAILED_STATE=''
for _ in $(seq 1 50); do
  FAILED_STATE=$("${CRICTL[@]}" inspect --output json "$FAILED" | jq -r '.status.state')
  [[ $FAILED_STATE == CONTAINER_EXITED ]] && break
  sleep 0.1
done
[[ $FAILED_STATE == CONTAINER_EXITED ]] || {
  echo "failed workload never exited (state: $FAILED_STATE)" >&2
  exit 1
}
FAILED_CODE=$("${CRICTL[@]}" inspect --output json "$FAILED" | jq -r '.status.exitCode')
[[ $FAILED_CODE != 0 ]] || { echo "failed workload exited 0, expected non-zero" >&2; exit 1; }
cleanup_container "$FAILED"

# Exercise normal successful delete before creating the reboot witness.
cleanup_container "$MAIN"; MAIN=''
cleanup_container "$SIDECAR"; SIDECAR=''
cleanup_pod "$POD"; POD=''
trap - EXIT
rm -rf "$WORK"

# Embedded bootstrap runs before the reboot witness so the witness bundle is
# the only GC-root-carrying bundle left when the driver asserts mount shapes.
embedded_bootstrap pre-reboot

# External references likewise leave no bundle behind: the phase tears its
# workloads down (deny-path pods included) before the witness is created.
external_reference pre-reboot

fresh_selected_workload pre-reboot yes
echo "pre-reboot imageless CRI smoke passed; reboot the node, then run: imageless-cri-smoke post-reboot"
