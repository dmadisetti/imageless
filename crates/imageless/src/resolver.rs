//! The resolver daemon: policy loading, single-flight materialization, the
//! development-source staging pipeline, and the UNIX-socket server.

use crate::client::{effective_uid, peer_allowed, peer_uid, read_frame, write_frame};
use crate::gc::{gc_root_path, prepare_gc_root, remove_bundle_gc_roots, validate_registered_root};
use crate::materialize::{
    elapsed_us, remaining, ClosurePathReport, ClosureReport, ErrorCategory, Materialize,
    ResolutionError, ResolutionSuccess, ResolutionTimings, ResolvePurpose, ResolveRequest,
    ResolveResponse,
};
use crate::nix::{parse_materialized_path, run_command, validate_realise_output};
use crate::release::{self, CachePolicy, ReleaseReference, ResolvedRelease, ResolverPolicy};
use crate::spec::{validate_output, validate_store_path};
use crate::{
    to_io, GC_ROOT_NAME, MAX_ANNOTATION_VALUE_BYTES, MAX_CONNECTIONS, MAX_FRAME_BYTES,
    MAX_STAGED_SOURCE_BYTES, MAX_STAGED_SOURCE_ENTRIES, PROTOCOL_VERSION,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The node policy file consulted by in-process materialization when
/// `IMAGELESS_POLICY` does not name another location.
pub const DEFAULT_POLICY_PATH: &str = "/etc/imageless/policy.json";

pub struct ResolverConfig {
    pub max_realizations: usize,
    pub max_timeout: Duration,
    pub nix: PathBuf,
    pub nix_store: PathBuf,
    pub policy: ResolverPolicy,
    pub development_worker: Option<DevelopmentWorkerConfig>,
    /// Without a development worker, evaluate development sources directly as
    /// the calling user instead of refusing. In-process materialization sets
    /// this; the daemon never does — it keeps requiring the unprivileged
    /// worker so evaluation cannot run with daemon privileges.
    pub evaluate_as_caller: bool,
}

#[derive(Clone, Debug)]
pub struct DevelopmentWorkerConfig {
    pub program: PathBuf,
    pub user: String,
}

impl ResolverConfig {
    pub fn from_environment(max_realizations: usize, timeout_seconds: u64) -> Self {
        Self {
            max_realizations,
            max_timeout: Duration::from_secs(timeout_seconds),
            nix: std::env::var_os("IMAGELESS_NIX")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(option_env!("IMAGELESS_NIX").unwrap_or("nix"))),
            nix_store: std::env::var_os("IMAGELESS_NIX_STORE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(option_env!("IMAGELESS_NIX_STORE").unwrap_or("nix-store"))
                }),
            policy: ResolverPolicy {
                system: std::env::var("IMAGELESS_SYSTEM")
                    .unwrap_or_else(|_| "x86_64-linux".to_string()),
                cache_only: std::env::var_os("IMAGELESS_DEVELOPMENT").is_none(),
                eval_allowed_uri_prefixes: Vec::new(),
                issuers: HashMap::new(),
            },
            development_worker: None,
            evaluate_as_caller: false,
        }
    }
}

/// Materialize a request inside the calling process instead of a resolver
/// daemon: load the node policy (an explicit path must load; the default path
/// falls back to a fail-closed cache-only policy when absent) and drive a
/// process-local [`Resolver`] as the calling user. There is no cross-process
/// single-flight in this mode.
pub fn resolve_in_process(
    policy_path: Option<&Path>,
    request: &ResolveRequest,
) -> Result<ResolutionSuccess, ResolutionError> {
    resolve_in_process_at(policy_path, Path::new(DEFAULT_POLICY_PATH), request)
}

fn resolve_in_process_at(
    policy_path: Option<&Path>,
    default_path: &Path,
    request: &ResolveRequest,
) -> Result<ResolutionSuccess, ResolutionError> {
    let (policy, defaulted) = in_process_policy(policy_path, default_path)?;
    let mut config = ResolverConfig::from_environment(2, 3600);
    config.policy = policy;
    config.evaluate_as_caller = true;
    let result = Resolver::new(config).resolve_detailed(request.clone());
    if defaulted {
        result.map_err(|error| default_policy_hint(error, default_path))
    } else {
        result
    }
}

fn in_process_policy(
    policy_path: Option<&Path>,
    default_path: &Path,
) -> Result<(ResolverPolicy, bool), ResolutionError> {
    let (path, defaultable) = match policy_path {
        Some(path) => (path, false),
        None => (default_path, true),
    };
    match load_resolver_policy(path) {
        Ok(policy) => Ok((policy, false)),
        Err(error) if defaultable && error.kind() == io::ErrorKind::NotFound => Ok((
            ResolverPolicy {
                system: std::env::var("IMAGELESS_SYSTEM")
                    .unwrap_or_else(|_| "x86_64-linux".to_string()),
                cache_only: true,
                eval_allowed_uri_prefixes: Vec::new(),
                issuers: HashMap::new(),
            },
            true,
        )),
        Err(error) => Err(ResolutionError::new(
            ErrorCategory::PolicyDenied,
            format!(
                "node resolver policy {} could not be loaded: {error}",
                path.display()
            ),
            false,
        )),
    }
}

fn default_policy_hint(error: ResolutionError, default_path: &Path) -> ResolutionError {
    match error.category {
        ErrorCategory::EvaluationDisabled | ErrorCategory::PolicyDenied => ResolutionError::new(
            error.category,
            format!(
                "{}; node policy does not permit this — write {} to authorize releases or development sources",
                error.diagnostic,
                default_path.display()
            ),
            error.retryable,
        ),
        _ => error,
    }
}

pub fn load_resolver_policy(path: &Path) -> io::Result<ResolverPolicy> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "resolver policy must be a daemon-owned regular file that is not group/world writable",
        ));
    }
    if metadata.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resolver policy exceeds 1 MiB",
        ));
    }
    let policy: ResolverPolicy =
        serde_json::from_slice(&std::fs::read(path)?).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("resolver policy is invalid: {error}"),
            )
        })?;
    release::validate_policy(&policy, effective_uid()).map_err(to_io)?;
    Ok(policy)
}

#[derive(Clone)]
pub struct Resolver {
    inner: Arc<ResolverInner>,
}

struct ResolverInner {
    config: ResolverConfig,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    permits: Mutex<usize>,
    permit_ready: Condvar,
}

struct Flight {
    state: Mutex<FlightState>,
    ready: Condvar,
}

struct FlightState {
    callers: usize,
    result: Option<Result<ResolvedRelease, ResolutionError>>,
}

