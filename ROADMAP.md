# Roadmap

The product is the shim and the spec. Everything below is staged so that
platform features have to *earn* their way in against a measured need, instead
of arriving alongside the core the way they did in the incubation repo.

## v0.1 — the extraction (in progress)

- [ ] Standalone workspace: own `Cargo.toml`/`Cargo.lock`, own flake +
      complete `flake.lock`, CI where **every** package is in `checks` and
      lint gates merge.
- [ ] Namespace cutover to the owned domain: `imageless.run/*` and
      `run.imageless.*`. Greenfield — no legacy names, no compat carve-outs;
      the code reads as if the incubation identifiers never existed.
- [ ] Zero-config embedded discovery: `etc/imageless/flake.nix` alone selects
      a container; `source.json` is retired (the flake is the metadata —
      alias nonstandard outputs to `#rootfs` in the flake itself).
- [ ] Deleted, not migrated: the containerd start/delete interposer (invalid
      under containerd 2.x pod-shim grouping), `imagelessctl`, the release
      publisher and `publish.json`.
- [ ] Acceptance gates as the only gates: the raw-Docker embedded-layer smoke
      and the CRI lifecycle VM test, the latter extended to prove
      embedded-layer bootstrap (flake present only in the image layer).
- [ ] Cowboy consumes this repo as a flake input; `cowboy-runtime` stays the
      first embedded-library consumer.
- [ ] **In-process materializer — no mandatory daemon.** A `Materializer`
      seam with a direct backend: the shim and library consumers read node
      policy (`/etc/imageless/policy.json` or `IMAGELESS_POLICY`) and perform
      the Nix work in-process. `imageless-resolver` becomes purely the optional
      hardened profile — node-wide concurrency caps, single-flight coalescing,
      privilege-separated evaluation — selected via
      `IMAGELESS_RESOLVER_SOCKET`.

## v0.2 — make the base product complete

- [ ] **External flake references as a first-class mode.** `run.imageless.source`
      may name a flake ref, gated by node policy
      (`eval_allowed_uri_prefixes`). Docs recommend enabling evaluation
      (`cache_only: false`) wherever embedded flakes are the point; the shipped
      default stays fail-closed (`cache_only: true`). Document pinning
      expectations — a mutable ref is not a deployment identity.
- [ ] **Compatibility matrix in CI.** The CRI gate against pinned containerd
      1.x and 2.x, the Docker smoke against a pinned Docker, plus rootless
      stock-runc — recorded per version so a containerd change cannot silently
      invalidate the claim.
- [ ] Publish the `imageless` crate once the `prepare_bundle`-shaped API
      settles.
- [ ] **`kubectl-imageless` plugin — zero Nix on the client.**
      `kubectl imageless run ./dir` packs the directory into a seed-image
      layer at `/etc/imageless/` (plain tar + manifest — no Nix, no Docker),
      pushes to a configured registry, and applies a Pod with
      `runtimeClassName: imageless`; `kubectl imageless run <flake-ref>`
      deploys the external-ref mode instead (annotation only, needs node
      policy allow-listing). Client-side enforcement of the spec's staging
      bounds, a loud warning on missing `flake.lock`, and a `doctor`
      subcommand that diagnoses unprepared clusters. Evaluation happens on
      the node, under node policy — the client stays Nix-free.
- [ ] Documented dev-cluster quickstart (kind/k3d) so the plugin has a
      five-minute path from nothing to a running flake.
- [ ] **No hand-typed digests.** Optional catalog name/channel index
      (`refs/<name>/<channel>` → digest, client-side only; nodes ignore it)
      plus `kubectl imageless pin <issuer>/<name>` and pin-on-apply, so
      release references are resolved to `@sha256:…` by tooling at authoring
      time. Examples never show `REPLACE_DIGEST` — they show the pin flow or
      Nix's `.reference` templating. The node contract stays digest-only.
- [ ] *(stretch)* **Shebang scripts as deployables.** `kubectl imageless run
      ./script.sh` where the entry file carries a `#!/usr/bin/env nix` +
      `#!nix shell ... --command <interpreter>` shebang: the packer parses
      the shebang and *generates* the embedded flake (buildEnv of the named
      packages + the script as entrypoint) into the seed image. Pure
      client-side desugaring — the spec and runtime never learn about
      shebangs. The generated flake pins a vendored nixpkgs rev + narHash
      and emits a real `flake.lock` (JSON, no Nix required), so shebang
      deploys are reproducible by default (`--unpinned` opts out). Accepts a
      single file or a directory whose entry file is shebang'd; supports the
      `nix shell` shebang grammar first, others only on demand.

## Spec v1 freeze

Freeze when, and not before:

- two independent conforming consumers (the shim and `cowboy-runtime`) pass
  both acceptance gates on the pinned compatibility matrix;
- the external-reference and release-profile sections have been exercised by a
  real deployment each;
- a deprecation policy for post-freeze changes is written down (pre-freeze
  there is none — v0.1 is greenfield and carries no compatibility surface).

## v0.3 — hardening the profiles that have users

The cache-only release profile has a live consumer (Cowboy's caged-agent
nodes), so its hardening is committed work, paced by that deployment:

- [ ] Threat-model document: compromised workloads, malicious caches, staging
      abuse, reboot cleanup; move the development evaluator into a dedicated
      service cgroup rather than UID-wide rlimits.
- [ ] Detached signatures over canonical manifest bytes (minisign-style), with
      node-owned issuer keys, rotation, revocation, and compromised-catalog
      recovery. Digest integrity exists today; authenticity does not.
- [ ] Private/authorized cache access without distributing cache credentials to
      workloads.
- [ ] Enough recorded evaluation/build input to reproduce a release after cache
      eviction.

## Measured before built

These return only in response to numbers from the timing telemetry, and as a
separate operations project rather than core:

- prewarm / readiness / desired-state reconciliation (the deleted
  `imagelessctl` functionality);
- closure byte-cost inspection and prepared-node labels;
- cache-affinity scheduling.

## Ideas (explicitly not on the roadmap)

Prior-art notes live with the incubation history; none of this defines the
runtime contract:

- a release *publisher* product (any CI that copies a closure and emits the
  manifest JSON conforms — that is the point of the manifest);
- Garnix-style recursive FOD verification, incremental build retention,
  SBOM/SLSA emission, hardware-attested builders;
- a containerd TTRPC `Create` wrapper adapter — only worth revisiting if the
  runc seam ever proves insufficient, which no current containerd generation
  suggests;
- automated multi-architecture placeholder publishing;
- a cluster-installer DaemonSet (drop the shim + containerd config onto nodes,
  gVisor/Kata style) — high adoption value but real blast radius (containerd
  restarts on live nodes); revisit once the kubectl plugin has users asking
  for it.

## Non-goals

Unchanged from incubation, and load-bearing:

- no reimplementation of containerd's ttrpc task service;
- no general CI system or database on nodes;
- no compilation, FOD rebuilding, or provenance generation during container
  start;
- no mutable remote flake refs as a *production* deployment identity;
- no policy hidden inside placeholder OCI images.
