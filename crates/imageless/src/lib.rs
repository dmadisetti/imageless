//! Shared annotation, protocol, resolver, and bundle-lifecycle implementation.
//!
//! The public API is deliberately flat: adapters (`imageless-runc`, embedding
//! OCI runtimes) and the resolver daemon consume `imageless::*` directly.

mod bundle;
mod client;
mod gc;
mod materialize;
mod mounts;
mod nix;
mod release;
mod resolver;
mod spec;

pub use bundle::{
    apply_resolution, apply_resolution_with_projection, export_timing_events,
    resolve_and_apply_bundle, resolve_and_apply_bundle_detailed, rewrite_root_path,
    AppliedResolution, BundleTimings, MaterializerConfig,
};
pub use client::{
    effective_uid, peer_allowed, peer_uid, read_frame, request_inspection, request_resolution,
    request_resolution_detailed, write_frame,
};
pub use gc::{remove_bundle_gc_roots, remove_gc_root};
pub use materialize::{
    ClosurePathReport, ClosureReport, ContractError, ErrorCategory, Materialize, ResolutionError,
    ResolutionSuccess, ResolutionTimings, ResolvePurpose, ResolveRequest, ResolveResponse,
};
pub use mounts::{enumerate_closure, store_projection_for, StoreProjection};
pub use release::{
    canonical_bytes as canonical_manifest_bytes, digest as manifest_digest, CachePolicy,
    EnvironmentEntry, EvidenceReference, IssuerPolicy, ManifestSource, ProcessMetadata,
    ReleaseManifest, ReleaseReference, ReleaseTarget, ResolvedRelease, ResolverPolicy, Selectors,
    StoreMount, MAX_MANIFEST_BYTES, RELEASE_SCHEMA,
};
pub use resolver::{
    handle_connection, load_resolver_policy, resolve_in_process, serve, DevelopmentWorkerConfig,
    Resolver, ResolverConfig, DEFAULT_POLICY_PATH,
};
pub use spec::{action_of, canonical_bundle, expansion_request, plan, validate_store_path};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME_BYTES: usize = 16 * 1024;
pub const MAX_CONNECTIONS: usize = 64;
pub const GC_ROOT_NAME: &str = ".imageless-rootfs-gcroot";
pub const GC_ROOTS_DIR_NAME: &str = ".imageless-store-gcroots";
pub const NIX_STORE_PATH: &str = "/nix/store";
pub const EMBEDDED_FLAKE_PATH: &str = "etc/imageless/flake.nix";
pub(crate) const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ANNOTATION_VALUE_BYTES: usize = 4096;
pub(crate) const MAX_SELECTOR_BYTES: usize = 1024;
pub(crate) const MAX_STAGED_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_STAGED_SOURCE_ENTRIES: usize = 4096;

pub const SOURCE_ANNOTATION: &str = "run.imageless.source";
pub const OUTPUT_ANNOTATION: &str = "run.imageless.output";
pub const CONTAINERS_ANNOTATION: &str = "run.imageless.containers";
pub const SKIP_CONTAINERS_ANNOTATION: &str = "run.imageless.skip-containers";
pub const RELEASE_ANNOTATION: &str = "imageless.run/release-v1";
pub const RELEASE_CONTAINERS_ANNOTATION: &str = "imageless.run/containers-v1";
pub const RELEASE_SKIP_CONTAINERS_ANNOTATION: &str = "imageless.run/skip-containers-v1";
pub const CRI_CONTAINER_TYPE_ANNOTATION: &str = "io.kubernetes.cri.container-type";
pub const CRI_CONTAINER_NAME_ANNOTATION: &str = "io.kubernetes.cri.container-name";

pub(crate) fn to_io(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[cfg(test)]
pub(crate) mod testutil {
    use crate::materialize::{Materialize, ResolvePurpose, ResolveRequest};
    use crate::release::{ResolvedRelease, ResolverPolicy};
    use crate::resolver::{Resolver, ResolverConfig};
    use crate::PROTOCOL_VERSION;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub(crate) const STORE: &str = "/nix/store/00000000000000000000000000000000-rootfs";
    pub(crate) const RELEASE_REF: &str =
        "test/agent@sha256:0000000000000000000000000000000000000000000000000000000000000000";

    pub(crate) fn resolved() -> ResolvedRelease {
        ResolvedRelease {
            identity: "test/agent@sha256:00".to_string(),
            rootfs: STORE.to_string(),
            process: None,
            mounts: Vec::new(),
        }
    }

    pub(crate) fn annotations(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    pub(crate) fn temporary(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "imageless-{label}-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    pub(crate) fn executable(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub(crate) fn fake_nix(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-nix");
        executable(
            &path,
            &format!(
                r#"
root=''
requested=''
next_root=0
for arg in "$@"; do
  if [ "$next_root" = 1 ]; then root="$arg"; next_root=0; fi
  if [ "$arg" = "--add-root" ] || [ "$arg" = "--out-link" ]; then next_root=1; fi
  case "$arg" in /nix/store/*) requested="$arg" ;; esac
done
{body}
rm -f "$root"
store_path="${{requested:-$FAKE_STORE}}"
ln -s "$store_path" "$root"
printf '%s\n' "$store_path"
"#
            ),
        );
        path
    }

    pub(crate) fn resolver(nix: PathBuf, max: usize, timeout: Duration) -> Resolver {
        Resolver::new(ResolverConfig {
            max_realizations: max,
            max_timeout: timeout,
            nix: nix.clone(),
            nix_store: nix,
            policy: ResolverPolicy {
                system: "x86_64-linux".to_string(),
                cache_only: false,
                eval_allowed_uri_prefixes: vec!["path:".to_string()],
                issuers: HashMap::new(),
            },
            development_worker: None,
            evaluate_as_caller: false,
        })
    }

    pub(crate) fn request(
        bundle: PathBuf,
        materialize: Materialize,
        timeout_ms: u64,
    ) -> ResolveRequest {
        ResolveRequest {
            version: PROTOCOL_VERSION,
            purpose: ResolvePurpose::Runtime,
            materialize,
            bundle_path: bundle,
            timeout_ms,
            container_name: None,
        }
    }
}
