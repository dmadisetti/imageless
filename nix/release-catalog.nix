# Canonical, digest-addressed release-manifest catalog (SPEC.md §6). The
# target store paths deliberately have their Nix string context discarded:
# publishing metadata must not force one build machine to realise every
# architecture. Each target closure is built and pushed to its named cache by
# whatever release pipeline the issuer runs — any CI that can copy a closure
# and emit this JSON conforms.
{ lib
, writeTextDir
, rootfsTargets
, issuer
, releaseName
, cache ? "default"
, process ? null
}:

let
  manifest = {
    schema = "imageless.release.v1";
    inherit issuer;
    name = releaseName;
    targets = lib.mapAttrs
      (_: rootfs: {
        rootfs = builtins.unsafeDiscardStringContext (toString rootfs);
        inherit cache;
      } // lib.optionalAttrs (process != null) { inherit process; })
      rootfsTargets;
  };
  # `builtins.toJSON` emits compact JSON with lexicographically sorted object
  # keys, matching the resolver's canonical serializer.
  json = builtins.toJSON manifest;
  digest = builtins.hashString "sha256" json;
  reference = "${issuer}/${releaseName}@sha256:${digest}";
  catalog = writeTextDir "sha256/${digest}.json" json;
in
catalog.overrideAttrs (old: {
  passthru = (old.passthru or { }) // {
    inherit digest manifest reference;
  };
})