#[derive(Clone)]
struct SelectedRelease {
    resolution: ResolvedRelease,
    cache: CachePolicy,
}

impl Resolver {
    pub fn new(config: ResolverConfig) -> Self {
        assert!((1..=64).contains(&config.max_realizations));
        Self {
            inner: Arc::new(ResolverInner {
                config,
                flights: Mutex::new(HashMap::new()),
                permits: Mutex::new(0),
                permit_ready: Condvar::new(),
            }),
        }
    }

    pub fn resolve(&self, request: ResolveRequest) -> Result<ResolvedRelease, ResolutionError> {
        self.resolve_detailed(request)
            .map(|success| success.resolution)
    }

    pub fn resolve_detailed(
        &self,
        request: ResolveRequest,
    ) -> Result<ResolutionSuccess, ResolutionError> {
        let (bundle, deadline) = self.validate_request(&request)?;
        if request.purpose == ResolvePurpose::Inspect {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "inspection requests cannot materialize a release",
                false,
            ));
        }

        // Selection and policy enforcement are deliberately per caller. The
        // subsequent Nix work is single-flighted by immutable release identity,
        // but a concurrent prewarm or differently named container must never
        // lend its selector decision to another request.
        let policy_started = Instant::now();
        let selected = match &request.materialize {
            Materialize::Release(reference) => Some(self.select_release(
                reference,
                request.container_name.as_deref(),
                request.purpose,
                deadline,
            )?),
            _ => None,
        };
        let policy_verification_us = elapsed_us(policy_started);
        let substitution_started = Instant::now();

        let key = request.materialize.key();
        let (flight, leader) = {
            let mut flights = self.inner.flights.lock().unwrap();
            if let Some(flight) = flights.get(&key) {
                flight.state.lock().unwrap().callers += 1;
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(Flight {
                    state: Mutex::new(FlightState {
                        callers: 1,
                        result: None,
                    }),
                    ready: Condvar::new(),
                });
                flights.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        let result = if leader {
            let result =
                self.materialize(&request.materialize, selected.as_ref(), &bundle, deadline);
            let mut state = flight.state.lock().unwrap();
            state.result = Some(result.clone());
            flight.ready.notify_all();
            result
        } else {
            match wait_for_flight(&flight, deadline) {
                Ok(resolution) => self.register_resolution(&resolution, &bundle, deadline),
                Err(error) => Err(error),
            }
        };

        if result.is_err() {
            let _ = remove_bundle_gc_roots(&bundle);
        }
        self.release_flight(&key, &flight);
        result.map(|resolution| ResolutionSuccess {
            resolution,
            timings: ResolutionTimings {
                policy_verification_us,
                substitution_us: elapsed_us(substitution_started),
            },
        })
    }

    pub fn inspect(
        &self,
        request: ResolveRequest,
    ) -> Result<(ResolutionSuccess, ClosureReport), ResolutionError> {
        let (_, deadline) = self.validate_request(&request)?;
        if request.purpose != ResolvePurpose::Inspect {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "closure inspection requires an inspection request",
                false,
            ));
        }
        let Materialize::Release(reference) = &request.materialize else {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "only production releases can be inspected",
                false,
            ));
        };
        let policy_started = Instant::now();
        let selected = self.select_release(reference, None, ResolvePurpose::Inspect, deadline)?;
        let policy_verification_us = elapsed_us(policy_started);
        let substitution_started = Instant::now();
        let closure = self.query_closure(&selected, deadline)?;
        let success = ResolutionSuccess {
            resolution: selected.resolution,
            timings: ResolutionTimings {
                policy_verification_us,
                substitution_us: elapsed_us(substitution_started),
            },
        };
        Ok((success, closure))
    }

    fn validate_request(
        &self,
        request: &ResolveRequest,
    ) -> Result<(PathBuf, Instant), ResolutionError> {
        if request.version != PROTOCOL_VERSION {
            return Err(ResolutionError::new(
                ErrorCategory::Protocol,
                "unsupported protocol version",
                false,
            ));
        }
        if request.timeout_ms == 0 || request.timeout_ms > 3_600_000 {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "timeout must be between 1ms and 3600s",
                false,
            ));
        }
        if matches!(
            request.purpose,
            ResolvePurpose::Prewarm | ResolvePurpose::Inspect
        ) && (!matches!(request.materialize, Materialize::Release(_))
            || request.container_name.is_some())
        {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "prewarm and inspection requests require a release and no runtime container identity",
                false,
            ));
        }
        match &request.materialize {
            Materialize::Closure(path) => validate_store_path("closure", path).map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::InvalidRequest,
                    "closure is not a canonical Nix store path",
                    false,
                )
            })?,
            Materialize::Flake(installable) => validate_installable(installable)?,
            Materialize::Release(reference) => {
                if !self
                    .inner
                    .config
                    .policy
                    .issuers
                    .contains_key(&reference.issuer)
                {
                    return Err(ResolutionError::new(
                        ErrorCategory::PolicyDenied,
                        "release issuer is not authorized on this node",
                        false,
                    ));
                }
            }
        }
        if matches!(&request.materialize, Materialize::Flake(_))
            && self.inner.config.policy.cache_only
        {
            return Err(ResolutionError::new(
                ErrorCategory::EvaluationDisabled,
                "flake source evaluation is disabled on this node (cache_only)",
                false,
            ));
        }
        if let Materialize::Flake(installable) = &request.materialize {
            let uri = installable
                .rsplit_once('#')
                .map(|(uri, _)| uri)
                .unwrap_or("");
            if !self
                .inner
                .config
                .policy
                .eval_allowed_uri_prefixes
                .iter()
                .any(|prefix| uri.starts_with(prefix))
            {
                return Err(ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "development source URI is not authorized by node policy",
                    false,
                ));
            }
        }
        if !request.bundle_path.is_absolute() {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "bundle path must be absolute and canonical",
                false,
            ));
        }
        let canonical = std::fs::canonicalize(&request.bundle_path).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "bundle path does not name an accessible directory",
                false,
            )
        })?;
        if canonical != request.bundle_path || !canonical.is_dir() {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "bundle path must be absolute and canonical",
                false,
            ));
        }
        let requested = Duration::from_millis(request.timeout_ms);
        let effective = requested.min(self.inner.config.max_timeout);
        let deadline = Instant::now().checked_add(effective).ok_or_else(|| {
            ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "timeout is out of range",
                false,
            )
        })?;
        Ok((canonical, deadline))
    }

    fn materialize(
        &self,
        materialize: &Materialize,
        selected: Option<&SelectedRelease>,
        bundle: &Path,
        deadline: Instant,
    ) -> Result<ResolvedRelease, ResolutionError> {
        if matches!(materialize, Materialize::Release(_)) {
            let selected = selected.ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::Internal,
                    "release selection was not available",
                    false,
                )
            })?;
            return self.materialize_release(selected, bundle, deadline);
        }
        if let Materialize::Flake(installable) = materialize {
            return self.materialize_development(installable, bundle, deadline);
        }
        let root = bundle.join(GC_ROOT_NAME);
        prepare_gc_root(&root)?;
        let _permit = self.acquire_permit(deadline, "while waiting for a Nix operation")?;
        let mut command = match materialize {
            Materialize::Closure(store_path) => {
                let mut command = Command::new(&self.inner.config.nix_store);
                command.args([OsStr::new("--realise"), store_path.as_ref()]);
                command.args([OsStr::new("--add-root"), root.as_os_str()]);
                command
            }
            Materialize::Flake(_) => unreachable!(),
            Materialize::Release(_) => unreachable!(),
        };
        let stdout = run_command(&mut command, remaining(deadline, "during materialization")?)
            .map_err(|error| match error.kind() {
                io::ErrorKind::TimedOut => ResolutionError::timeout("during materialization"),
                _ => ResolutionError::new(
                    ErrorCategory::Materialization,
                    "Nix could not materialize the requested rootfs",
                    true,
                ),
            })?;
        let store_path = match materialize {
            Materialize::Closure(store_path) => store_path,
            Materialize::Flake(_) | Materialize::Release(_) => unreachable!(),
        };
        validate_realise_output(&stdout, store_path, &root).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Materialization,
                "Nix returned an invalid or ambiguous rootfs path",
                false,
            )
        })?;
        validate_registered_root(&root, store_path, ErrorCategory::Materialization)?;
        Ok(ResolvedRelease {
            identity: match materialize {
                Materialize::Closure(_) => "legacy-closure".to_string(),
                Materialize::Flake(_) => "development-source".to_string(),
                Materialize::Release(_) => unreachable!(),
            },
            rootfs: store_path.clone(),
            process: None,
            mounts: Vec::new(),
        })
    }

    fn materialize_development(
        &self,
        installable: &str,
        bundle: &Path,
        deadline: Instant,
    ) -> Result<ResolvedRelease, ResolutionError> {
        let worker = self.inner.config.development_worker.as_ref();
        if worker.is_none() && !self.inner.config.evaluate_as_caller {
            return Err(ResolutionError::new(
                ErrorCategory::EvaluationDisabled,
                "development source evaluation has no unprivileged worker configured",
                false,
            ));
        }
        let root = bundle.join(GC_ROOT_NAME);
        prepare_gc_root(&root)?;
        let staged = stage_development_installable(
            installable,
            worker.map(|worker| worker.user.as_str()),
            deadline,
        )?;
        let evaluation_installable = staged
            .as_ref()
            .map(|(installable, _guard)| installable.as_str())
            .unwrap_or(installable);
        let timeout = remaining(deadline, "before development source evaluation")?;
        let timeout_seconds = timeout.as_secs().max(1).to_string();
        let stdout = {
            let _permit =
                self.acquire_permit(deadline, "while waiting for a development Nix operation")?;
            let mut command = match worker {
                Some(worker) => {
                    let mut command = Command::new(&worker.program);
                    command
                        .arg("--user")
                        .arg(&worker.user)
                        .arg("--nix")
                        .arg(&self.inner.config.nix)
                        .arg("--cpu-seconds")
                        .arg(timeout_seconds)
                        .arg("--installable")
                        .arg(evaluation_installable)
                        .env_clear();
                    command
                }
                None => {
                    let mut command = Command::new(&self.inner.config.nix);
                    command
                        .args([
                            "--extra-experimental-features",
                            "nix-command flakes",
                            "build",
                            "--no-link",
                            "--print-out-paths",
                        ])
                        .arg(evaluation_installable);
                    command
                }
            };
            run_command(
                &mut command,
                remaining(deadline, "during development source evaluation")?,
            )
            .map_err(|error| match error.kind() {
                io::ErrorKind::TimedOut => {
                    ResolutionError::timeout("during development source evaluation")
                }
                _ => ResolutionError::new(
                    ErrorCategory::Materialization,
                    "the development resolver could not evaluate the source",
                    true,
                ),
            })?
        };
        let store_path = parse_materialized_path(&stdout).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Materialization,
                "development resolver returned an invalid or ambiguous rootfs path",
                false,
            )
        })?;
        self.register_root(&store_path, &root, deadline)?;
        Ok(ResolvedRelease {
            identity: "development-source".to_string(),
            rootfs: store_path,
            process: None,
            mounts: Vec::new(),
        })
    }

    fn select_release(
        &self,
        reference: &ReleaseReference,
        container_name: Option<&str>,
        purpose: ResolvePurpose,
        deadline: Instant,
    ) -> Result<SelectedRelease, ResolutionError> {
        let issuer = self
            .inner
            .config
            .policy
            .issuers
            .get(&reference.issuer)
            .ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::PolicyDenied,
                    "release issuer is not authorized on this node",
                    false,
                )
            })?;
        if !issuer.permits_release(&reference.name) {
            return Err(ResolutionError::new(
                ErrorCategory::PolicyDenied,
                "release name is not allowed by node policy",
                false,
            ));
        }
        let bytes = release::fetch_manifest(
            &issuer.source,
            &reference.sha256,
            remaining(deadline, "while fetching the release manifest")?,
        )?;
        let manifest = release::parse_full_manifest(&bytes, reference)?;
        if purpose == ResolvePurpose::Runtime && !manifest.selectors.permits(container_name) {
            return Err(ResolutionError::new(
                ErrorCategory::PolicyDenied,
                "release manifest does not permit this container",
                false,
            ));
        }
        let target = manifest
            .targets
            .get(&self.inner.config.policy.system)
            .cloned()
            .ok_or_else(|| {
                let available = manifest
                    .targets
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                ResolutionError::new(
                    ErrorCategory::ArchitectureMismatch,
                    format!(
                        "release has no target for {}; available targets: {available}",
                        self.inner.config.policy.system
                    ),
                    false,
                )
            })?;
        let cache = issuer.caches.get(&target.cache).cloned().ok_or_else(|| {
            ResolutionError::new(
                ErrorCategory::PolicyDenied,
                "release cache identity is not authorized for this issuer",
                false,
            )
        })?;
        let resolution = ResolvedRelease {
            identity: reference.identity(),
            rootfs: target.rootfs,
            process: target.process,
            mounts: target.mounts,
        };
        let response = ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(resolution.clone()),
            timings: ResolutionTimings::default(),
            closure: None,
        };
        if serde_json::to_vec(&response)
            .map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::Internal,
                    "release response could not be encoded",
                    false,
                )
            })?
            .len()
            > MAX_FRAME_BYTES
        {
            return Err(ResolutionError::new(
                ErrorCategory::InvalidRequest,
                "selected release metadata exceeds the resolver protocol frame limit",
                false,
            ));
        }
        Ok(SelectedRelease { resolution, cache })
    }

    fn materialize_release(
        &self,
        selected: &SelectedRelease,
        bundle: &Path,
        deadline: Instant,
    ) -> Result<ResolvedRelease, ResolutionError> {
        for (index, store_path) in selected.resolution.store_paths().enumerate() {
            let root = gc_root_path(bundle, index)?;
            prepare_gc_root(&root)?;
            self.materialize_store_path(store_path, &root, &selected.cache, deadline)?;
        }
        Ok(selected.resolution.clone())
    }

    fn query_closure(
        &self,
        selected: &SelectedRelease,
        deadline: Instant,
    ) -> Result<ClosureReport, ResolutionError> {
        #[derive(Deserialize)]
        struct PathInfo {
            #[serde(rename = "narSize")]
            nar_size: u64,
            #[serde(rename = "downloadSize")]
            download_size: Option<u64>,
        }

        let _permit = self.acquire_permit(deadline, "while waiting to inspect the cache")?;
        let mut path_info = Command::new(&self.inner.config.nix);
        path_info.args([
            OsStr::new("path-info"),
            OsStr::new("--extra-experimental-features"),
            OsStr::new("nix-command"),
            OsStr::new("--json"),
            OsStr::new("--json-format"),
            OsStr::new("1"),
            OsStr::new("--recursive"),
            OsStr::new("--size"),
        ]);
        // The registry-free smoke uses the daemon's already-valid local store
        // as its intentionally trivial cache. Other file:// values are real
        // binary caches and remain explicit remote stores.
        if selected.cache.substituter != "file:///nix/store" {
            path_info.args([OsStr::new("--store"), selected.cache.substituter.as_ref()]);
        }
        if !selected.cache.public_keys.is_empty() {
            path_info.args([
                OsStr::new("--option"),
                OsStr::new("trusted-public-keys"),
                OsStr::new(&selected.cache.public_keys.join(" ")),
            ]);
        }
        path_info.args(selected.resolution.store_paths());
        let stdout = run_command(
            &mut path_info,
            remaining(deadline, "while querying the authorized cache")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => {
                ResolutionError::timeout("while querying the authorized cache")
            }
            _ => ResolutionError::new(
                ErrorCategory::CacheQuery,
                "the authorized cache could not describe the release closure",
                true,
            ),
        })?;
        let infos: BTreeMap<String, PathInfo> = serde_json::from_str(&stdout).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::CacheQuery,
                "the authorized cache returned an invalid closure description",
                false,
            )
        })?;
        if infos.is_empty()
            || selected
                .resolution
                .store_paths()
                .any(|path| !infos.contains_key(path))
        {
            return Err(ResolutionError::new(
                ErrorCategory::CacheQuery,
                "the authorized cache omitted a release store path",
                false,
            ));
        }
        for path in infos.keys() {
            validate_store_path("cache closure path", path).map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::CacheQuery,
                    "the authorized cache returned an invalid closure path",
                    false,
                )
            })?;
        }

        let mut validity = Command::new(&self.inner.config.nix_store);
        validity.args([
            OsStr::new("--check-validity"),
            OsStr::new("--print-invalid"),
        ]);
        validity.args(infos.keys());
        let invalid_stdout = run_command(
            &mut validity,
            remaining(deadline, "while checking the node store")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => ResolutionError::timeout("while checking the node store"),
            _ => ResolutionError::new(
                ErrorCategory::CacheQuery,
                "the node store could not report valid closure paths",
                true,
            ),
        })?;
        let mut invalid = HashSet::new();
        for path in invalid_stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if !infos.contains_key(path) || !invalid.insert(path.to_string()) {
                return Err(ResolutionError::new(
                    ErrorCategory::CacheQuery,
                    "the node store returned an invalid path status",
                    false,
                ));
            }
        }

        let mut total_nar_bytes = 0_u64;
        let mut missing_download_bytes = 0_u64;
        let mut closure_paths = Vec::with_capacity(infos.len());
        for (path, info) in infos {
            let present = !invalid.contains(&path);
            let download_bytes = info.download_size.unwrap_or(info.nar_size);
            total_nar_bytes = total_nar_bytes.checked_add(info.nar_size).ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::CacheQuery,
                    "closure size exceeds the supported range",
                    false,
                )
            })?;
            if !present {
                missing_download_bytes = missing_download_bytes
                    .checked_add(download_bytes)
                    .ok_or_else(|| {
                        ResolutionError::new(
                            ErrorCategory::CacheQuery,
                            "download estimate exceeds the supported range",
                            false,
                        )
                    })?;
            }
            closure_paths.push(ClosurePathReport {
                path,
                nar_bytes: info.nar_size,
                download_bytes,
                present,
            });
        }
        let report = ClosureReport {
            schema: "imageless.closure-report.v1".to_string(),
            release: selected.resolution.identity.clone(),
            closure_paths,
            total_nar_bytes,
            missing_download_bytes,
        };
        let response = ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(selected.resolution.clone()),
            timings: ResolutionTimings::default(),
            closure: Some(Box::new(report.clone())),
        };
        if serde_json::to_vec(&response).map_or(true, |bytes| bytes.len() > MAX_FRAME_BYTES) {
            return Err(ResolutionError::new(
                ErrorCategory::CacheQuery,
                "closure report exceeds the resolver protocol frame limit",
                false,
            ));
        }
        Ok(report)
    }

    fn materialize_store_path(
        &self,
        store_path: &str,
        root: &Path,
        cache: &CachePolicy,
        deadline: Instant,
    ) -> Result<(), ResolutionError> {
        let _permit = self.acquire_permit(deadline, "while waiting for a Nix operation")?;
        let mut command = Command::new(&self.inner.config.nix_store);
        command.args([OsStr::new("--realise"), store_path.as_ref()]);
        command.args([OsStr::new("--add-root"), root.as_os_str()]);
        command.args([
            OsStr::new("--option"),
            OsStr::new("substituters"),
            cache.substituter.as_ref(),
        ]);
        if !cache.public_keys.is_empty() {
            command.args([
                OsStr::new("--option"),
                OsStr::new("trusted-public-keys"),
                OsStr::new(&cache.public_keys.join(" ")),
            ]);
        }
        let stdout = run_command(
            &mut command,
            remaining(deadline, "during release materialization")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => ResolutionError::timeout("during release materialization"),
            _ => ResolutionError::new(
                ErrorCategory::Materialization,
                "Nix could not materialize a release store path",
                true,
            ),
        })?;
        validate_realise_output(&stdout, store_path, root).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Materialization,
                "Nix returned an invalid or ambiguous release store path",
                false,
            )
        })?;
        validate_registered_root(root, store_path, ErrorCategory::Materialization)
    }

    fn register_resolution(
        &self,
        resolution: &ResolvedRelease,
        bundle: &Path,
        deadline: Instant,
    ) -> Result<ResolvedRelease, ResolutionError> {
        for (index, store_path) in resolution.store_paths().enumerate() {
            let root = gc_root_path(bundle, index)?;
            prepare_gc_root(&root)?;
            self.register_root(store_path, &root, deadline)?;
        }
        Ok(resolution.clone())
    }

    fn register_root(
        &self,
        store_path: &str,
        root: &Path,
        deadline: Instant,
    ) -> Result<String, ResolutionError> {
        let _permit = self.acquire_permit(deadline, "while waiting to register the GC root")?;
        let mut command = Command::new(&self.inner.config.nix_store);
        command.args([OsStr::new("--realise"), store_path.as_ref()]);
        command.args([OsStr::new("--add-root"), root.as_os_str()]);
        let stdout = run_command(
            &mut command,
            remaining(deadline, "during GC-root registration")?,
        )
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => ResolutionError::timeout("during GC-root registration"),
            _ => ResolutionError::new(
                ErrorCategory::RootRegistration,
                "Nix could not register the bundle GC root",
                true,
            ),
        })?;
        validate_realise_output(&stdout, store_path, root).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::RootRegistration,
                "Nix returned an invalid path while registering the bundle GC root",
                false,
            )
        })?;
        validate_registered_root(root, store_path, ErrorCategory::RootRegistration)?;
        Ok(store_path.to_string())
    }

    fn acquire_permit(
        &self,
        deadline: Instant,
        stage: &str,
    ) -> Result<Permit<'_>, ResolutionError> {
        let mut active = self.inner.permits.lock().unwrap();
        while *active >= self.inner.config.max_realizations {
            let duration = remaining(deadline, stage)?;
            let (next, timed) = self
                .inner
                .permit_ready
                .wait_timeout(active, duration)
                .unwrap();
            active = next;
            if timed.timed_out() && *active >= self.inner.config.max_realizations {
                return Err(ResolutionError::timeout(stage));
            }
        }
        *active += 1;
        Ok(Permit {
            resolver: &self.inner,
        })
    }

    fn release_flight(&self, key: &str, flight: &Arc<Flight>) {
        let remove = {
            let mut state = flight.state.lock().unwrap();
            state.callers -= 1;
            state.callers == 0
        };
        if remove {
            let mut flights = self.inner.flights.lock().unwrap();
            if flights
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, flight))
            {
                flights.remove(key);
            }
        }
    }
}

