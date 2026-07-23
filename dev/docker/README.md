# Dev Docker daemon for the imageless runtime

Run a real `docker run --runtime imageless` by hand, isolated from any system
Docker (its own socket + data-root under `/tmp/imageless-docker-dev`). This is
the local counterpart of the hermetic `docker-embedded-smoke` VM gate — no KVM
required, just Docker. Nothing is installed to `/etc` or `/run`, and nothing
lands on disk outside this tree: the policy rides the daemon's environment.

`daemon.json` names the runtime `imageless-runc` (no path), so the daemon
resolves it from its `PATH` — put the build's `bin` there when you start it.
The policy is handed to the runtime inline through `IMAGELESS_POLICY_JSON`
(the body of `policy.json` in this directory): `cache_only: false` so the node
may evaluate an embedded flake, and only `path:` sources are allowed (the
embedded `etc/imageless/flake.nix`).

## Why the inline policy — and why the dev build

A production `imageless-runc` reads its node policy only from a **file**, gated
by an ownership check (the file must be owned by the uid the runtime runs as and
not be group/world writable). Under a **root** daemon that means the runtime runs
as root, so an in-tree, user-owned `policy.json` is rejected — and copying a
root-owned policy into `/etc` or `/root` is exactly the out-of-tree impurity we
want to avoid.

`.#imageless-dev` is the same workspace built with the `inline-policy` cargo
feature. It additionally accepts the policy inline via `IMAGELESS_POLICY_JSON`,
whose trust anchor is the daemon environment (which you, a sudoer, set) rather
than a file's ownership. The feature is compiled out of `.#imageless`, so a
production node can never read policy from the environment — do not deploy
`imageless-dev` to a shared host.

## Start the daemon

```sh
nix build .#imageless-dev   # provides ./result/bin/imageless-runc (inline-policy)

sudo env "PATH=$(readlink -f result)/bin:$PATH" \
     "IMAGELESS_POLICY_JSON=$(cat dev/docker/policy.json)" \
     dockerd --config-file dev/docker/daemon.json
```

Root or rootless, this is the same command — the runtime reads the policy from
its environment, so there is no file to own and no ownership check to satisfy.
`policy.json` here is just the canonical body to paste, not a file the runtime
must own.

## Run the nginx embedded-layer test against it

In another shell (points `docker` at the dev socket):

```sh
export DOCKER_HOST=unix:///tmp/imageless-docker-dev/docker.sock
IMAGELESS_DOCKER_IMAGE_ARCHIVE="$(nix build .#nginx-embedded-image --print-out-paths)" \
IMAGELESS_DOCKER_IMAGE=localhost/imageless-nginx-embedded:e2e \
IMAGELESS_DOCKER_EXPECTED=imageless-nginx-ok \
IMAGELESS_DOCKER_NETWORK=host \
IMAGELESS_DOCKER_RUN_ARGS="--tmpfs /tmp" \
  nix run .#docker-embedded-scenario
```

Expected output: `imageless-nginx-ok`. The 2 KB image ships only
`flake.nix` + `flake.lock`; nginx is materialized on the node from its own
cache at container-create, and `/nix/store` is bound read-only so the sparse
root's loader resolves. `--tmpfs /tmp` gives nginx a writable pid/temp dir
under the readonly materialized root.

The busybox counterpart uses the same harness:

```sh
IMAGELESS_DOCKER_IMAGE_ARCHIVE="$(nix build .#docker-embedded-image --print-out-paths)" \
IMAGELESS_DOCKER_NETWORK=none \
  nix run .#docker-embedded-scenario
```
