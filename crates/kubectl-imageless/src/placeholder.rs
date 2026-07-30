//! The image an external-reference pod is created from.
//!
//! Kubernetes requires `containers[].image`, but `--external` packs nothing:
//! the filesystem comes from the node's evaluation of the flake reference. So
//! the plugin synthesizes a content-free image whose only job is to be
//! pullable — and, if the runtime never receives the annotation that makes the
//! pod imageless, to say so.
//!
//! The flake below exists to fail. `expansion_request` consults an embedded
//! flake only when no source annotation is present, so this one can never
//! shadow the external reference; reaching it at all means the annotation was
//! dropped — the single silent failure mode this design has. Evaluating a
//! `throw` converts that silence into a create-time error carrying its own
//! diagnosis, which is why the placeholder is a throwing flake rather than an
//! empty layer (`exec: no such file or directory`) or a borrowed public image
//! (whose Env, User and WorkingDir CRI would merge into the container).

use crate::pack::{LayerWriter, LAYER_ROOT};

/// Reached only when the annotation never arrived, so the text is a diagnosis
/// rather than a rootfs. Editing it changes every future pod's image digest —
/// see the golden test below.
pub const FLAKE: &str = r#"# Reached only when this pod's run.imageless.source annotation never arrived:
# the annotation takes precedence over this flake (SPEC.md §3), so evaluating
# it at all means the runtime never saw it.
{
  outputs = _: {
    rootfs = throw ''
      imageless: this pod deploys an external flake reference, but the runtime
      received no run.imageless.source annotation. The containerd runtime
      handler must allow-list run.imageless.* in pod_annotations and
      container_annotations (examples/containerd-config.toml); see
      `kubectl imageless doctor`.
    '';
  };
}
"#;

/// The lock nix writes for an input-less flake. Shipped so evaluation does not
/// try to write one into the staged copy — which is read-only to the workload
/// and, on a node without network egress, would fail before reaching the throw.
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
            "sha256:35dd2d7f57ccb49a27cd0cc54e808c7988483ce621edfd0a6728edd7e653daac"
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
        assert!(
            text.contains("run.imageless.source"),
            "the diagnosis must name the missing annotation"
        );
    }

    #[test]
    fn the_layer_is_a_pure_function_of_its_contents() {
        assert_eq!(layer(), layer());
    }
}