struct Permit<'a> {
    resolver: &'a ResolverInner,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut active = self.resolver.permits.lock().unwrap();
        *active -= 1;
        self.resolver.permit_ready.notify_one();
    }
}

fn wait_for_flight(flight: &Flight, deadline: Instant) -> Result<ResolvedRelease, ResolutionError> {
    let mut state = flight.state.lock().unwrap();
    loop {
        if let Some(result) = &state.result {
            return result.clone();
        }
        let duration = remaining(deadline, "while waiting for an identical realization")?;
        let (next, timed) = flight.ready.wait_timeout(state, duration).unwrap();
        state = next;
        if timed.timed_out() && state.result.is_none() {
            return Err(ResolutionError::timeout(
                "while waiting for an identical realization",
            ));
        }
    }
}

fn validate_installable(installable: &str) -> Result<(), ResolutionError> {
    if installable.is_empty()
        || installable.len() > MAX_ANNOTATION_VALUE_BYTES + 512
        || installable.chars().any(char::is_whitespace)
        || installable.chars().any(char::is_control)
    {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "flake installable is not normalized",
            false,
        ));
    }
    let Some((flake, output)) = installable.rsplit_once('#') else {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "flake installable must include one output",
            false,
        ));
    };
    if flake.is_empty() || flake.contains('#') || validate_output("flake output", output).is_err() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "flake installable is not normalized",
            false,
        ));
    }
    Ok(())
}

