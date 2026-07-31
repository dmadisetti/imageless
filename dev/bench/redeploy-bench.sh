#!/usr/bin/env bash
# Measure the node-side cost of redeploying a Rust workload through imageless.
#
# The question this answers is "I changed one line of Rust; how long until it is
# running again, and where did that time go?". It drives the real create path —
# `imageless-runc create` against a real OCI bundle, materializing a real
# embedded flake with real Nix — in the daemonless in-process mode, so no
# resolver daemon, container runtime, or cluster is required.
#
# Two seed-flake shapes are measured against three scenarios each:
#
#   single  one derivation builds the whole Cargo workspace
#   split   a dependency derivation builds the slow crate, and the application
#           derivation reuses its `target/` — the crane/cargoArtifacts pattern
#
#   cold     nothing in the store yet
#   restart  byte-identical source, materialized again (pod restart, scale-up)
#   edit     one line changed in the application crate
#
# `split`/`edit` is the incremental-build case. `single`/`edit` is what a naive
# seed flake costs on every redeploy.
#
# Each cell is the FASTEST of --samples repetitions, and carries the load
# average at the moment that sample started. Contention only ever adds time —
# nothing about a busy host makes a build finish early — so the minimum is the
# closest estimate of the real cost, and a cell whose load column is not near
# idle is still an overestimate. A loaded host can move these numbers by an
# order of magnitude, which is more than any effect being measured here.
#
# Usage: dev/bench/redeploy-bench.sh [--modules N] [--functions N]
#                                    [--samples N] [--keep]
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
MODULES=24
FUNCTIONS=30
SAMPLES=3
KEEP=0

while [[ $# -gt 0 ]]; do
  case $1 in
    --modules) MODULES=$2; shift 2 ;;
    --functions) FUNCTIONS=$2; shift 2 ;;
    --samples) SAMPLES=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    # The leading comment block, minus the shebang — printed by walking it
    # rather than by a line range, which silently truncates the moment the
    # block grows.
    -h|--help)
      awk '/^#!/ { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

