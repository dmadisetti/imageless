# The imageless workspace: the runc interposer, the optional resolver daemon,
# and its unprivileged development evaluator, in one derivation so the NixOS
# module can point every consumer at a single package.
#
# Delegate and tool paths are baked at compile time (option_env!):
#  - IMAGELESS_RUNC: absolute stock runc, so installing imageless-runc as
#    `runc` can never recurse through PATH.
#  - IMAGELESS_NIX / IMAGELESS_NIX_STORE: the Nix the materializer drives —
#    both the in-process shim path and the resolver daemon.
#  - IMAGELESS_DEV_PATH / IMAGELESS_DEV_SSL_CERT_FILE: the sanitized
#    environment for the privilege-dropped development evaluator.
{ lib
, rustPlatform
, runc
, nix
, gitMinimal
, cacert
}:

rustPlatform.buildRustPackage {
  pname = "imageless";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).workspace.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../crates ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  IMAGELESS_RUNC = "${runc}/bin/runc";
  IMAGELESS_NIX = "${nix}/bin/nix";
  IMAGELESS_NIX_STORE = "${nix}/bin/nix-store";
  IMAGELESS_DEV_PATH = lib.makeBinPath [ nix gitMinimal ];
  IMAGELESS_DEV_SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  meta = {
    description = "Materialize a Nix flake carried in OCI image layers into the container rootfs at create time";
    homepage = "https://imageless.run";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
    mainProgram = "imageless-runc";
  };
}