struct StagedDevelopmentSource {
    directory: PathBuf,
}

impl Drop for StagedDevelopmentSource {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[derive(Default)]
struct StagingBudget {
    bytes: u64,
    entries: usize,
}

fn stage_development_installable(
    installable: &str,
    worker_user: Option<&str>,
    deadline: Instant,
) -> Result<Option<(String, StagedDevelopmentSource)>, ResolutionError> {
    let Some((source, output)) = installable
        .strip_prefix("path:")
        .and_then(|value| value.rsplit_once('#'))
    else {
        return Ok(None);
    };
    let worker_gid = match worker_user {
        Some(worker_user) => user_gid(worker_user).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Internal,
                "development worker user could not be resolved",
                false,
            )
        })?,
        None => unsafe { libc::getegid() },
    };
    let directory = create_staging_directory(worker_gid)?;
    let staged = StagedDevelopmentSource { directory };
    let mut budget = StagingBudget::default();
    copy_staged_tree(
        Path::new(source),
        &staged.directory,
        worker_gid,
        deadline,
        &mut budget,
    )?;
    Ok(Some((
        format!("path:{}#{output}", staged.directory.display()),
        staged,
    )))
}

fn create_staging_directory(worker_gid: libc::gid_t) -> Result<PathBuf, ResolutionError> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    for _ in 0..128 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!(".imageless-source-{}-{nonce}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                set_staged_access(&path, worker_gid, 0o750)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    Err(ResolutionError::new(
        ErrorCategory::Internal,
        "could not allocate embedded source staging directory",
        true,
    ))
}

