//! Materialization request/response types shared by clients, adapters, and the
//! resolver daemon.

use crate::release::{ReleaseReference, ResolvedRelease};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Materialize {
    Closure(String),
    Flake(String),
    Release(ReleaseReference),
}

impl Materialize {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Closure(path) => format!("closure:{path}"),
            Self::Flake(installable) => format!("flake:{installable}"),
            Self::Release(reference) => format!("release:{}", reference.identity()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolveRequest {
    pub version: u32,
    #[serde(default, skip_serializing_if = "ResolvePurpose::is_runtime")]
    pub purpose: ResolvePurpose,
    pub materialize: Materialize,
    pub bundle_path: PathBuf,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvePurpose {
    #[default]
    Runtime,
    Prewarm,
    Inspect,
}

impl ResolvePurpose {
    fn is_runtime(&self) -> bool {
        matches!(self, Self::Runtime)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCategory {
    Protocol,
    Unauthorized,
    InvalidRequest,
    Unavailable,
    Overloaded,
    Timeout,
    Materialization,
    RootCollision,
    RootRegistration,
    Internal,
    ManifestFetch,
    DigestMismatch,
    PolicyDenied,
    ArchitectureMismatch,
    SpecConflict,
    EvaluationDisabled,
    CacheQuery,
}

impl ErrorCategory {
    /// The wire spelling, for the places a category has to sit inside a line of
    /// prose rather than in a JSON field of its own. A test holds this in step
    /// with the `snake_case` rename above, because a reader correlating a pod
    /// event against a telemetry record matches these strings by eye.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Unauthorized => "unauthorized",
            Self::InvalidRequest => "invalid_request",
            Self::Unavailable => "unavailable",
            Self::Overloaded => "overloaded",
            Self::Timeout => "timeout",
            Self::Materialization => "materialization",
            Self::RootCollision => "root_collision",
            Self::RootRegistration => "root_registration",
            Self::Internal => "internal",
            Self::ManifestFetch => "manifest_fetch",
            Self::DigestMismatch => "digest_mismatch",
            Self::PolicyDenied => "policy_denied",
            Self::ArchitectureMismatch => "architecture_mismatch",
            Self::SpecConflict => "spec_conflict",
            Self::EvaluationDisabled => "evaluation_disabled",
            Self::CacheQuery => "cache_query",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolutionError {
    pub category: ErrorCategory,
    pub diagnostic: String,
    pub retryable: bool,
}

impl ResolutionError {
    pub fn new(category: ErrorCategory, diagnostic: impl Into<String>, retryable: bool) -> Self {
        Self {
            category,
            diagnostic: diagnostic.into(),
            retryable,
        }
    }

    pub(crate) fn timeout(stage: &str) -> Self {
        Self::new(
            ErrorCategory::Timeout,
            format!("request deadline exceeded {stage}"),
            true,
        )
    }

    /// A timeout that carries what the underlying Nix process last said.
    ///
    /// The stage alone cannot distinguish a materializer wedged on its first
    /// store path from one that was copying its six-hundredth when the deadline
    /// arrived, and those call for opposite responses. The non-timeout arms
    /// have always interpolated their error; only this one threw it away.
    pub(crate) fn timeout_with(stage: &str, detail: &str) -> Self {
        Self::new(
            ErrorCategory::Timeout,
            format!("request deadline exceeded {stage}: {detail}"),
            true,
        )
    }
}

impl std::fmt::Display for ResolutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?}: {} (retryable: {})",
            self.category, self.diagnostic, self.retryable
        )
    }
}

impl std::error::Error for ResolutionError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResolveResponse {
    Success {
        version: u32,
        resolution: Box<ResolvedRelease>,
        #[serde(default, skip_serializing_if = "ResolutionTimings::is_zero")]
        timings: ResolutionTimings,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closure: Option<Box<ClosureReport>>,
    },
    Error {
        version: u32,
        error: ResolutionError,
    },
}

/// Where the time inside one resolution went.
///
/// The two original buckets each answered a different question than their name
/// suggested: `policy_verification_us` spanned `select_release`, which fetches
/// the release manifest, so a slow catalog read as a slow policy check; and
/// `substitution_us` ran from the end of selection to the end of the flight,
/// folding staging, the permit wait, evaluation, realisation and GC-root
/// registration into one integer. Both keep their names and their outer spans —
/// the finer fields are carved out of them, so a reader comparing two nodes
/// across this change is comparing the same quantity.
///
/// Every field added here is optional on the wire: a shim and a daemon are
/// upgraded independently, and a resolution is not worth failing over a
/// telemetry field.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolutionTimings {
    pub policy_verification_us: u64,
    pub substitution_us: u64,
    /// Fetching the release manifest — a network round trip on an HTTPS issuer,
    /// and a file read on a local one. Carved out of `policy_verification_us`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub manifest_fetch_us: u64,
    /// Copying the embedded source out of the image's rootfs. Development
    /// sources only; zero for a release and for an external reference, which is
    /// evaluated where it stands.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub staging_us: u64,
    /// The Nix process itself: `build` for a development source, `--realise`
    /// for a release or closure. Carved out of `substitution_us`, and the field
    /// most likely to explain a create that spent minutes.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub evaluation_us: u64,
    /// Registering the GC root that keeps the rootfs alive. A follower on a
    /// coalesced flight pays this and nothing else.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub root_registration_us: u64,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

impl ResolutionTimings {
    fn is_zero(&self) -> bool {
        // Every field, or a response carrying only the newer ones would be
        // elided whole and reach the shim as a default.
        self.policy_verification_us == 0
            && self.substitution_us == 0
            && self.manifest_fetch_us == 0
            && self.staging_us == 0
            && self.evaluation_us == 0
            && self.root_registration_us == 0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolutionSuccess {
    pub resolution: ResolvedRelease,
    pub timings: ResolutionTimings,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClosurePathReport {
    pub path: String,
    pub nar_bytes: u64,
    pub download_bytes: u64,
    pub present: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClosureReport {
    pub schema: String,
    pub release: String,
    pub closure_paths: Vec<ClosurePathReport>,
    pub total_nar_bytes: u64,
    pub missing_download_bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ContractError {
    pub(crate) field: &'static str,
    pub(crate) reason: String,
}

impl ContractError {
    pub(crate) fn new(field: &'static str, reason: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid {}: {}", self.field, self.reason)
    }
}

impl std::error::Error for ContractError {}

pub(crate) fn remaining(deadline: Instant, stage: &str) -> Result<Duration, ResolutionError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| ResolutionError::timeout(stage))
}

pub(crate) fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node runs one shim and one daemon, and nothing upgrades them together.
    /// A daemon that predates these fields sends a response without them, and a
    /// resolution is not worth failing over a telemetry field.
    #[test]
    fn timings_added_since_an_older_daemon_default_rather_than_fail() {
        let older: ResolutionTimings =
            serde_json::from_str(r#"{"policy_verification_us":12,"substitution_us":34}"#).unwrap();
        assert_eq!(older.policy_verification_us, 12);
        assert_eq!(older.substitution_us, 34);
        assert_eq!(older.manifest_fetch_us, 0);
        assert_eq!(older.staging_us, 0);
        assert_eq!(older.evaluation_us, 0);
        assert_eq!(older.root_registration_us, 0);
    }

    /// The other direction: a newer daemon's extra fields must not trip an
    /// older shim's decoder.
    #[test]
    fn a_newer_daemons_extra_timing_fields_are_ignored_not_rejected() {
        let decoded: ResolutionTimings = serde_json::from_str(
            r#"{"policy_verification_us":1,"substitution_us":2,"a_stage_from_the_future_us":3}"#,
        )
        .unwrap();
        assert_eq!(decoded.substitution_us, 2);
    }

    /// `as_str` is a second spelling of the same category, and two spellings
    /// drift. A pod event carries one and a telemetry record the other, so a
    /// reader correlating them is relying on this holding.
    #[test]
    fn every_category_spells_itself_as_it_serializes() {
        for category in [
            ErrorCategory::Protocol,
            ErrorCategory::Unauthorized,
            ErrorCategory::InvalidRequest,
            ErrorCategory::Unavailable,
            ErrorCategory::Overloaded,
            ErrorCategory::Timeout,
            ErrorCategory::Materialization,
            ErrorCategory::RootCollision,
            ErrorCategory::RootRegistration,
            ErrorCategory::Internal,
            ErrorCategory::ManifestFetch,
            ErrorCategory::DigestMismatch,
            ErrorCategory::PolicyDenied,
            ErrorCategory::ArchitectureMismatch,
            ErrorCategory::SpecConflict,
            ErrorCategory::EvaluationDisabled,
            ErrorCategory::CacheQuery,
        ] {
            let serialized = serde_json::to_string(&category).unwrap();
            assert_eq!(serialized, format!("\"{}\"", category.as_str()));
        }
    }

    /// The zero-elision is what keeps an all-default `timings` off the wire, so
    /// it has to consider every field — a response carrying only the newer ones
    /// would otherwise be elided whole and arrive as a default.
    #[test]
    fn a_response_carrying_only_the_newer_timings_survives_a_round_trip() {
        let timings = ResolutionTimings {
            evaluation_us: 7,
            ..ResolutionTimings::default()
        };
        assert!(!timings.is_zero());
        let response = ResolveResponse::Success {
            version: crate::PROTOCOL_VERSION,
            resolution: Box::new(ResolvedRelease {
                identity: "test".to_string(),
                rootfs: "/nix/store/x".to_string(),
                process: None,
                mounts: Vec::new(),
            }),
            timings: timings.clone(),
            closure: None,
        };
        let encoded = serde_json::to_string(&response).unwrap();
        let ResolveResponse::Success { timings: back, .. } =
            serde_json::from_str::<ResolveResponse>(&encoded).unwrap()
        else {
            panic!("a success response must decode as one");
        };
        assert_eq!(back, timings);
    }
}