for tool in nix jq cargo; do
  command -v "$tool" >/dev/null || { echo "$tool is required; run inside \`nix develop\`" >&2; exit 1; }
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/imageless-redeploy-bench.XXXXXX")
cleanup() {
  # The GC roots live in the bundles; dropping them is what makes a rerun cold
  # again for anything the store has not already pinned elsewhere.
  [[ $KEEP == 1 ]] && { echo "kept: $WORK" >&2; return; }
  chmod -R u+w "$WORK" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$WORK"/{bundles,telemetry,logs,runc-root}

# Every run must start genuinely cold, and a store that already holds the last
# run's artifacts would report a warm build as a cold one. Salting the generated
# source gives this run derivations nothing has ever built.
SALT=$(date +%s%N)
SYSTEM=$(nix eval --raw --impure --expr 'builtins.currentSystem')
NIXPKGS=$(nix flake archive --json --no-write-lock-file "$REPO" | jq -r '.inputs.nixpkgs.path')
[[ -n $NIXPKGS && -d $NIXPKGS ]] || { echo "could not locate the locked nixpkgs" >&2; exit 1; }

echo "==> building imageless-runc" >&2
cargo build --release -p imageless-runc --manifest-path "$REPO/Cargo.toml" >&2
RUNC=$REPO/target/release/imageless-runc

# The shim delegates to a real OCI runtime after it has rewritten the bundle.
# Benchmarking materialization means stopping there: the delegate is a stub, so
# every microsecond reported below is imageless's own.
DELEGATE=$WORK/delegate-runc
printf '#!/bin/sh\nexit 0\n' > "$DELEGATE"
chmod 755 "$DELEGATE"

# Development sources are evaluated from a `path:` installable; the empty issuer
# set is what makes this a development-only policy.
POLICY=$WORK/policy.json
jq -n --arg system "$SYSTEM" \
  '{system: $system, cache_only: false, eval_allowed_uri_prefixes: ["path:"], issuers: {}}' \
  > "$POLICY"
chmod 600 "$POLICY"

# ---------------------------------------------------------------- the workload

generate_workspace() {
  local root=$1
  mkdir -p "$root/slowdep/src" "$root/app/src"

  cat > "$root/Cargo.toml" <<'EOF'
[workspace]
members = ["slowdep", "app"]
resolver = "2"
EOF

  cat > "$root/slowdep/Cargo.toml" <<'EOF'
[package]
name = "slowdep"
version = "0.1.0"
edition = "2021"
EOF

  cat > "$root/app/Cargo.toml" <<'EOF'
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
slowdep = { path = "../slowdep" }
EOF

  # Generic code instantiated over several concrete types: monomorphization is
  # what makes a real Rust dependency tree expensive to compile, and it is the
  # cost a per-crate split is supposed to stop paying.
  local module function
  : > "$root/slowdep/src/lib.rs"
  for ((module = 0; module < MODULES; module++)); do
    printf 'pub mod m%s;\n' "$module" >> "$root/slowdep/src/lib.rs"
    {
      printf 'use std::collections::BTreeMap;\n\n'
      printf 'pub const SALT: u64 = %s;\n\n' "$((SALT % 1000000007))"
      printf 'pub trait Fold<T> { fn fold_in(&self, acc: u64, item: T) -> u64; }\n\n'
      for ((function = 0; function < FUNCTIONS; function++)); do
        cat <<EOF
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct S${function}<T>(pub T);

impl<T: Copy + Into<u64>> Fold<T> for S${function}<T> {
    fn fold_in(&self, acc: u64, item: T) -> u64 {
        let mut table: BTreeMap<u64, u64> = BTreeMap::new();
        let base: u64 = self.0.into();
        for step in 0..8u64 {
            table.insert(step ^ base, acc.wrapping_mul(step + $((function + 1))).wrapping_add(item.into()));
        }
        table.values().fold(acc, |a, b| a.rotate_left(3) ^ b.wrapping_add(SALT + $module))
    }
}

pub fn go${function}(seed: u64) -> u64 {
    let a = S${function}(seed as u8).fold_in(seed, 3u8);
    let b = S${function}(seed as u16).fold_in(a, 5u16);
    let c = S${function}(seed as u32).fold_in(b, 7u32);
    S${function}(c).fold_in(c, 11u64)
}
EOF
      done
      printf 'pub fn total(seed: u64) -> u64 {\n    let mut acc = seed;\n'
      for ((function = 0; function < FUNCTIONS; function++)); do
        printf '    acc = go%s(acc);\n' "$function"
      done
      printf '    acc\n}\n'
    } > "$root/slowdep/src/m$module.rs"
  done

  write_app "$root" 1
}

# The one line a developer edits between redeploys.
write_app() {
  cat > "$1/app/src/main.rs" <<EOF
fn main() {
    println!("imageless-bench revision $2: {}", slowdep::m0::total($2));
}
EOF
}

# ------------------------------------------------------------- the seed flakes

write_flake() {
  local root=$1 variant=$2
  local header="  inputs.nixpkgs.url = \"path:$NIXPKGS\";"

  if [[ $variant == single ]]; then
    cat > "$root/flake.nix" <<EOF
{
$header
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages."$SYSTEM";
      # One derivation for the whole workspace: any source change anywhere
      # changes this derivation's input, so every redeploy rebuilds every crate.
      app = pkgs.stdenv.mkDerivation {
        name = "bench-app";
        src = ./.;
        nativeBuildInputs = [ pkgs.cargo pkgs.rustc ];
        buildPhase = ''
          export CARGO_HOME=\$TMPDIR/cargo
          cargo build --release --offline
        '';
        installPhase = ''
          mkdir -p \$out/bin
          cp target/release/app \$out/bin/app
        '';
      };
    in
    {
      rootfs = pkgs.runCommand "bench-rootfs" { } ''
        mkdir -p \$out/bin \$out/nix/store \$out/tmp
        ln -s \${app}/bin/app \$out/bin/app
      '';
    };
}
EOF
  else
    cat > "$root/flake.nix" <<EOF
{
$header
  outputs = { self, nixpkgs }:
    let
      pkgs = nixpkgs.legacyPackages."$SYSTEM";
      rust = [ pkgs.cargo pkgs.rustc ];
      # Cargo decides a unit is fresh by stat-ing the paths recorded when it was
      # built, so the dependency build and the application build must see the
      # workspace at the same absolute path — hence \$NIX_BUILD_TOP/ws in both
      # rather than each derivation's own unpacked source root. This needs a
      # sandboxed build to hold; without one NIX_BUILD_TOP varies per build and
      # the split degrades to a full rebuild rather than to a wrong answer.

      # Only the manifests and the slow crate, with the application crate
      # stubbed out. Editing application code cannot change this derivation's
      # inputs, so the expensive build behind it stays a cache hit.
      depsSrc = pkgs.runCommand "bench-deps-src" { } ''
        mkdir -p \$out/app/src
        cp \${builtins.path { path = ./Cargo.toml; name = "workspace-manifest"; }} \$out/Cargo.toml
        cp -r \${builtins.path { path = ./slowdep; name = "slowdep"; }} \$out/slowdep
        cp \${builtins.path { path = ./app/Cargo.toml; name = "app-manifest"; }} \$out/app/Cargo.toml
        printf 'fn main() {}\n' > \$out/app/src/main.rs
      '';

      deps = pkgs.stdenv.mkDerivation {
        name = "bench-deps";
        src = depsSrc;
        nativeBuildInputs = rust;
        buildPhase = ''
          export CARGO_HOME=\$TMPDIR/cargo
          mkdir -p "\$NIX_BUILD_TOP/ws"
          cp -r ./. "\$NIX_BUILD_TOP/ws"
          chmod -R u+w "\$NIX_BUILD_TOP/ws"
          cd "\$NIX_BUILD_TOP/ws"
          # cp does not preserve timestamps, so the sources would carry this
          # build's wall clock and the next derivation's copy would carry its
          # own. Cargo compares those mtimes against the ones recorded in the
          # fingerprint; pinning them is what makes the two builds agree.
          find . -exec touch -h -d @1 {} +
          cargo build --release --offline -p slowdep
        '';
        installPhase = ''
          mkdir -p \$out
          cp -r "\$NIX_BUILD_TOP/ws/target" \$out/target
        '';
      };

      app = pkgs.stdenv.mkDerivation {
        name = "bench-app";
        src = ./.;
        nativeBuildInputs = rust;
        buildPhase = ''
          export CARGO_HOME=\$TMPDIR/cargo
          mkdir -p "\$NIX_BUILD_TOP/ws"
          cp -r ./. "\$NIX_BUILD_TOP/ws"
          chmod -R u+w "\$NIX_BUILD_TOP/ws"
          cd "\$NIX_BUILD_TOP/ws"
          # Pinned to the same mtime the dependency build saw, so cargo believes
          # the artifacts copied in below. The copy comes after, and lands with
          # a current mtime: newer than every source, which is exactly the
          # relation cargo reads as "fresh".
          find . -exec touch -h -d @1 {} +
          cp -r \${deps}/target ./target
          chmod -R u+w ./target
          # The stub application binary is in that target/, and cargo would
          # otherwise call the real crate fresh and install the stub. Dropping
          # its artifacts is what forces the one crate that changed to be the
          # one crate rebuilt.
          rm -rf target/release/app target/release/app.d
          rm -rf target/release/deps/app-* target/release/.fingerprint/app-*
          cargo build --release --offline -p app
        '';
        installPhase = ''
          mkdir -p \$out/bin
          cp "\$NIX_BUILD_TOP/ws/target/release/app" \$out/bin/app
        '';
      };
    in
    {
      rootfs = pkgs.runCommand "bench-rootfs" { } ''
        mkdir -p \$out/bin \$out/nix/store \$out/tmp
        ln -s \${app}/bin/app \$out/bin/app
      '';
    };
}
EOF
  fi

  # Locking here rather than at create time: a development source is staged
  # read-only, and an unlocked flake would send Nix looking for a lock it cannot
  # write. `path:` inputs resolve offline.
  nix flake lock "$root" >/dev/null 2>&1
}

# ------------------------------------------------------------------ the driver

write_config() {
  local bundle=$1
  jq -n '{
    ociVersion: "1.0.2",
    root: { path: "rootfs", readonly: true },
    process: { args: ["/bin/app"], cwd: "/", env: ["PATH=/bin"] },
    mounts: [],
    annotations: {
      "run.imageless.source": "/source",
      "run.imageless.output": "rootfs"
    }
  }' > "$bundle/config.json"
}