fn copy_staged_tree(
    source: &Path,
    destination: &Path,
    worker_gid: libc::gid_t,
    deadline: Instant,
    budget: &mut StagingBudget,
) -> Result<(), ResolutionError> {
    let _ = remaining(deadline, "during embedded source staging")?;
    let metadata = std::fs::symlink_metadata(source).map_err(staging_error)?;
    if metadata.file_type().is_symlink() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source contains a symlink",
            false,
        ));
    }
    budget.entries = budget.entries.saturating_add(1);
    if budget.entries > MAX_STAGED_SOURCE_ENTRIES {
        return Err(staging_limit("file-count"));
    }
    if metadata.is_dir() {
        if destination.exists() {
            if !destination.is_dir() {
                return Err(staging_error(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "staging destination collision",
                )));
            }
        } else {
            std::fs::create_dir(destination).map_err(staging_error)?;
        }
        for entry in std::fs::read_dir(source).map_err(staging_error)? {
            let entry = entry.map_err(staging_error)?;
            copy_staged_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                worker_gid,
                deadline,
                budget,
            )?;
        }
        set_staged_access(destination, worker_gid, 0o750)?;
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source contains a non-regular file",
            false,
        ));
    }
    budget.bytes = budget.bytes.saturating_add(metadata.len());
    if budget.bytes > MAX_STAGED_SOURCE_BYTES {
        return Err(staging_limit("byte"));
    }
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(staging_error)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(staging_error)?;
    let copied = io::copy(
        &mut std::io::Read::by_ref(&mut input).take(metadata.len().saturating_add(1)),
        &mut output,
    )
    .map_err(staging_error)?;
    if copied != metadata.len() {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source changed while it was being staged",
            true,
        ));
    }
    output.sync_all().map_err(staging_error)?;
    let mode = if metadata.mode() & 0o111 == 0 {
        0o640
    } else {
        0o750
    };
    set_staged_access(destination, worker_gid, mode)
}

