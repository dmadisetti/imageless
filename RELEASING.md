# Releasing

One number moves the whole workspace. `[workspace.package] version` in
`Cargo.toml` is the only place it is written: all four crates inherit it with
`version.workspace = true`, and `nix/package.nix` reads the same field out of
the same file, so a Nix build and a Cargo build cannot disagree about what they
are.

## What 0.x means here

The `0.` is a claim, not a formality: the library API is not frozen, and
neither is the spec it implements. Both may change in a minor release until the
criteria under "Spec v1 freeze" in [ROADMAP.md](ROADMAP.md) are met. Until
then there is no deprecation policy to violate, because there is no
compatibility surface anyone was promised.

- **Minor** (`0.1.0` → `0.2.0`) — a change a consumer has to read about. A
  changed signature or behaviour in the `imageless` library, a change to the
  annotation set or the on-disk formats in SPEC.md, or a new node-policy field
  that an existing policy file must grow.
- **Patch** (`0.1.0` → `0.1.1`) — everything else. Fixes, hardening,
  documentation, and additions that no existing caller has to notice.

Post-freeze this becomes ordinary semver and this section gets rewritten. That
is the point of saying it out loud now.

## Why the unpublished crates version in lockstep too

Only `imageless` is published; `imageless-runc`, `imageless-resolver` and
`kubectl-imageless` all carry `publish = false`. They still share the number,
because the question people actually ask is "which shim goes with which
library" — and under lockstep the answer is "the one with the same version"
rather than a compatibility matrix nobody maintains. It costs a version bump on
a crate that did not change, which is cheaper than the matrix.

## The homepage field is deliberately absent

`[workspace.package]` carries `repository` but no `homepage`. `imageless.run`
is registered and delegated but publishes no address record, and a crates.io
version renders the metadata it was uploaded with forever — a Homepage button
that fails to resolve cannot be fixed in the next patch, only in the next
version. When the domain serves something, add the line back; it is one line,
and the next minor release picks it up. This has nothing to do with the
`imageless.run/*` annotation namespace, which is a name and not a URL.

## Cutting a release

1. Edit `[workspace.package] version` in `Cargo.toml`. Nothing else records a
   version — do not hand-edit the crates or `nix/package.nix`.
2. `nix flake check` and `nix build .#checks.x86_64-linux.lint`. The lint gate
   packages the crate and asserts the tarball carries `README.md` and `LICENSE`
   and that the README has no relative URLs, because both are things that fail
   silently and only on the published page.
3. Commit. This comes before the dry run, not after: `cargo package` refuses a
   dirty git tree, so a dry run over an uncommitted version bump fails on the
   working tree rather than telling you anything about the release.
4. `cargo publish -p imageless --dry-run`, from inside `nix develop`. It
   packages the crate and compiles the packaged tree, and needs no token.
5. Tag `v<version>`, push the branch and the tag.
6. `cargo publish -p imageless`.

Step 6 is irreversible. A yank hides a version from resolution; it does not
free the number and does not remove the bytes. The crates.io page for a version
— README, rendered links, metadata — is whatever it was at upload, forever, so
everything in step 2 has to be right beforehand rather than fixed in the next
patch.
