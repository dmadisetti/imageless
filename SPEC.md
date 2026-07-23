# The imageless contract — v1 (draft)

This document specifies how an OCI container image carries a Nix flake in its
layers and how a conforming runtime materializes that flake into the container's
root filesystem at create time. `imageless-runc` is the reference
implementation; any OCI runtime may implement this contract directly or by
linking the `imageless` library.

Status: draft. Identifiers (`run.imageless.*`, `imageless.run/*`,
`imageless.release.v1`) are stable in the reference implementation but the
spec may still renumber or renamespace them before v1 is frozen.

## 1. Terms

- **Image** — an ordinary OCI image. Nothing in this contract changes how
  images are built, pushed, pulled, or admitted.
- **Bundle** — the OCI runtime bundle (`config.json` + `rootfs/`) a runtime
  receives at `create`.
- **Materialization** — realizing a Nix installable to exactly one store path
  that becomes the container's root filesystem.
- **Conforming runtime** — the component that interposes on per-container
  `create`: the `imageless-runc` shim, or a runtime linking the library.
- **Materializer** — the component that performs the Nix work. It may run
  in-process inside the conforming runtime or as the optional
  `imageless-resolver` node daemon; the contract is identical either way.

## 2. The embedded flake (core contract)

An image opts in by carrying a flake at a conventional path in its layers:

```
rootfs/
└── etc/imageless/
    ├── flake.nix          # required for the zero-config path
    ├── flake.lock         # optional, strongly recommended
    └── ...                # any source files the flake references
```

### 2.1 Zero-config default

If `etc/imageless/flake.nix` exists in the bundle rootfs as a regular file, the
container is selected with:

- source: `/etc/imageless`
- output: `rootfs`

The installable is the canonical equivalent of
`path:<bundle-rootfs>/etc/imageless#rootfs`. The flake output must evaluate to
a derivation whose single output path is a usable root filesystem.

Note: materializing an embedded flake is node-side evaluation, which the
reference materializer ships **disabled** (`cache_only: true`, §6). Deployment
documentation and examples should show the opt-in (`cache_only: false`) —
enabling it is the expected configuration wherever embedded flakes are the
point — but the fail-closed default is deliberate and stays.

### 2.2 The flake is the metadata

There is no sidecar metadata file. The flake is code, so every image-side
degree of freedom is expressed *in the flake*: an image whose "real" output
lives elsewhere aliases it to the conventional name —

```nix
# flake.nix
outputs = { self, ... }: {
  rootfs = self.packages.x86_64-linux.my-actual-thing;
};
```

Deployer-side (not image-side) overrides are what annotations are for (§3).

### 2.3 Source staging

Before evaluation, the runtime stages a copy of the source tree out of the
bundle rootfs. The staged tree is bounded: at most 16 MiB and 4096 entries,
regular files and directories only (symlinks are rejected). The staged copy is
what the materializer evaluates; the container never controls paths outside its
own rootfs.

## 3. Annotations (highest precedence)

OCI annotations override the zero-config default. Annotation values are
limited to 4096 bytes; selectors to 1024.

Development / source-evaluation namespace:

| Annotation | Meaning |
|---|---|
| `run.imageless.source` | Absolute in-rootfs path (staged and evaluated as `path:<rootfs><source>`) or a flake reference. |
| `run.imageless.output` | Flake output attribute; defaults to the runtime's configured default (`rootfs`). |
| `run.imageless.containers` | Selector list: only named containers are materialized. |
| `run.imageless.skip-containers` | Selector list: named containers are passed through. |

A `source` that is not an absolute in-image path is an **external flake
reference**. External references are a supported mode, but the node must opt in
through materializer policy (`eval_allowed_uri_prefixes`); a node that has not
allow-listed the reference's prefix fails the request. Pin external references
(locked inputs, explicit revisions) for anything beyond development — a mutable
ref is not a deployment identity.

Release namespace (cache-only production, §6):

| Annotation | Meaning |
|---|---|
| `imageless.run/release-v1` | A digest-addressed release reference. Mutually exclusive with `run.imageless.source`. |
| `imageless.run/containers-v1` | Release-mode container selector. |
| `imageless.run/skip-containers-v1` | Release-mode skip selector. |

Kubernetes handling:

- A container annotated `io.kubernetes.cri.container-type: sandbox` is **never**
  materialized. The pause sandbox always runs its ordinary rootfs.
- `io.kubernetes.cri.container-name` participates in selector matching.
- Under containerd, the runtime handler must allow-list these annotation
  prefixes (`pod_annotations` / `container_annotations`) or they never reach the
  OCI spec.

## 4. Runtime obligations

A conforming runtime, at per-container `create`:

1. **Selects or passes through.** A bundle with no embedded flake and no
   annotations proceeds unchanged. Passthrough must be a no-op: no
   materializer contact, no bundle mutation.
2. **Validates fail-closed.** Malformed metadata, invalid selectors, oversized
   values, or contradictory annotations (e.g. release + source) fail creation.
   The runtime must never delegate a partially rewritten spec.
3. **Materializes boundedly.** Materialization has a deadline (reference
   default 300 s, configurable 1–3600 s). On expiry the materializer's whole
   process tree is killed. Exactly one realized store path is accepted;
   ambiguous output is an error.
4. **Rewrites atomically.** `root.path` in `config.json` is replaced via
   write-to-temp + rename, preserving file mode and fsyncing the file and its
   parent directory. Unrelated OCI fields are preserved byte-for-byte where not
   rewritten. Process metadata is only applied when the release manifest
   explicitly requests it.
5. **Projects the store.** The realized closure must be visible to the
   container. Reference modes: `node` (bind the node's `/nix/store` read-only)
   or `closure` (read-only bind mounts scoped to the closure of the realized
   root, computed by the materializer).
6. **Holds GC roots for the container's lifetime.** Materialization registers
   Nix GC roots tied to the bundle (`.imageless-rootfs-gcroot`,
   `.imageless-store-gcroots/`). Roots are released when creation fails, the
   delegate exits unsuccessfully, or the container is deleted. A live container
   must survive `nix-collect-garbage`; a deleted container must not pin its
   realization.

## 5. Interposition seam

The contract binds at the point that runs **once per container**: the OCI
runtime's `create`.

- Generic nodes: `imageless-runc` interposes the runc CLI (`create` triggers
  resolution; every verb delegates to the real runc). Under containerd —
  including 2.x pod-shim grouping, where all of a pod's containers share one
  shim process — this seam still fires per container, because the shim execs
  the OCI runtime binary per `runc create`.
- Embedded runtimes: link the `imageless` library and call it during `create`.

A containerd runtime-v2 `start`/`delete` binary interposer is **not** a
conforming implementation: under containerd 2.x it observes only the pod
sandbox, not each workload container.

## 6. Release profile (optional, cache-only)

Production nodes may refuse all evaluation (`cache_only: true`, the default
policy) and resolve only digest-addressed releases:

- `imageless.run/release-v1` carries a reference resolved against
  node-configured **issuer catalogs** (local directory or HTTPS), fetching an
  `imageless.release.v1` manifest addressed as `sha256/<digest>.json`, at most
  64 KiB, validated against its digest (canonical JSON).
- Node policy allow-lists issuers, release-name patterns, and the substituters
  (with public keys) a release may be fetched from.
- The manifest maps target systems to store paths and may carry explicit
  process metadata.

Digest references are for machines, not fingers. A catalog MAY additionally
publish a name/channel index (`refs/<name>/<channel>` → digest) so that
*client-side* tooling can resolve a human-friendly name to a pinned reference
at authoring or apply time. Nodes MUST ignore the index: the annotation a node
accepts is always digest-addressed, and node-side resolution of mutable
pointers is non-conforming.

The publisher that produces manifests is out of scope for this spec; any CI
that can copy a Nix closure to a cache and emit the manifest JSON conforms.

## 7. Conformance

An implementation conforms when it passes the acceptance gates in this
repository:

1. **Raw Docker embedded-layer bootstrap** — a seed image whose layer contains
   only the flake and its inputs (not the executable that produces the expected
   response) serves the expected response after materialization; an ordinary
   image passes through unchanged.
2. **Kubernetes CRI lifecycle** — a real containerd node with a
   `RuntimeClass`, proving sandbox passthrough, per-container selection,
   recreate, GC-while-running, delete-and-collect, and reboot recovery.