RESULTS=$WORK/results.jsonl
: > "$RESULTS"

# What one edit costs before any of the workload's own code is compiled.
#
# Nix caches evaluation against the flake's fingerprint, which covers every file
# in the tree, so changing one line of Rust misses the cache and nixpkgs is
# evaluated from scratch. That term is a floor: it is paid per edit no matter
# how well the seed flake splits its derivations, and on a small workload it can
# be most of the redeploy.
eval_floor_ms() {
  local probe=$WORK/eval-floor
  rm -rf "$probe"
  mkdir -p "$probe"
  cat > "$probe/flake.nix" <<EOF
{
  inputs.nixpkgs.url = "path:$NIXPKGS";
  outputs = { self, nixpkgs }: {
    rootfs = nixpkgs.legacyPackages."$SYSTEM".runCommand "probe" { } "mkdir -p \$out";
  };
}
EOF
  nix flake lock "$probe" >/dev/null 2>&1
  nix eval --raw "$probe#rootfs.drvPath" >/dev/null 2>&1

  # A comment is enough: the fingerprint covers content, not meaning.
  printf '# %s\n' "$SALT" >> "$probe/flake.nix"
  nix flake lock "$probe" >/dev/null 2>&1
  local started finished
  started=$(date +%s%N)
  nix eval --raw "$probe#rootfs.drvPath" >/dev/null 2>&1
  finished=$(date +%s%N)
  echo $(( (finished - started) / 1000000 ))
}

