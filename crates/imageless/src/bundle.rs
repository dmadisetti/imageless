//! Atomic OCI bundle rewrite and the client-side resolve-and-apply seam.

use crate::client::{effective_uid, request_resolution_detailed};
use crate::gc::remove_bundle_gc_roots;
use crate::materialize::{
    elapsed_us, ErrorCategory, ResolutionError, ResolutionSuccess, ResolveRequest,
};
use crate::mounts::{apply_node_store_projection, apply_store_mounts, store_projection_for};
use crate::release::{ProcessMetadata, ResolvedRelease};
use crate::resolver::resolve_in_process;
use crate::spec::expansion_request;
use crate::{to_io, StoreProjection};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How a caller reaches materialization: through the node resolver daemon's
/// UNIX socket, or in-process under the node policy file. In-process
/// materialization runs as the calling user and provides no cross-process
/// single-flight; the daemon remains the multi-tenant deployment shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializerConfig {
    Socket(PathBuf),
    InProcess { policy_path: Option<PathBuf> },
}

impl MaterializerConfig {
    /// Environment-based selection for adapter binaries: a non-empty
    /// `IMAGELESS_RESOLVER_SOCKET` selects the daemon socket; otherwise
    /// materialization happens in-process, reading the policy file named by
    /// `IMAGELESS_POLICY` (default `/etc/imageless/policy.json`).
    pub fn from_environment() -> Self {
        match std::env::var("IMAGELESS_RESOLVER_SOCKET") {
            Ok(socket) if !socket.is_empty() => Self::Socket(PathBuf::from(socket)),
            _ => Self::InProcess {
                policy_path: std::env::var_os("IMAGELESS_POLICY")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from),
            },
        }
    }

    fn resolve(&self, request: &ResolveRequest) -> Result<ResolutionSuccess, ResolutionError> {
        match self {
            Self::Socket(socket_path) => request_resolution_detailed(socket_path, request),
            Self::InProcess { policy_path } => resolve_in_process(policy_path.as_deref(), request),
        }
    }
}

/// Resolve an annotated OCI bundle through the configured materializer and
/// atomically apply the returned image-like metadata. Both a standalone
/// adapter and an embedding OCI runtime can call this seam; a socket-backed
/// caller never gains authority to execute Nix.
pub fn resolve_and_apply_bundle(
    config_path: &Path,
    bundle_path: &Path,
    default_output: &str,
    timeout_seconds: u64,
    materializer: &MaterializerConfig,
) -> io::Result<Option<ResolvedRelease>> {
    resolve_and_apply_bundle_detailed(
        config_path,
        bundle_path,
        default_output,
        timeout_seconds,
        materializer,
    )
    .map(|result| result.map(|result| result.resolution))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleTimings {
    pub selection_us: u64,
    pub policy_verification_us: u64,
    pub substitution_us: u64,
    pub rewrite_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedResolution {
    pub resolution: ResolvedRelease,
    pub timings: BundleTimings,
}

pub fn resolve_and_apply_bundle_detailed(
    config_path: &Path,
    bundle_path: &Path,
    default_output: &str,
    timeout_seconds: u64,
    materializer: &MaterializerConfig,
) -> io::Result<Option<AppliedResolution>> {
    let selection_started = Instant::now();
    let Some(request) =
        expansion_request(config_path, bundle_path, default_output, timeout_seconds)?
    else {
        return Ok(None);
    };
    let selection_us = elapsed_us(selection_started);
    let success = materializer
        .resolve(&request)
        .map_err(|error| io::Error::other(format!("resolution failed: {error}")))?;
    let rewrite_started = Instant::now();
    if let Err(error) = apply_resolution(config_path, &success.resolution) {
        let _ = remove_bundle_gc_roots(bundle_path);
        return Err(io::Error::new(
            error.kind(),
            format!("{:?}: {error}", ErrorCategory::SpecConflict),
        ));
    }
    Ok(Some(AppliedResolution {
        resolution: success.resolution,
        timings: BundleTimings {
            selection_us,
            policy_verification_us: success.timings.policy_verification_us,
            substitution_us: success.timings.substitution_us,
            rewrite_us: elapsed_us(rewrite_started),
        },
    }))
}

#[derive(Serialize)]
struct TimingEvent<'a> {
    schema: &'static str,
    release: &'a str,
    stage: &'a str,
    duration_us: u64,
    timestamp_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a str>,
}

/// Append timing events to a node-owned JSON-lines sink without touching the
/// adapter's inherited stdio streams. The configured path is deployment policy;
/// callers should treat telemetry failure as non-fatal to workload startup.
pub fn export_timing_events(
    path: &Path,
    release: &str,
    stages: &[(&str, u64)],
    outcome: Option<&str>,
) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "telemetry path must be absolute",
        ));
    }
    let timestamp_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    for (stage, duration_us) in stages {
        serde_json::to_writer(
            &mut bytes,
            &TimingEvent {
                schema: "imageless.timing.v1",
                release,
                stage,
                duration_us: *duration_us,
                timestamp_unix_ms,
                outcome,
            },
        )
        .map_err(to_io)?;
        bytes.push(b'\n');
    }

    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "telemetry sink must be a daemon-owned regular file that is not group/world writable",
        ));
    }
    file.write_all(&bytes)
}