fn set_staged_access(
    path: &Path,
    worker_gid: libc::gid_t,
    mode: u32,
) -> Result<(), ResolutionError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(staging_error)?;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "embedded source path contains a NUL byte",
            false,
        )
    })?;
    if unsafe { libc::chown(path.as_ptr(), effective_uid(), worker_gid) } == -1 {
        return Err(staging_error(io::Error::last_os_error()));
    }
    Ok(())
}

fn user_gid(name: &str) -> io::Result<libc::gid_t> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid user name"))?;
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 16 * 1024];
    let status = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(if status == 0 {
            io::Error::new(io::ErrorKind::NotFound, "worker user does not exist")
        } else {
            io::Error::from_raw_os_error(status)
        });
    }
    Ok(unsafe { entry.assume_init() }.pw_gid)
}

fn staging_error(_error: io::Error) -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::InvalidRequest,
        "embedded source could not be staged safely",
        false,
    )
}

fn staging_limit(kind: &str) -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::InvalidRequest,
        format!("embedded source exceeds the {kind} limit"),
        false,
    )
}

pub fn handle_connection(
    mut stream: UnixStream,
    resolver: &Resolver,
    daemon_uid: u32,
) -> io::Result<()> {
    if !peer_allowed(peer_uid(&stream)?, daemon_uid) {
        return write_frame(
            &mut stream,
            &ResolveResponse::Error {
                version: PROTOCOL_VERSION,
                error: ResolutionError::new(
                    ErrorCategory::Unauthorized,
                    "peer UID does not match resolver UID",
                    false,
                ),
            },
        );
    }
    let request: ResolveRequest = match read_frame(&mut stream) {
        Ok(request) => request,
        Err(_) => {
            return write_frame(
                &mut stream,
                &ResolveResponse::Error {
                    version: PROTOCOL_VERSION,
                    error: ResolutionError::new(
                        ErrorCategory::Protocol,
                        "request frame is invalid",
                        false,
                    ),
                },
            )
        }
    };
    let purpose = request.purpose;
    let response = match if purpose == ResolvePurpose::Inspect {
        resolver
            .inspect(request)
            .map(|(success, closure)| (success, Some(closure)))
    } else {
        resolver
            .resolve_detailed(request)
            .map(|success| (success, None))
    } {
        Ok((success, closure)) => ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(success.resolution),
            timings: success.timings,
            closure: closure.map(Box::new),
        },
        Err(error) => ResolveResponse::Error {
            version: PROTOCOL_VERSION,
            error,
        },
    };
    write_frame(&mut stream, &response)
}

