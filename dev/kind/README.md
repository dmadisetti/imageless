# Dev Kubernetes cluster for the imageless runtime (kind)

From nothing to a flake serving HTTP on a real Kubernetes cluster, in about
five minutes, without touching the host's `/etc` or its Nix store. This is
the local Kubernetes counterpart of `dev/docker/` — same policy, same seed
image, but through kubelet, CRI, and a `RuntimeClass` instead of
`docker run`.

The authoritative proof of this wiring is the hermetic CRI VM gate
(`.#imageless-cri-vm`); this harness reproduces its node setup on a
[kind](https://kind.sigs.k8s.io/) node so you can poke at it interactively.

**Requirements:** Linux x86_64 host, Nix with flakes, Docker,
kind ≥ 0.27 (containerd 2.x nodes), kubectl. All commands run from the
repository root.

## 1. Create the cluster

```sh
kind create cluster --name imageless --config dev/kind/kind-config.yaml
```

Expected: `Set kubectl context to "kind-imageless"` after ~60–90 s. The
config's `containerdConfigPatches` merges the `imageless` runtime entry
(the body of `examples/containerd-config.toml`) at node creation — there is
no containerd restart step, and no later reconciliation: config changes mean
recreating the cluster.

## 2. Make the node an imageless node

```sh
dev/kind/setup.sh imageless
```

Expected: stage lines ending in `node imageless-control-plane ready` after
~30–90 s. The script builds `.#imageless`, stages its closure as a
self-contained store (`nix copy --to 'local?root=…'`), imports it into the
node, links `imageless-runc` where the containerd patch's `BinaryName`
expects it, installs `examples/dev-policy.json` root-owned at
`/etc/imageless/policy.json` (the production binary's ownership check passes
because `docker exec` writes it as the node's root), and labels the node to
satisfy `examples/runtimeclass.yaml`'s scheduling gate.

Why the node gets its own store instead of a bind of the host's: the GC-root
guarantee — a live container survives `nix-collect-garbage` — only holds
when the store, the bundles, and the GC share one mount namespace. A host
bind would leave root symlinks pointing at paths the host GC cannot
resolve, and the guarantee would fail silently.

Add `--prewarm` to pre-realize the nginx rootfs on the node (adds 1–2 min
here, and the first pod then starts in seconds instead of minutes).

## 3. Load the seed image

```sh
nix build .#nginx-embedded-image
zcat -f result > /tmp/imageless-seed.tar
kind load image-archive /tmp/imageless-seed.tar --name imageless
```

Expected: `Image: "localhost/imageless-nginx-embedded:e2e" ... loaded`. The
image is a few kilobytes: its only contents are
`examples/nginx-embedded/{flake.nix,flake.lock}` at `etc/imageless/`. The
`zcat` matters — `dockerTools.buildImage` emits a gzipped archive and the
node's containerd import wants a plain tar (the same pipeline the CRI VM
smoke uses).

## 4. Run it

```sh
kubectl apply -f examples/runtimeclass.yaml -f dev/kind/pod-nginx-embedded.yaml
kubectl wait --for=condition=Ready pod/imageless-nginx --timeout=300s
```

Expected: `pod/imageless-nginx condition met`. Without `--prewarm` the first
pod takes 2–4 min: the node fetches the flake's pinned nixpkgs, evaluates
`#rootfs`, and substitutes nginx from cache.nixos.org. If a slow network
trips the kubelet's create deadline, the automatic retry hits the
now-warm store and succeeds. Subsequent pods start in seconds.

```sh
kubectl port-forward pod/imageless-nginx 18080:18080 &
curl -s http://127.0.0.1:18080/   # => imageless-nginx-ok
```

## 5. Prove it was imageless

```sh
docker exec imageless-control-plane ctr -n k8s.io images ls | grep nginx-embedded
```

The image the node holds is a few kilobytes and contains no nginx — the
filesystem serving that response was materialized on the node from the
embedded flake at container-create, GC-rooted per bundle. `kubectl exec`
into the pod and look around: the rootfs is the flake's `#rootfs` output,
read-only, with `/nix/store` bound alongside it.

## Cleanup, GC, and re-runs

- Teardown: `kind delete cluster --name imageless` (removes the node and its
  store; nothing was installed on the host).
- The node's `/nix` grows on the Docker data-root (pinned nixpkgs plus the
  workload closures, a few hundred MB for the nginx demo). Safe cleanup is
  node-side GC — `docker exec imageless-control-plane
  /usr/local/bin/imageless-runc --help >/dev/null; docker exec
  imageless-control-plane sh -c 'PATH=/nix/var/nix/gcroots/imageless-runc/bin:$PATH
  nix-store --gc'` — which honors the per-bundle roots of live containers
  and the shim's own root. Do not `docker system prune` your way out while
  the cluster exists.
- Re-running `setup.sh` after rebuilding `.#imageless` refreshes the node
  store from a fresh staging copy; store paths the node materialized on its
  own are forgotten by the imported db, so treat a re-run as node re-init
  and recreate imageless pods.

## Production is not this

This harness runs the **development posture**: `cache_only: false` with
`path:` evaluation allowed (`examples/dev-policy.json`), so the node
evaluates embedded flakes. Production nodes keep the shipped fail-closed
default (`cache_only: true`) and resolve digest-addressed releases — see
"Who runs the evaluation" in the top-level README and SPEC.md.

## k3d / k3s

Deliberately deferred: k3s regenerates containerd's config on every start,
and customizing it goes through a `config.toml.tmpl` whose correct variant
(`config-v3.toml.tmpl` vs `config.toml.tmpl`) depends on the k3s release's
containerd generation — a version matrix that doubles this document for
zero additional coverage, since the seam being demonstrated (a `BinaryName`
under the stock runc-v2 shim) is identical. The shape of the port is known
(template the base config + the runtime table, `k3d image import`, volume
mounts for store and policy); it becomes interesting when the
`kubectl imageless` push flow lands, because `k3d --registry-create` is the
easiest local-registry path.