pub fn apply_resolution(config_path: &Path, resolution: &ResolvedRelease) -> io::Result<()> {
    let projection = store_projection_for(resolution)?;
    apply_resolution_with_projection(config_path, resolution, &projection)
}

/// Bundle rewrite with an explicit store-projection backend. [`apply_resolution`]
/// selects the backend from node configuration (`IMAGELESS_STORE_PROJECTION`);
/// tests drive the backend directly.
pub fn apply_resolution_with_projection(
    config_path: &Path,
    resolution: &ResolvedRelease,
    projection: &StoreProjection,
) -> io::Result<()> {
    let text = std::fs::read_to_string(config_path)?;
    let permissions = std::fs::metadata(config_path)?.permissions();
    let mut document: serde_json::Value = serde_json::from_str(&text).map_err(to_io)?;
    let root = document
        .get_mut("root")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| io::Error::other("config.json has no `root` object to rewrite"))?;
    root.insert(
        "path".into(),
        serde_json::Value::String(resolution.rootfs.clone()),
    );

    if let Some(metadata) = &resolution.process {
        apply_process_metadata(&mut document, metadata)?;
    }
    apply_store_mounts(&mut document, &resolution.mounts)?;
    apply_node_store_projection(&mut document, projection)?;

    write_config_atomically(config_path, permissions, &document)
}

/// Compatibility helper for callers that only replace a rootfs. Production
/// release expansion should use [`apply_resolution`] so image metadata is not
/// silently discarded.
pub fn rewrite_root_path(config_path: &Path, store_path: &str) -> io::Result<()> {
    apply_resolution(
        config_path,
        &ResolvedRelease {
            identity: "rootfs-only".to_string(),
            rootfs: store_path.to_string(),
            process: None,
            mounts: Vec::new(),
        },
    )
}

fn apply_process_metadata(
    document: &mut serde_json::Value,
    metadata: &ProcessMetadata,
) -> io::Result<()> {
    let process = document
        .get_mut("process")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| io::Error::other("config.json has no `process` object to rewrite"))?;

    if metadata.entrypoint.is_some() || metadata.default_args.is_some() {
        let existing = process
            .get("args")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "config.json process.args contains a non-string",
                            )
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let mut args = match &metadata.entrypoint {
            Some(entrypoint) => entrypoint.clone(),
            None => existing.first().cloned().into_iter().collect(),
        };
        if let Some(default_args) = &metadata.default_args {
            args.extend(default_args.clone());
        }
        if args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "release process metadata would produce an empty process.args",
            ));
        }
        process.insert(
            "args".to_string(),
            serde_json::Value::Array(args.into_iter().map(serde_json::Value::String).collect()),
        );
    }

    if let Some(cwd) = &metadata.working_directory {
        process.insert("cwd".to_string(), serde_json::Value::String(cwd.clone()));
    }

    if !metadata.environment.is_empty() {
        let environment = process
            .entry("env")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.json process.env is not an array",
                )
            })?;
        let mut positions = HashMap::new();
        for (index, value) in environment.iter().enumerate() {
            let value = value.as_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "config.json process.env contains a non-string",
                )
            })?;
            if let Some((name, _)) = value.split_once('=') {
                positions.insert(name.to_string(), index);
            }
        }
        for entry in &metadata.environment {
            if let Some(index) = positions.get(&entry.name).copied() {
                if !entry.allow_workload_override {
                    environment[index] =
                        serde_json::Value::String(format!("{}={}", entry.name, entry.value));
                }
            } else {
                positions.insert(entry.name.clone(), environment.len());
                environment.push(serde_json::Value::String(format!(
                    "{}={}",
                    entry.name, entry.value
                )));
            }
        }
    }
    Ok(())
}

