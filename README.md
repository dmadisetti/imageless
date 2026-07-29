# imageless

**Ship the flake, not the filesystem.** An OCI image that carries a Nix flake
in its layers bootstraps its own root filesystem at container-create time —
through stock Docker, containerd, and Kubernetes.

```text
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

# 3. Build and load a real seed image. `nginx-embedded-image` is a ~2 KB
#    image whose entire contents are examples/nginx-embedded/{flake.nix,flake.lock}
docker load < "$(nix build .#nginx-embedded-image --print-out-paths)"

# 4. Run it. nginx does not exist in the image; the node materializes it.
docker run --rm --runtime=imageless --network=host --tmpfs /tmp \
  localhost/imageless-nginx-embedded:e2e
curl -s http://127.0.0.1:18080/   # => imageless-nginx-ok
```

**How the runtime found the flake:** nothing above passes an annotation. A
plain regular file at `etc/imageless/flake.nix` in the image's rootfs is
auto-discovered and selects the container, with source `/etc/imageless` and the
runtime's default output (`rootfs`, see `IMAGELESS_DEFAULT_OUTPUT`). That is
the zero-config path — an image with no such file is passed straight through to
stock runc, untouched. Deployer-side overrides (a different source, a different
output, per-container selection) are annotations only; when they are present,
discovery is skipped. See [SPEC.md](SPEC.md) for the annotation set.

No daemon. The shim materializes in-process by node policy. Multi-tenant
nodes can instead run the optional `imageless-resolver` daemon (selected via
`IMAGELESS_RESOLVER_SOCKET`) for node-wide concurrency caps, single-flight
coalescing of identical realizations, and privilege-separated evaluation.

`dev/docker/` runs exactly this against a throwaway Docker daemon that touches
nothing under `/etc` — the recommended way to try it on a machine you care
about. The hermetic end-to-end proof lives in `nix build .#docker-embedded-smoke`
(a NixOS VM test; it needs KVM): the seed's layer deliberately lacks the
executable that produces the expected HTTP response — a successful response
proves the rootfs was materialized from the flake, not shipped in the image.

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
| **Trust and policy** | The node builds what workloads name; isolation/allowlisting is future work. | Node-owned policy: evaluation is **off by default** (`cache_only`), an enabled node still evaluates only URI prefixes its policy allow-lists, staged sources are size/entry-bounded and symlink-free, and production nodes resolve only digest-addressed releases from allow-listed issuers and caches. Privilege separation is opt-in: see below. |
| **Lifecycle correctness** | Store GC is delegated to the operator. | GC roots are tied to the container: registered at create, released on failure or delete; a live container survives `nix-collect-garbage`. Atomic spec rewrite, bounded materialization with process-tree kill, fail-closed validation. |
| **Scope** | An experimental tool. | A spec with a reference shim, acceptance gates (Docker embedded-layer proof + CRI lifecycle VM test), and a library for embedding into other OCI runtimes. |

If you want "pod annotation → flake ref → container" with minimal machinery,
containix is simpler. If the artifact of record must remain an OCI image and
the node must decide what it will and won't build, that is imageless.

## Who runs the evaluation

Be precise about this, because the two deployment shapes differ:

- **Daemonless (the default).** `imageless-runc` materializes in-process and
  evaluation runs **as the calling process** — under Docker or containerd that
  is root. There is no privilege drop, no rlimit, and no environment scrub on
  this path. What bounds it is policy, not privilege: evaluation is off unless
  the node's `policy.json` sets `cache_only: false`, and even then only URIs
  matching `eval_allowed_uri_prefixes` are evaluated, under a wall-clock
  timeout with process-tree kill. Appropriate when every workload on the node
  is already trusted with root — a dev box, a single-tenant node.
- **Daemon (`imageless-resolver`, opt-in via `IMAGELESS_RESOLVER_SOCKET`).**
  The daemon refuses to enable evaluation at all without
  `--development-worker`/`--development-worker-user`, and runs every evaluation
  through that worker: a separate unprivileged user, CPU rlimit, and a cleared,
  explicitly reconstructed environment. This is the posture for multi-tenant
  nodes, and it also supplies node-wide concurrency caps and single-flight.

Release-profile materialization (digest-addressed, from allow-listed issuers
and caches) substitutes rather than evaluates and is the same on both paths.

## External flake references

The embedded flake is the default trust model, but a pod may instead point the
node at a flake it does not carry: set `run.imageless.source` to a flake
reference with an explicit remote scheme (`github:`, `git+https:`,
`tarball+https:`, …). The node opts in per prefix — this is evaluation, so it
needs `cache_only: false` *and* the reference's prefix allow-listed:

```json
{
  "system": "x86_64-linux",
  "cache_only": false,
  "eval_allowed_uri_prefixes": ["path:", "github:yourorg/"],
  "issuers": {}
}
```

(`examples/external-refs-policy.json`; `path:` keeps embedded flakes working —
it authorizes the runtime's own rewrite of staged in-image sources, never a
node path named by an annotation.) Two rules save real pain:

- **Terminate prefixes at a boundary.** Matching is a literal string prefix:
  `github:yourorg` also authorizes `github:yourorg-evil/anything`; write
  `github:yourorg/`.
- **Pin anything you would redeploy.** A mutable ref is not a deployment
  identity — the same annotation can materialize different software tomorrow.
  Pin with an explicit revision or content hash, and remember the referenced
  flake's *own* unlocked inputs still resolve at evaluation time (its committed
  `flake.lock` is honored; pinning the top-level ref pins nothing else):

