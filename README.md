# imageless

**Ship the flake, not the filesystem.** An OCI image that carries a Nix flake
in its layers bootstraps its own root filesystem at container-create time —
through stock Docker, containerd, and Kubernetes.

```
seed image layer:  /etc/imageless/flake.nix  (+ lock + sources)
        │
        ▼  docker run / kubelet → containerd → runc create
imageless-runc interposes `create`
        │  realize flake#rootfs into /nix/store   (bounded, GC-rooted)
        │  atomically rewrite config.json root.path
        ▼
stock runc starts the realized rootfs
```

The image you push is an ordinary OCI image — registries, digests, admission
control, `docker run`, air-gapped mirrors all keep working. It just doesn't
contain the filesystem it will run. It contains the *recipe*, and the node's
Nix store (or binary cache) supplies the result.

imageless is two things, in this order:

1. **A shim** — `imageless-runc`, a runc-compatible interposer you register as
   a Docker runtime or a containerd `BinaryName`. No custom containerd shim, no
   kubelet changes, no image-format extension.
2. **A spec** — [SPEC.md](SPEC.md), the contract for embedded flakes,
   annotations, materialization bounds, atomic rewrite, store projection, and
   GC-root lifecycle. Any OCI runtime can implement it; a Rust library
   (`imageless`) is provided for runtimes that want to link it instead
   ([Cowboy's](https://github.com/dmadisetti/cowboy) `cowboy-runtime` does
   exactly this).

## Quick start (Docker)

```bash
nix build .#imageless-runc

# 1. Register the runtime
# /etc/docker/daemon.json: {"runtimes": {"imageless": {"path": "/path/to/imageless-runc"}}}

# 2. Opt the node into evaluating embedded flakes. The shipped default is
#    cache_only: true (the node evaluates nothing); on a dev box you almost
#    always want the opt-in — examples/dev-policy.json sets "cache_only": false
sudo install -Dm600 examples/dev-policy.json /etc/imageless/policy.json

# 3. Run a seed image whose only contents are a flake and its inputs
docker run --runtime=imageless localhost/my-seed
```

No daemon. The shim materializes in-process by node policy. Multi-tenant
nodes can instead run the optional `imageless-resolver` daemon (selected via
`IMAGELESS_RESOLVER_SOCKET`) for node-wide concurrency caps, single-flight
coalescing of identical realizations, and privilege-separated evaluation.

The end-to-end proof lives in `nix build .#checks.x86_64-linux.docker-embedded-smoke`:
the seed's layer deliberately lacks the executable that produces the expected
HTTP response — a successful response proves the rootfs was materialized from
the flake, not shipped in the image.

## Kubernetes

Register a `RuntimeClass` backed by the **stock** runc-v2 shim with its OCI
runtime pointed at the interposer:

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.imageless]
  runtime_type = "io.containerd.runc.v2"
  pod_annotations = ["imageless.run/*", "run.imageless.*"]
  container_annotations = ["imageless.run/*", "run.imageless.*"]
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.imageless.options]
    BinaryName = "/path/to/imageless-runc"
```

Why this seam and not a custom containerd shim: under containerd 2.x, all of a
pod's containers are grouped into a **single** shim process, and app containers
are created by `Task.Create` RPC inside it — a custom runtime-v2 start/delete
binary only ever observes the pod sandbox. `runc create`, by contrast, runs
once per container in every containerd generation. Interposing there is
version-agnostic and is what the hermetic CRI VM test
(`nix build .#imageless-cri-vm`) validates: sandbox passthrough, per-container
selection, recreate, GC-while-running, delete-and-collect, and reboot recovery
on a real containerd/CRI node.

See `examples/` for the RuntimeClass, pod, and containerd configs, and the
NixOS module (`nixosModules.imageless`) for a packaged node setup.

## Why not containix?

[containix](https://github.com/atmask/containix) proved the appeal of
flake-launched containers on Kubernetes, and its product shape — one obvious
use case, one interception point, stock runc afterwards — is the right one.
imageless exists because we need a different set of guarantees:

| | containix | imageless |
|---|---|---|
| **Where the flake lives** | A flake *reference* in pod metadata; the node fetches and builds whatever the annotation points at. | **In the image layers.** The deployable artifact is self-contained and content-addressed; registries, digest pinning, admission policy, and air-gapped nodes work unchanged. Pointing at an external flake ref is *also* supported — but as a mode the node's policy must explicitly enable, not the default trust model. |
| **Interception seam** | A containerd TTRPC shim wrapping `Task.Create` — coupled to containerd's shim interfaces and version, Kubernetes-only. | The `runc create` CLI seam (or direct library linkage in your runtime) — works for raw Docker, containerd 1.x and 2.x, and any CRI, with no TTRPC surface to track. |
| **Trust and policy** | The node builds what workloads name; isolation/allowlisting is future work. | Node-owned policy daemon: evaluation is **off by default** (`cache_only`), staged sources are size/entry-bounded and symlink-free, evaluation (when enabled) runs in a privilege-dropped, rlimited worker, and production nodes resolve only digest-addressed releases from allow-listed issuers and caches. |
| **Lifecycle correctness** | Store GC is delegated to the operator. | GC roots are tied to the container: registered at create, released on failure or delete; a live container survives `nix-collect-garbage`. Atomic spec rewrite, bounded materialization with process-tree kill, fail-closed validation. |
| **Scope** | An experimental tool. | A spec with a reference shim, acceptance gates (Docker embedded-layer proof + CRI lifecycle VM test), and a library for embedding into other OCI runtimes. |

If you want "pod annotation → flake ref → container" with minimal machinery,
containix is simpler. If the artifact of record must remain an OCI image and
the node must decide what it will and won't build, that is imageless.

## Embedding the library

The shim is a ~200-line consumer of the `imageless` crate. A runtime that owns
its `create` path can skip the interposer entirely:

```rust
use imageless::{resolve_and_apply_bundle, remove_bundle_gc_roots};

// during OCI create, after the bundle is staged:
let applied = resolve_and_apply_bundle(
    &bundle.join("config.json"), &bundle, "rootfs", timeout_secs, &resolver_socket,
)?; // Ok(None) => not an imageless bundle; proceed unchanged
// on any later failure or at delete:
remove_bundle_gc_roots(&bundle)?;
```

## Repository layout

- `crates/imageless` — the library: spec types, bundle planning/rewrite, store
  projection, GC roots, release-manifest parsing, resolver client.
- `crates/imageless-runc` — the shim (the product).
- `crates/imageless-resolver` — the optional hardened profile: the node
  materializer daemon and its privilege-dropped evaluation worker, for
  multi-tenant nodes that want central concurrency caps, single-flight, and
  evaluation in a separate privilege domain.
- `SPEC.md` — the contract.
- `examples/`, `smoke/` — deployment examples and the acceptance smokes.

## Status and limitations

- Linux only; a flake's `rootfs` output is per-system (no implicit
  multi-arch — publish per-platform images or releases).
- The node needs Nix (evaluation posture) or a reachable binary cache
  (cache-only posture).
- The spec is v1 **draft**: annotation namespaces are settled
  (`imageless.run/*`, `run.imageless.*`), but schema details may still change
  before freeze.
- Extracted from and battle-tested inside
  [Cowboy](https://github.com/dmadisetti/cowboy); now developed standalone at
  [imageless.run](https://imageless.run).
