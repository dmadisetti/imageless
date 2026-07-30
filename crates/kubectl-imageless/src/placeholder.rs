//! The image a pod is created from when nothing is packed.
//!
//! Kubernetes requires `containers[].image`, but neither `--external` nor
//! `--release` packs one: the filesystem comes from what the node resolves
//! from the pod's annotation. So the plugin synthesizes a content-free image
//! whose only job is to be pullable — and, if the runtime never receives that
//! annotation, to say so.
//!
//! The flake below exists to fail. `expansion_request` consults an embedded
//! flake only when neither a release nor a source annotation is present, so
//! this one can never shadow either; reaching it at all means the annotation
//! was dropped — the single silent failure mode this design has. Evaluating a
//! `throw` converts that silence into a create-time error carrying its own
//! diagnosis, which is why the placeholder is a throwing flake rather than an
//! empty layer (`exec: no such file or directory`) or a borrowed public image
//! (whose Env, User and WorkingDir CRI would merge into the container).
//!
//! One image serves both modes. The text therefore names both annotation
//! families rather than guessing which one went missing: the runtime cannot
//! tell us what it never received, and a diagnosis that confidently named the
//! wrong one would send a reader to the wrong line of containerd's config.

use crate::pack::{LayerWriter, LAYER_ROOT};

/// Reached only when the annotation never arrived, so the text is a diagnosis
/// rather than a rootfs. Editing it changes every future pod's image digest —
/// see the golden test below.
pub const FLAKE: &str = r#"# Reached only when this pod's imageless annotation never arrived: an
# annotation takes precedence over this flake (SPEC.md §3), so evaluating it at
# all means the runtime never saw one.
{
  outputs = _: {
    rootfs = throw ''
      imageless: this pod's root filesystem was to come from an imageless
      annotation, but the runtime received none. The containerd runtime handler
      must allow-list the annotation family in BOTH pod_annotations and
      container_annotations: imageless.run/* for a digest-addressed release,
      run.imageless.* for a flake reference (examples/containerd-config.toml).
      Run `kubectl imageless doctor` to see what this cluster is missing.
    '';
  };
}
"#;

/// The lock nix writes for an input-less flake. Shipped so evaluation does not
/// try to write one into the staged copy — which is read-only to the workload
/// and, on a node without network egress, would fail before reaching the throw.
///
/// Checked rather than assumed: `nix build --offline path:<read-only copy>#rootfs`,
/// with no lock-file flags, reaches the `throw` and prints its text. Both
/// halves matter — an air-gapped node and a copy nix cannot write to are the
/// conditions under which this diagnosis has to arrive.
pub const LOCK: &str = r#"{"nodes":{"root":{}},"root":"root","version":7}"#;

/// The placeholder layer: byte-identical for every user of a given build, so
/// `ensure_blob`'s HEAD short-circuits after the first push to a repository.
pub fn layer() -> Vec<u8> {
    let mut writer = LayerWriter::default();
    // Byte-lexicographic order, matching `pack`'s determinism rule.
    for entry in ["etc", LAYER_ROOT] {
        writer
            .directory(entry)
            .expect("static paths fit a tar header");
    }
    for (name, contents) in [("flake.lock", LOCK), ("flake.nix", FLAKE)] {
        writer
            .file(&format!("{LAYER_ROOT}/{name}"), 0o644, contents.as_bytes())
            .expect("static paths fit a tar header");
    }
    writer.finish();
    writer.tar
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci;

    /// Pinned because the digest is a deployment identity: every external pod
    /// this plugin prints references it. A change here is intentional churn —
    /// it means existing repositories need the new placeholder pushed — so the
    /// test failing on an edit to FLAKE or LOCK is the point, not friction to
    /// route around.
    #[test]
    fn golden_placeholder_digest() {
        let image = oci::assemble(layer(), "amd64");
        assert_eq!(
            image.layer_digest,
            "sha256:c7c6f7e813a2d533b7c93cb3aedf2c4f09c42fd1f8d157922d22392660bd3999"
        );
    }

    #[test]
    fn the_placeholder_carries_the_flake_where_discovery_looks() {
        let tar = layer();
        let text = String::from_utf8_lossy(&tar);
        assert!(text.contains("etc/imageless/flake.nix"), "flake path");
        assert!(text.contains("etc/imageless/flake.lock"), "lock path");
        // The whole point: evaluating it fails, loudly and with a reason.
        assert!(
            text.contains("throw"),
            "the placeholder must fail evaluation"
        );
        // One image serves both modes, and the runtime cannot report which
        // annotation it never received — so the diagnosis names both families
        // rather than sending a reader to the wrong line of containerd's config.
        for family in ["imageless.run/*", "run.imageless.*"] {
            assert!(
                text.contains(family),
                "the diagnosis must name the {family} annotation family"
            );
        }
    }

    #[test]
    fn the_layer_is_a_pure_function_of_its_contents() {
        assert_eq!(layer(), layer());
    }
}
