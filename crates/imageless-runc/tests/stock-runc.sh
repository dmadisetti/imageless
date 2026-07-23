#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 ]]; then
  echo "usage: stock-runc.sh RUNC ROOTFS" >&2
  exit 2
fi

runc=$1
rootfs=$2
shell_path=$(readlink "$rootfs/bin/sh")
sleep_path=$(readlink "$rootfs/bin/sleep")
interpreter=$(readelf -l "$shell_path" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')

[[ $shell_path == /nix/store/* ]]
[[ $sleep_path == /nix/store/* ]]
[[ $interpreter == /nix/store/* ]]
[[ $shell_path != "$rootfs"/* ]]
[[ -d $rootfs/nix/store ]]

work=$(mktemp -d)
state=$work/state
bundle=$work/bundle
mkdir -p "$state" "$bundle"
cleanup() {
  "$runc" --rootless true --root "$state" delete --force imageless-stock-runc >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

(
  cd "$bundle"
  "$runc" spec --rootless
)
command="test -x $(printf '%q' "$interpreter") && /bin/sleep 0 && printf 'stock-runc-store-projection-ok\\n'"
jq \
  --arg rootfs "$rootfs" \
  --arg command "$command" \
  '.root.path = $rootfs
   | .root.readonly = true
   | .process.terminal = false
   | .process.args = ["/bin/sh", "-c", $command]
   | .mounts |= map(select(.type != "cgroup"))
   | .linux.namespaces |= map(select(.type != "cgroup"))
   | .mounts += [{
       "destination": "/nix/store",
       "options": ["rbind", "ro", "nosuid", "nodev"],
       "source": "/nix/store",
       "type": "bind"
     }]' \
  "$bundle/config.json" >"$bundle/config.next.json"
mv "$bundle/config.next.json" "$bundle/config.json"

output=$(cd "$bundle" && "$runc" --rootless true --root "$state" run imageless-stock-runc)
[[ $output == stock-runc-store-projection-ok ]]