measure() {
  local variant=$1 scenario=$2 source=$3
  local tag=$variant-$scenario-$SAMPLE
  local bundle=$WORK/bundles/$tag
  local telemetry=$WORK/telemetry/$tag.jsonl

  rm -rf "$bundle"
  mkdir -p "$bundle/rootfs/source"
  # Staging is a tree copy, so a cold page cache would be charged to whichever
  # variant happens to run first rather than to anything imageless does.
  find "$source" -type f -exec cat {} + > /dev/null
  cp -a "$source/." "$bundle/rootfs/source/"
  write_config "$bundle"
  : > "$telemetry"
  chmod 600 "$telemetry"

  printf '==> %-14s ' "$tag" >&2
  local started finished status=ok load
  load=$(cut -d' ' -f1 /proc/loadavg)
  started=$(date +%s%N)
  if ! env IMAGELESS_POLICY="$POLICY" \
           IMAGELESS_RUNC="$DELEGATE" \
           IMAGELESS_TELEMETRY_PATH="$telemetry" \
           IMAGELESS_REALIZATION_TIMEOUT_SECONDS=1800 \
       "$RUNC" --root "$WORK/runc-root" create \
         --bundle "$bundle" --pid-file "$WORK/pid" "bench-$tag" \
         > "$WORK/logs/$tag.log" 2>&1; then
    status=failed
  fi
  finished=$(date +%s%N)

  if [[ $status == failed ]]; then
    echo "FAILED" >&2
    sed -n '1,20p' "$WORK/logs/$tag.log" >&2
    exit 1
  fi

  local total_ms=$(( (finished - started) / 1000000 ))
  echo "${total_ms} ms  (load ${load})" >&2
  jq -s --arg variant "$variant" --arg scenario "$scenario" --argjson total_ms "$total_ms" \
        --argjson sample "$SAMPLE" --argjson load "$load" '
    (map({ (.stage): .duration_us }) | add) as $stage |
    {
      variant: $variant,
      scenario: $scenario,
      sample: $sample,
      load: $load,
      total_ms: $total_ms,
      staging_ms: (($stage.staging // 0) / 1000 | floor),
      evaluation_ms: (($stage.evaluation // 0) / 1000 | floor),
      root_registration_ms: (($stage.root_registration // 0) / 1000 | floor),
      selection_ms: (($stage.selection // 0) / 1000 | floor),
      rewrite_ms: (($stage.rewrite // 0) / 1000 | floor)
    }' "$telemetry" >> "$RESULTS"
}

echo "==> host load average: $(cut -d' ' -f1-3 /proc/loadavg) across $(nproc) cpus" >&2
EVAL_FLOOR=$(eval_floor_ms)
echo "==> nix evaluation floor: ${EVAL_FLOOR} ms" >&2

revision=1
for variant in single split; do
  source=$WORK/src-$variant
  rm -rf "$source"
  mkdir -p "$source"
  generate_workspace "$source"
  write_flake "$source" "$variant"

  # Cold happens once by definition; the store remembers.
  SAMPLE=1 measure "$variant" cold "$source"
  for ((SAMPLE = 1; SAMPLE <= SAMPLES; SAMPLE++)); do
    measure "$variant" restart "$source"
  done
  for ((SAMPLE = 1; SAMPLE <= SAMPLES; SAMPLE++)); do
    # Each sample is a genuinely new edit — reusing one revision would measure
    # a restart after the first pass.
    revision=$((revision + 1))
    write_app "$source" "$revision"
    measure "$variant" edit "$source"
  done
done

# ------------------------------------------------------------------ the report

echo
echo "fastest of $SAMPLES samples (cold is measured once), milliseconds:"
echo
printf '%-8s %-8s %2s %9s %8s %11s %7s %8s %6s\n' \
  variant scenario n total staging evaluation root rewrite load
printf '%-8s %-8s %2s %9s %8s %11s %7s %8s %6s\n' \
  -------- -------- -- --------- -------- ----------- ------- -------- ------
jq -s -r '
  # The fastest sample, not the median. Contention only ever adds time — there
  # is no mechanism by which a busy host makes a build finish early — so under
  # uncontrolled load the minimum is the closest estimate of the real cost and
  # every other sample is that plus an unknown amount of someone else. The load
  # column is the load average when that fastest sample started; if it is not
  # near idle, the number beside it is still an overestimate.
  def best: min_by(.total_ms);
  group_by(.variant + "/" + .scenario)
  | map(best + { n: length })
  | sort_by(.variant, {cold: 0, restart: 1, edit: 2}[.scenario])
  | .[] | [.variant, .scenario, .n, .total_ms, .staging_ms, .evaluation_ms,
           .root_registration_ms, .rewrite_ms, .load] | @tsv
' "$RESULTS" \
  | while IFS=$'\t' read -r variant scenario n total staging evaluation root rewrite load; do
      printf '%-8s %-8s %2s %9s %8s %11s %7s %8s %6s\n' \
        "$variant" "$scenario" "$n" "$total" "$staging" "$evaluation" "$root" "$rewrite" "$load"
    done

echo
jq -s -r --argjson floor "$EVAL_FLOOR" '
  def pick($v; $s): map(select(.variant == $v and .scenario == $s) | .total_ms) | min;
  (pick("single"; "edit")) as $single |
  (pick("split"; "edit")) as $split |
  (map(select(.scenario == "restart") | .total_ms) | min) as $restart |
  "unchanged redeploy:                \($restart) ms",
  "edit -> running, one derivation:   \($single) ms",
  "edit -> running, split derivation: \($split) ms  (\((($single * 10 / ($split + 1)) | floor) / 10)x)",
  "",
  "nix evaluation floor:              \($floor) ms",
  "  Paid on every edit, before any of the workload compiles, because changing",
  "  one line changes the flake fingerprint and nixpkgs is evaluated again.",
  "  Splitting derivations removes compilation from a redeploy, not this.",
  "  Compilation actually avoided by the split: \($single - $split) ms."
' "$RESULTS"

if [[ $KEEP == 1 ]]; then
  echo
  echo "raw measurements: $RESULTS"
fi