```yaml
# examples/pod-external-ref.yaml — the annotation, pinned
run.imageless.source: "github:yourorg/yourapp/8b7c95329f0a143adf971a0f27a4a0af8ddf9d5b"
run.imageless.output: "rootfs"
```

The node does not police pin forms — that is authoring tooling's job — and
production deployments should not ride this mode at all: they resolve
digest-addressed releases (`imageless.run/release-v1`), where identity is a
digest, not a ref. External references are the development and trusted-cluster
convenience, gated so a node operator can keep them off entirely.

## Embedding the library

The shim is a ~200-line consumer of the `imageless` crate. A runtime that owns
its `create` path can skip the interposer entirely:

```rust
use imageless::{prepare_bundle, remove_bundle_gc_roots, PrepareBundle};
use std::path::Path;

/// Call during OCI `create`, once the bundle is staged.
fn create(bundle: &Path) -> std::io::Result<()> {
    // Defaults: output `rootfs`, 300 s timeout, and the materializer picked by
    // the environment — the daemon socket when IMAGELESS_RESOLVER_SOCKET is
    // set, in-process under the IMAGELESS_POLICY file otherwise. Override the
    // struct's fields to make any of that explicit.
    let request = PrepareBundle::new(bundle.join("config.json"), bundle);
    // Ok(None) => not an imageless bundle; proceed unchanged.
    let _applied = prepare_bundle(&request)?;
    Ok(())
}

/// Call on any later failure, and at `delete`.
fn cleanup(bundle: &Path) -> std::io::Result<()> {
    remove_bundle_gc_roots(bundle)
}
```

This block is compiled as a doc-test of the `imageless` crate
(`crates/imageless/src/lib.rs`, `ReadmeDoctests`), so it cannot drift from the
API.

## Environment variables

Every knob below is read at runtime by `imageless-runc` (and, where noted, the
resolver daemon). The NixOS module sets the operator-facing ones for you; the
list is here so a hand-rolled deployment is not guesswork.

Operator-facing:

| Variable | Default | What it does |
|---|---|---|
| `IMAGELESS_RESOLVER_SOCKET` | unset | Non-empty selects the daemon: materialization goes over this UNIX socket instead of running in-process. Unset (or empty) is the daemonless default. |
| `IMAGELESS_POLICY` | `/etc/imageless/policy.json` | Node policy file for in-process materialization. An explicit path *must* load or create fails closed; the default path is allowed to be absent, in which case a fail-closed `cache_only` policy is synthesized. The file must be owned by the uid the runtime runs as and not group/world writable. Ignored when a resolver socket is configured. |
| `IMAGELESS_STORE_PROJECTION` | unset (`node`) | `closure` binds only the selected release's closure, one read-only mount per store path — the hardened backend. Any other value keeps the whole-node `/nix/store` bind. |
| `IMAGELESS_REALIZATION_TIMEOUT_SECONDS` | `300` | Wall-clock bound on materialization, 1–3600. Out-of-range or non-integer values fail the create. |
| `IMAGELESS_DEFAULT_OUTPUT` | `rootfs` | Flake output used when the image carries no `run.imageless.output` annotation. |
| `IMAGELESS_TELEMETRY_PATH` | unset | Append per-phase timing events (selection, policy verification, substitution, rewrite, delegate startup) as ndjson to this file. Unset disables telemetry; write failures are ignored and never fail a create. |
| `IMAGELESS_RUNC` | the stock `runc` baked in at build time (`runc` on PATH for an unbaked build) | The real OCI runtime to delegate to after the rewrite. Set this if you install `imageless-runc` *as* `runc`, so delegation cannot recurse through `PATH`. |
| `IMAGELESS_NIX` / `IMAGELESS_NIX_STORE` | baked at build time (`nix` / `nix-store` on PATH for an unbaked build) | The `nix` and `nix-store` binaries the materializer drives, in-process and in the daemon. `IMAGELESS_NIX_STORE` is also used for closure enumeration under `IMAGELESS_STORE_PROJECTION=closure`. |
| `IMAGELESS_SYSTEM` | `x86_64-linux` | Nix system double for the synthesized fail-closed default policy. A loaded policy file carries its own `system` and wins, so this only matters when no policy file exists. |

Internal / development only:

| Variable | Notes |
|---|---|
| `IMAGELESS_POLICY_JSON` | Node policy inline as JSON, ≤1 MiB, trusted because the daemon environment set it. Compiled in **only** under the `inline-policy` cargo feature (`nix build .#imageless-dev`) and absent from a production binary. Exists so the dev Docker harness can run a root daemon with no policy file to own — see `dev/docker/README.md`. Ignored when `IMAGELESS_RESOLVER_SOCKET` is set. |
| `IMAGELESS_DEV_PATH`, `IMAGELESS_DEV_SSL_CERT_FILE` | Build-time only (`option_env!`, baked by `nix/package.nix`): the sanitized `PATH` and CA bundle handed to the privilege-dropped development evaluator, which otherwise runs with a cleared environment. Setting them at runtime does nothing. |
| `IMAGELESS_DOCKER_*`, `IMAGELESS_SMOKE_*` | Inputs to the acceptance smoke harnesses only (`crates/imageless-runc/tests/`, `smoke/`). Not read by any shipped binary. |

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