pub fn serve(socket_path: &Path, resolver: Resolver) -> io::Result<()> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket has no parent"))?;
    std::fs::create_dir_all(parent)?;
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(socket_path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refusing to replace a non-socket resolver path",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    let active = Arc::new(AtomicUsize::new(0));
    let daemon_uid = effective_uid();
    for accepted in listener.incoming() {
        let mut stream = accepted?;
        if !peer_uid(&stream).is_ok_and(|uid| peer_allowed(uid, daemon_uid)) {
            let _ = write_frame(
                &mut stream,
                &ResolveResponse::Error {
                    version: PROTOCOL_VERSION,
                    error: ResolutionError::new(
                        ErrorCategory::Unauthorized,
                        "peer UID does not match resolver UID",
                        false,
                    ),
                },
            );
            continue;
        }
        if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::AcqRel);
            let _ = write_frame(
                &mut stream,
                &ResolveResponse::Error {
                    version: PROTOCOL_VERSION,
                    error: ResolutionError::new(
                        ErrorCategory::Overloaded,
                        "resolver connection limit reached",
                        true,
                    ),
                },
            );
            continue;
        }
        let resolver = resolver.clone();
        let active = Arc::clone(&active);
        thread::spawn(move || {
            struct Active(Arc<AtomicUsize>);
            impl Drop for Active {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _active = Active(active);
            let _ = handle_connection(stream, &resolver, daemon_uid);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fake_nix, request, resolver, temporary, RELEASE_REF, STORE};
    use std::os::unix::fs::symlink;
    use std::sync::Barrier;

    #[test]
    fn version_rejection_is_categorized() {
        let dir = temporary("version");
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(ResolveRequest {
                version: 99,
                purpose: ResolvePurpose::Runtime,
                materialize: Materialize::Closure(STORE.into()),
                bundle_path: dir.clone(),
                timeout_ms: 1000,
                container_name: None,
            })
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::Protocol);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prewarm_purpose_is_explicit_and_release_only() {
        let decoded: ResolveRequest = serde_json::from_value(serde_json::json!({
            "version": PROTOCOL_VERSION,
            "materialize": {"kind": "closure", "value": STORE},
            "bundle_path": "/tmp",
            "timeout_ms": 1000
        }))
        .unwrap();
        assert_eq!(decoded.purpose, ResolvePurpose::Runtime);

        let dir = temporary("prewarm-purpose");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(ResolveRequest {
                version: PROTOCOL_VERSION,
                purpose: ResolvePurpose::Prewarm,
                materialize: Materialize::Closure(STORE.into()),
                bundle_path: bundle,
                timeout_ms: 1000,
                container_name: None,
            })
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::InvalidRequest);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unauthorized_issuer_is_rejected_before_any_nix_work() {
        let dir = temporary("issuer-policy");
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(request(
                dir.clone(),
                Materialize::Release(ReleaseReference::parse(RELEASE_REF).unwrap()),
                1000,
            ))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::PolicyDenied);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn development_evaluation_requires_the_unprivileged_worker() {
        let dir = temporary("development-worker-required");
        let nix = fake_nix(&dir, "true");
        let error = resolver(nix, 2, Duration::from_secs(2))
            .resolve(request(
                dir.clone(),
                Materialize::Flake("path:/source#rootfs".to_string()),
                1000,
            ))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::EvaluationDisabled);
        assert!(!dir.join(GC_ROOT_NAME).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn socket_handler_authenticates_peer_and_frames_version_errors() {
        let dir = temporary("socket-protocol");
        let nix = fake_nix(&dir, "true");
        let resolver = resolver(nix, 2, Duration::from_secs(2));

        let (mut client, server) = UnixStream::pair().unwrap();
        let rejected = resolver.clone();
        let unauthorized = thread::spawn(move || {
            handle_connection(server, &rejected, effective_uid().wrapping_add(1)).unwrap()
        });
        let response: ResolveResponse = read_frame(&mut client).unwrap();
        assert!(matches!(
            response,
            ResolveResponse::Error {
                error: ResolutionError {
                    category: ErrorCategory::Unauthorized,
                    ..
                },
                ..
            }
        ));
        unauthorized.join().unwrap();

        let (mut client, server) = UnixStream::pair().unwrap();
        let rejected = resolver.clone();
        let handler =
            thread::spawn(move || handle_connection(server, &rejected, effective_uid()).unwrap());
        write_frame(
            &mut client,
            &ResolveRequest {
                version: PROTOCOL_VERSION + 1,
                purpose: ResolvePurpose::Runtime,
                materialize: Materialize::Closure(STORE.into()),
                bundle_path: dir.clone(),
                timeout_ms: 1000,
                container_name: None,
            },
        )
        .unwrap();
        let response: ResolveResponse = read_frame(&mut client).unwrap();
        assert!(matches!(
            response,
            ResolveResponse::Error {
                error: ResolutionError {
                    category: ErrorCategory::Protocol,
                    ..
                },
                ..
            }
        ));
        handler.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn eight_identical_requests_share_materialization_and_get_eight_roots() {
        let dir = temporary("single-flight");
        let log = dir.join("calls");
        let nix = fake_nix(&dir, "printf x >> \"$FAKE_LOG\"; sleep 0.08");
        let resolver = resolver(nix, 2, Duration::from_secs(5));
        let barrier = Arc::new(Barrier::new(9));
        let bundles: Vec<_> = (0..8)
            .map(|index| {
                let bundle = dir.join(format!("bundle-{index}"));
                std::fs::create_dir(&bundle).unwrap();
                bundle
            })
            .collect();
        let threads: Vec<_> = bundles
            .iter()
            .map(|bundle| {
                let bundle = bundle.clone();
                let resolver = resolver.clone();
                let log = log.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    std::env::set_var("FAKE_STORE", STORE);
                    std::env::set_var("FAKE_LOG", log);
                    barrier.wait();
                    resolver
                        .resolve(request(bundle, Materialize::Closure(STORE.into()), 4000))
                        .unwrap()
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            assert_eq!(thread.join().unwrap().rootfs, STORE);
        }
        // One leader materialization plus seven necessary per-bundle registrations.
        assert_eq!(std::fs::read_to_string(log).unwrap().len(), 8);
        for bundle in &bundles {
            assert_eq!(
                std::fs::read_link(bundle.join(GC_ROOT_NAME)).unwrap(),
                Path::new(STORE)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shared_failure_is_removed_and_a_later_request_retries() {
        let dir = temporary("retry");
        let marker = dir.join("first");
        let nix = fake_nix(
            &dir,
            "if [ ! -e \"$FAIL_MARKER\" ]; then touch \"$FAIL_MARKER\"; sleep 0.1; exit 7; fi",
        );
        std::env::set_var("FAKE_STORE", STORE);
        std::env::set_var("FAIL_MARKER", &marker);
        let resolver = resolver(nix, 2, Duration::from_secs(3));
        let one = dir.join("one");
        let two = dir.join("two");
        std::fs::create_dir(&one).unwrap();
        std::fs::create_dir(&two).unwrap();
        let leader = {
            let resolver = resolver.clone();
            thread::spawn(move || {
                resolver.resolve(request(one, Materialize::Closure(STORE.into()), 2000))
            })
        };
        thread::sleep(Duration::from_millis(20));
        let follower = resolver.resolve(request(
            two.clone(),
            Materialize::Closure(STORE.into()),
            2000,
        ));
        assert_eq!(
            leader.join().unwrap().unwrap_err().category,
            ErrorCategory::Materialization
        );
        assert_eq!(
            follower.unwrap_err().category,
            ErrorCategory::Materialization
        );
        assert_eq!(
            resolver
                .resolve(request(two, Materialize::Closure(STORE.into()), 2000))
                .unwrap()
                .rootfs,
            STORE
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deadlines_cover_materializer_flight_and_semaphore_waiting() {
        let dir = temporary("deadlines");
        let nix = fake_nix(&dir, "sleep 0.3");
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 1, Duration::from_secs(2));
        let one = dir.join("one");
        let two = dir.join("two");
        let three = dir.join("three");
        for bundle in [&one, &two, &three] {
            std::fs::create_dir(bundle).unwrap();
        }
        let leader = {
            let resolver = resolver.clone();
            thread::spawn(move || {
                resolver.resolve(request(one, Materialize::Closure(STORE.into()), 1000))
            })
        };
        thread::sleep(Duration::from_millis(25));
        let flight = resolver
            .resolve(request(two, Materialize::Closure(STORE.into()), 40))
            .unwrap_err();
        assert_eq!(flight.category, ErrorCategory::Timeout);
        let semaphore = resolver
            .resolve(request(
                three,
                Materialize::Closure("/nix/store/11111111111111111111111111111111-other".into()),
                40,
            ))
            .unwrap_err();
        assert_eq!(semaphore.category, ErrorCategory::Timeout);
        leader.join().unwrap().unwrap();

        let four = dir.join("four");
        std::fs::create_dir(&four).unwrap();
        let materializer = resolver
            .resolve(request(four, Materialize::Closure(STORE.into()), 40))
            .unwrap_err();
        assert_eq!(materializer.category, ErrorCategory::Timeout);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn distinct_requests_never_exceed_two_external_nix_operations() {
        let dir = temporary("global-limit");
        let state = dir.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("current"), "0").unwrap();
        std::fs::write(state.join("maximum"), "0").unwrap();
        let body = format!(
            r#"
while ! mkdir "{state}/lock" 2>/dev/null; do sleep 0.005; done
current=$(cat "{state}/current"); current=$((current + 1)); printf '%s' "$current" > "{state}/current"
maximum=$(cat "{state}/maximum"); if [ "$current" -gt "$maximum" ]; then printf '%s' "$current" > "{state}/maximum"; fi
rmdir "{state}/lock"
sleep 0.08
while ! mkdir "{state}/lock" 2>/dev/null; do sleep 0.005; done
current=$(cat "{state}/current"); current=$((current - 1)); printf '%s' "$current" > "{state}/current"
rmdir "{state}/lock"
"#,
            state = state.display()
        );
        let nix = fake_nix(&dir, &body);
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 2, Duration::from_secs(5));
        let threads: Vec<_> = (0..6)
            .map(|index| {
                let bundle = dir.join(format!("distinct-{index}"));
                std::fs::create_dir(&bundle).unwrap();
                let resolver = resolver.clone();
                thread::spawn(move || {
                    let path = format!("/nix/store/{:032}-request-{index}", index + 1);
                    resolver.resolve(request(bundle, Materialize::Closure(path), 4000))
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert_eq!(std::fs::read_to_string(state.join("maximum")).unwrap(), "2");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn follower_gc_root_registration_uses_the_original_deadline() {
        let dir = temporary("root-timeout");
        let leader_started = dir.join("leader-started");
        let nix = fake_nix(
            &dir,
            &format!(
                "case \"$root\" in *slow-root*) sleep 0.4 ;; *leader*) touch {}; sleep 0.08 ;; esac",
                leader_started.display()
            ),
        );
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 2, Duration::from_secs(2));
        let leader_bundle = dir.join("leader");
        let slow_bundle = dir.join("slow-root");
        std::fs::create_dir(&leader_bundle).unwrap();
        std::fs::create_dir(&slow_bundle).unwrap();
        let leader = {
            let resolver = resolver.clone();
            thread::spawn(move || {
                resolver.resolve(request(
                    leader_bundle,
                    Materialize::Closure(STORE.into()),
                    1000,
                ))
            })
        };
        let wait_started = Instant::now();
        while !leader_started.exists() {
            assert!(wait_started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        }
        let error = resolver
            .resolve(request(
                slow_bundle,
                Materialize::Closure(STORE.into()),
                250,
            ))
            .unwrap_err();
        assert_eq!(error.category, ErrorCategory::Timeout);
        assert!(error.diagnostic.contains("GC-root registration"));
        leader.join().unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reserved_root_collision_is_refused_and_stale_symlink_is_replaced() {
        let dir = temporary("roots");
        let nix = fake_nix(&dir, "true");
        std::env::set_var("FAKE_STORE", STORE);
        let resolver = resolver(nix, 1, Duration::from_secs(2));
        let collision = dir.join("collision");
        std::fs::create_dir(&collision).unwrap();
        std::fs::write(collision.join(GC_ROOT_NAME), "reserved").unwrap();
        assert_eq!(
            resolver
                .resolve(request(collision, Materialize::Closure(STORE.into()), 1000,))
                .unwrap_err()
                .category,
            ErrorCategory::RootCollision
        );

        let stale = dir.join("stale");
        std::fs::create_dir(&stale).unwrap();
        symlink(
            "/nix/store/22222222222222222222222222222222-stale",
            stale.join(GC_ROOT_NAME),
        )
        .unwrap();
        resolver
            .resolve(request(
                stale.clone(),
                Materialize::Closure(STORE.into()),
                1000,
            ))
            .unwrap();
        assert_eq!(
            std::fs::read_link(stale.join(GC_ROOT_NAME)).unwrap(),
            Path::new(STORE)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn in_process_resolution_fails_closed_without_a_node_policy() {
        let dir = temporary("in-process-default");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let absent_default = dir.join("policy.json");

        let error = resolve_in_process_at(
            None,
            &absent_default,
            &request(
                bundle.clone(),
                Materialize::Flake("path:/source#rootfs".to_string()),
                1000,
            ),
        )
        .unwrap_err();
        assert_eq!(error.category, ErrorCategory::EvaluationDisabled);
        assert!(error
            .diagnostic
            .contains(&format!("write {}", absent_default.display())));
        assert!(!bundle.join(GC_ROOT_NAME).exists());

        // An explicitly configured policy path must load; it never defaults.
        let explicit = resolve_in_process_at(
            Some(&dir.join("missing-explicit.json")),
            &absent_default,
            &request(
                bundle,
                Materialize::Flake("path:/source#rootfs".to_string()),
                1000,
            ),
        )
        .unwrap_err();
        assert_eq!(explicit.category, ErrorCategory::PolicyDenied);
        assert!(explicit.diagnostic.contains("could not be loaded"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn in_process_policy_enables_caller_evaluation_without_a_worker() {
        let dir = temporary("in-process-eval");
        let bundle = dir.join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let source = dir.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("flake.nix"), "{ outputs = _: { }; }").unwrap();

        let policy = dir.join("policy.json");
        std::fs::write(
            &policy,
            r#"{"system":"x86_64-linux","cache_only":false,"eval_allowed_uri_prefixes":["path:"],"issuers":{}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();

        let log = dir.join("eval-args");
        let eval_nix = dir.join("fake-eval-nix");
        crate::testutil::executable(
            &eval_nix,
            &format!(
                "printf '%s\\n' \"$@\" >> {log}\nprintf '%s\\n' {STORE}",
                log = log.display()
            ),
        );
        std::env::set_var("IMAGELESS_NIX", &eval_nix);
        std::env::set_var("IMAGELESS_NIX_STORE", fake_nix(&dir, "true"));
        std::env::set_var("FAKE_STORE", STORE);

        let success = resolve_in_process_at(
            None,
            &policy,
            &request(
                bundle.clone(),
                Materialize::Flake(format!("path:{}#rootfs", source.display())),
                5000,
            ),
        )
        .unwrap();
        std::env::remove_var("IMAGELESS_NIX");
        std::env::remove_var("IMAGELESS_NIX_STORE");

        assert_eq!(success.resolution.rootfs, STORE);
        assert_eq!(success.resolution.identity, "development-source");
        assert_eq!(
            std::fs::read_link(bundle.join(GC_ROOT_NAME)).unwrap(),
            Path::new(STORE)
        );
        let arguments = std::fs::read_to_string(&log).unwrap();
        // Evaluation ran as the caller (plain `nix build`, no worker flags) on
        // a staged copy of the source, with no imageless-dev user involved.
        assert!(arguments.contains("--print-out-paths"));
        assert!(!arguments.contains("--user"));
        assert!(arguments.contains(".imageless-source-"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
