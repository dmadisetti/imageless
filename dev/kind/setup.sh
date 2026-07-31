#!/usr/bin/env bash
# Bootstrap a kind node (created from dev/kind/kind-config.yaml) into an
# imageless node: a self-contained /nix carrying the production `.#imageless`
# closure, the shim on the path containerd's BinaryName expects, the dev
# policy, and the RuntimeClass capability label.
#
# The node gets its OWN store rather than a bind of the host's: the GC-root
# guarantee (a live container survives nix-collect-garbage) only holds when
# the store, the bundles, and the GC share one mount namespace. The package
# bakes absolute IMAGELESS_NIX/IMAGELESS_NIX_STORE paths, so importing its
# closure also delivers the exact nix the shim drives — nothing else to
# install.
#
# Usage: dev/kind/setup.sh [CLUSTER_NAME] [--prewarm]
#   --prewarm  pre-realize the nginx-embedded rootfs on the node so the
#              first pod starts in seconds instead of minutes
#
# Idempotent for the same build. Re-running after a rebuild refreshes the
# node's store db from a fresh staging copy, which forgets store paths the
# node materialized on its own — treat a re-run as node re-init and recreate
# imageless pods afterwards.
set -euo pipefail

cluster=imageless
prewarm=0
for argument in "$@"; do
  case "$argument" in
    --prewarm) prewarm=1 ;;
    -*)
      echo "unknown flag: $argument (usage: setup.sh [CLUSTER_NAME] [--prewarm])" >&2
      exit 2
      ;;
    *) cluster="$argument" ;;
  esac
done
node="${cluster}-control-plane"
repo="$(cd "$(dirname "$0")/../.." && pwd)"

echo "==> building .#imageless (the production shim and its baked nix)"
build="$(nix build "$repo#imageless" --print-out-paths)"

echo "==> staging the closure as a self-contained store"
staging="$(mktemp -d)"
# Staged store paths carry read-only modes; restore write permission before
# removal or the cleanup silently fails for an unprivileged user.
trap 'chmod -R u+w "$staging" 2>/dev/null || true; rm -rf "$staging"' EXIT
nix copy --no-check-sigs --to "local?root=$staging" "$build"

echo "==> importing the store into node $node"
# Force root ownership: the staged tree is host-uid-owned, and a store the
# node's root nix does not own misbehaves. Extraction overwrites as root, so
# the immutable modes on existing store paths do not block a re-run.
tar --owner=0 --group=0 -C "$staging" -c nix | docker exec -i "$node" tar -C / -x

echo "==> wiring the shim, nix.conf, and dev policy"
docker exec "$node" mkdir -p /nix/var/nix/gcroots
# Protect the shim's own closure from node-side GC; workload closures are
# protected per bundle by the runtime itself.
docker exec "$node" ln -sfn "$build" /nix/var/nix/gcroots/imageless-runc
# The nix the shim drives lives in a different store path than the shim, and
# the node has no other one: `.#imageless`'s bin carries the four imageless
# binaries and no nix-store. Rooting it under a stable name is what lets the
# documented node-side GC name an interpreter that exists.
#
# eval, not build: the binary only has to exist in the NODE's imported closure,
# and outPath sidesteps the package's extra outputs (man).
node_nix="$(nix eval --raw "$repo#imageless.materializerNix.outPath")"
docker exec "$node" ln -sfn "$node_nix" /nix/var/nix/gcroots/imageless-nix
docker exec "$node" ln -sfn "$build/bin/imageless-runc" /usr/local/bin/imageless-runc
# Daemonless root nix inside the node: no build users, no sandbox (the node
# is already a privileged container, and the quickstart substitutes rather
# than builds). The cert path is the node image's Debian bundle.
docker exec -i "$node" sh -c 'mkdir -p /etc/nix && cat > /etc/nix/nix.conf' <<'EOF'
experimental-features = nix-command flakes
sandbox = false
build-users-group =
ssl-cert-file = /etc/ssl/certs/ca-certificates.crt
EOF
# Installed root-owned 0600 through docker exec so the production runtime's
# policy ownership check passes; a kind extraMount would be host-uid-owned
# and fail closed.
docker exec -i "$node" sh -c \
  'mkdir -p /etc/imageless && install -m 0600 /dev/stdin /etc/imageless/policy.json' \
  <"$repo/examples/dev-policy.json"

echo "==> labeling the node for the RuntimeClass scheduling gate"
kubectl label node "$node" imageless.run/runtime=v2 --overwrite

if [ "$prewarm" = 1 ]; then
  echo "==> prewarming: realizing the nginx-embedded rootfs on the node"
  docker exec "$node" rm -rf /root/imageless-prewarm
  docker exec "$node" mkdir -p /root/imageless-prewarm
  tar -C "$repo/examples" -c nginx-embedded | docker exec -i "$node" tar -C /root/imageless-prewarm -x
  # --out-link registers an indirect GC root, so the prewarmed rootfs
  # survives node-side GC until the first pod takes its own bundle root.
  docker exec -e NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt "$node" \
    "$node_nix/bin/nix" build --out-link /root/imageless-prewarm/result \
    "path:/root/imageless-prewarm/nginx-embedded#rootfs"
fi

echo "==> node $node ready"
