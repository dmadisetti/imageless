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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolutionTimings {
    pub policy_verification_us: u64,
    pub substitution_us: u64,
}

impl ResolutionTimings {
    fn is_zero(&self) -> bool {
        self.policy_verification_us == 0 && self.substitution_us == 0
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
