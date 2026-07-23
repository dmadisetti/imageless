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
CTR_ADDRESS=${ENDPOINT#unix://}
CRICTL=(crictl --runtime-endpoint "$ENDPOINT" --image-endpoint "$ENDPOINT")
mkdir -p "$STATE_DIR"

import_artifacts() {
  ctr --address "$CTR_ADDRESS" --namespace k8s.io images import "$IMAGELESS_SMOKE_IMAGE_ARCHIVE" >/dev/null
  local sandbox_image
  sandbox_image=$("${CRICTL[@]}" info | jq -r 'first(.. | objects | .sandboxImage? // empty)')
  if [[ -n $sandbox_image ]] && ! ctr --address "$CTR_ADDRESS" --namespace k8s.io images list --quiet | grep -Fxq "$sandbox_image"; then
    ctr --address "$CTR_ADDRESS" --namespace k8s.io images tag "$LOCAL_IMAGE" "$sandbox_image" >/dev/null
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

FAILED=$("${CRICTL[@]}" create --no-pull "$POD" "$WORK/failed.json" "$WORK/pod.json")
if "${CRICTL[@]}" start "$FAILED" >/dev/null 2>&1; then
  echo "expected failed workload start to fail" >&2
  exit 1
fi
cleanup_container "$FAILED"

# Exercise normal successful delete before creating the reboot witness.
cleanup_container "$MAIN"; MAIN=''
cleanup_container "$SIDECAR"; SIDECAR=''
cleanup_pod "$POD"; POD=''
trap - EXIT
rm -rf "$WORK"

fresh_selected_workload pre-reboot yes
echo "pre-reboot imageless CRI smoke passed; reboot the node, then run: imageless-cri-smoke post-reboot"