fn write_config_atomically(
    config_path: &Path,
    permissions: std::fs::Permissions,
    document: &serde_json::Value,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&document).map_err(to_io)?;
    let (temporary, mut file) = create_sibling_temp(config_path)?;
    let result = (|| {
        file.set_permissions(permissions)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, config_path)?;
        let parent = config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn create_sibling_temp(config_path: &Path) -> io::Result<(PathBuf, File)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let parent = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{name}.imageless-{}-{nonce}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate config.json temporary file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::normalized_oci_destination;
    use crate::release::{EnvironmentEntry, StoreMount};
    use crate::testutil::{temporary, STORE};
    use crate::NIX_STORE_PATH;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn materializer_selection_follows_the_environment() {
        std::env::set_var("IMAGELESS_RESOLVER_SOCKET", "/run/imageless/test.sock");
        std::env::set_var("IMAGELESS_POLICY", "/etc/imageless/other-policy.json");
        assert_eq!(
            MaterializerConfig::from_environment(),
            MaterializerConfig::Socket(PathBuf::from("/run/imageless/test.sock"))
        );

        // An empty socket variable means daemonless, not "socket at ''".
        std::env::set_var("IMAGELESS_RESOLVER_SOCKET", "");
        assert_eq!(
            MaterializerConfig::from_environment(),
            MaterializerConfig::InProcess {
                policy_path: Some(PathBuf::from("/etc/imageless/other-policy.json"))
            }
        );

        std::env::remove_var("IMAGELESS_RESOLVER_SOCKET");
        std::env::remove_var("IMAGELESS_POLICY");
        assert_eq!(
            MaterializerConfig::from_environment(),
            MaterializerConfig::InProcess { policy_path: None }
        );
    }

    #[test]
    fn passthrough_bundles_never_consult_resolver_or_policy() {
        let dir = temporary("passthrough");
        let config = dir.join("config.json");
        std::fs::write(
            &config,
            r#"{"ociVersion":"1.2.0","root":{"path":"rootfs"},"process":{"args":["sh"]}}"#,
        )
        .unwrap();
        let before = std::fs::read(&config).unwrap();
        // A bundle with no imageless annotations and no embedded flake must
        // pass through even when the configured policy file cannot exist.
        let result = resolve_and_apply_bundle_detailed(
            &config,
            &dir,
            "rootfs",
            5,
            &MaterializerConfig::InProcess {
                policy_path: Some(PathBuf::from("/nonexistent/imageless-policy.json")),
            },
        )
        .unwrap();
        assert!(result.is_none());
        assert_eq!(std::fs::read(&config).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewrite_preserves_fields_and_mode() {
        let dir = temporary("rewrite");
        let config = dir.join("config.json");
        std::fs::write(
            &config,
            r#"{"root":{"path":"rootfs","readonly":true},"unknown":{"keep":3}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        rewrite_root_path(&config, STORE).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(value["root"]["path"], STORE);
        assert_eq!(value["root"]["readonly"], true);
        assert_eq!(value["unknown"]["keep"], 3);
        assert_eq!(
            std::fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_metadata_replaces_only_declared_image_fields() {
        assert!(normalized_oci_destination("/"));
        assert!(normalized_oci_destination("/opt/data"));
        for invalid in [
            "opt/data",
            "/opt//data",
            "/opt/./data",
            "/opt/../data",
            "/opt/data/",
        ] {
            assert!(!normalized_oci_destination(invalid));
        }

        let dir = temporary("release-metadata");
        let config = dir.join("config.json");
        let original = serde_json::json!({
            "ociVersion": "1.2.0",
            "root": { "path": "rootfs", "readonly": true },
            "process": {
                "args": ["placeholder", "workload-arg"],
                "cwd": "/original",
                "env": ["LOCKED=workload", "OPEN=workload", "KEEP=workload"]
            },
            "mounts": [{
                "destination": "/data",
                "source": "tmpfs",
                "type": "tmpfs"
            }],
            "linux": {
                "namespaces": [{ "type": "network" }],
                "resources": { "memory": { "limit": 1048576 } }
            }
        });
        std::fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let resolution = ResolvedRelease {
            identity: "test/agent@sha256:00".to_string(),
            rootfs: STORE.to_string(),
            process: Some(ProcessMetadata {
                entrypoint: Some(vec!["/bin/agent".to_string()]),
                default_args: Some(vec!["serve".to_string()]),
                working_directory: Some("/workspace".to_string()),
                environment: vec![
                    EnvironmentEntry {
                        name: "LOCKED".to_string(),
                        value: "release".to_string(),
                        allow_workload_override: false,
                    },
                    EnvironmentEntry {
                        name: "OPEN".to_string(),
                        value: "release".to_string(),
                        allow_workload_override: true,
                    },
                    EnvironmentEntry {
                        name: "ADDED".to_string(),
                        value: "release".to_string(),
                        allow_workload_override: false,
                    },
                ],
            }),
            mounts: vec![StoreMount {
                source: "/nix/store/11111111111111111111111111111111-tools".to_string(),
                destination: "/opt/tools".to_string(),
            }],
        };
        apply_resolution(&config, &resolution).unwrap();
        let applied: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(applied["root"]["path"], STORE);
        assert_eq!(
            applied["process"]["args"],
            serde_json::json!(["/bin/agent", "serve"])
        );
        assert_eq!(applied["process"]["cwd"], "/workspace");
        assert_eq!(
            applied["process"]["env"],
            serde_json::json!([
                "LOCKED=release",
                "OPEN=workload",
                "KEEP=workload",
                "ADDED=release"
            ])
        );
        assert_eq!(applied["mounts"][0], original["mounts"][0]);
        assert_eq!(applied["mounts"][1]["destination"], "/opt/tools");
        assert_eq!(
            applied["mounts"][1]["options"],
            serde_json::json!(["rbind", "ro", "nosuid", "nodev"])
        );
        assert_eq!(
            applied["mounts"][2],
            serde_json::json!({
                "destination": NIX_STORE_PATH,
                "options": ["rbind", "ro", "nosuid", "nodev"],
                "source": NIX_STORE_PATH,
                "type": "bind"
            })
        );
        assert_eq!(applied["linux"], original["linux"]);

        std::fs::write(&config, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let before = std::fs::read(&config).unwrap();
        let collision = ResolvedRelease {
            mounts: vec![StoreMount {
                source: "/nix/store/22222222222222222222222222222222-data".to_string(),
                destination: "/data".to_string(),
            }],
            ..resolution.clone()
        };
        let error = apply_resolution(&config, &collision).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&config).unwrap(), before);

        for destination in ["/", "/nix", "/nix/store", "/nix/store/dependency"] {
            let mut conflicting = original.clone();
            conflicting["mounts"] = serde_json::json!([{
                "destination": destination,
                "source": "/host/source",
                "type": "bind"
            }]);
            std::fs::write(&config, serde_json::to_vec_pretty(&conflicting).unwrap()).unwrap();
            let before = std::fs::read(&config).unwrap();
            let error = apply_resolution(&config, &resolution).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("Nix store projection"));
            assert_eq!(std::fs::read(&config).unwrap(), before);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
