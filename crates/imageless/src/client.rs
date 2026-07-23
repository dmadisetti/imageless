//! Length-prefixed JSON frame protocol and the resolver-socket client.

use crate::materialize::{
    ClosureReport, ErrorCategory, ResolutionError, ResolutionSuccess, ResolvePurpose,
    ResolveRequest, ResolveResponse,
};
use crate::release::ResolvedRelease;
use crate::spec::validate_store_path;
use crate::{to_io, MAX_FRAME_BYTES, PROTOCOL_VERSION};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(to_io)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("protocol frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protocol frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn request_resolution(
    socket_path: &Path,
    request: &ResolveRequest,
) -> Result<ResolvedRelease, ResolutionError> {
    request_resolution_detailed(socket_path, request).map(|success| success.resolution)
}

pub fn request_resolution_detailed(
    socket_path: &Path,
    request: &ResolveRequest,
) -> Result<ResolutionSuccess, ResolutionError> {
    if request.purpose == ResolvePurpose::Inspect {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "inspection requests must use the inspection client",
            false,
        ));
    }
    let (success, closure) = request_resolver(socket_path, request)?;
    if closure.is_some() {
        return Err(ResolutionError::new(
            ErrorCategory::Protocol,
            "resolver returned an unexpected closure report",
            false,
        ));
    }
    Ok(success)
}

pub fn request_inspection(
    socket_path: &Path,
    request: &ResolveRequest,
) -> Result<(ResolutionSuccess, ClosureReport), ResolutionError> {
    if request.purpose != ResolvePurpose::Inspect {
        return Err(ResolutionError::new(
            ErrorCategory::InvalidRequest,
            "inspection client requires an inspection request",
            false,
        ));
    }
    let (success, closure) = request_resolver(socket_path, request)?;
    let closure = closure.ok_or_else(|| {
        ResolutionError::new(
            ErrorCategory::Protocol,
            "resolver omitted the closure report",
            false,
        )
    })?;
    Ok((success, closure))
}

fn request_resolver(
    socket_path: &Path,
    request: &ResolveRequest,
) -> Result<(ResolutionSuccess, Option<ClosureReport>), ResolutionError> {
    let timeout = Duration::from_millis(request.timeout_ms.max(1));
    let mut stream = UnixStream::connect(socket_path).map_err(|_| {
        ResolutionError::new(ErrorCategory::Unavailable, "resolver is unavailable", true)
    })?;
    if peer_uid(&stream).map_err(|_| {
        ResolutionError::new(
            ErrorCategory::Unauthorized,
            "could not authenticate resolver",
            false,
        )
    })? != effective_uid()
    {
        return Err(ResolutionError::new(
            ErrorCategory::Unauthorized,
            "resolver UID does not match shim UID",
            false,
        ));
    }
    stream.set_read_timeout(Some(timeout)).map_err(client_io)?;
    stream.set_write_timeout(Some(timeout)).map_err(client_io)?;
    write_frame(&mut stream, request).map_err(client_io)?;
    let response: ResolveResponse = read_frame(&mut stream).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            ResolutionError::timeout("while communicating with the resolver")
        } else if error.kind() == io::ErrorKind::InvalidData {
            ResolutionError::new(
                ErrorCategory::Protocol,
                "resolver returned an invalid protocol frame",
                false,
            )
        } else {
            client_io(error)
        }
    })?;
    match response {
        ResolveResponse::Success {
            version,
            resolution,
            timings,
            closure,
        } => {
            if version != PROTOCOL_VERSION {
                return Err(ResolutionError::new(
                    ErrorCategory::Protocol,
                    "resolver returned an unsupported protocol version",
                    false,
                ));
            }
            validate_store_path("resolver rootfs", &resolution.rootfs).map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::Protocol,
                    "resolver returned an invalid rootfs path",
                    false,
                )
            })?;
            for mount in &resolution.mounts {
                validate_store_path("resolver mount source", &mount.source).map_err(|_| {
                    ResolutionError::new(
                        ErrorCategory::Protocol,
                        "resolver returned an invalid mount source",
                        false,
                    )
                })?;
            }
            if let Some(report) = &closure {
                validate_closure_report(report, &resolution.identity)?;
            }
            Ok((
                ResolutionSuccess {
                    resolution: *resolution,
                    timings,
                },
                closure.map(|report| *report),
            ))
        }
        ResolveResponse::Error { version, error } => {
            if version != PROTOCOL_VERSION {
                Err(ResolutionError::new(
                    ErrorCategory::Protocol,
                    "resolver returned an unsupported protocol version",
                    false,
                ))
            } else {
                Err(error)
            }
        }
    }
}

fn validate_closure_report(
    report: &ClosureReport,
    expected_release: &str,
) -> Result<(), ResolutionError> {
    if report.schema != "imageless.closure-report.v1"
        || report.release != expected_release
        || report.closure_paths.is_empty()
    {
        return Err(ResolutionError::new(
            ErrorCategory::Protocol,
            "resolver returned an invalid closure report",
            false,
        ));
    }
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    let mut missing = 0_u64;
    for item in &report.closure_paths {
        validate_store_path("closure report path", &item.path).map_err(|_| {
            ResolutionError::new(
                ErrorCategory::Protocol,
                "resolver returned an invalid closure path",
                false,
            )
        })?;
        if !seen.insert(&item.path) {
            return Err(ResolutionError::new(
                ErrorCategory::Protocol,
                "resolver returned a duplicate closure path",
                false,
            ));
        }
        total = total.checked_add(item.nar_bytes).ok_or_else(|| {
            ResolutionError::new(
                ErrorCategory::Protocol,
                "resolver closure size overflowed",
                false,
            )
        })?;
        if !item.present {
            missing = missing.checked_add(item.download_bytes).ok_or_else(|| {
                ResolutionError::new(
                    ErrorCategory::Protocol,
                    "resolver download estimate overflowed",
                    false,
                )
            })?;
        }
    }
    if total != report.total_nar_bytes || missing != report.missing_download_bytes {
        return Err(ResolutionError::new(
            ErrorCategory::Protocol,
            "resolver closure totals are inconsistent",
            false,
        ));
    }
    Ok(())
}

fn client_io(_: io::Error) -> ResolutionError {
    ResolutionError::new(
        ErrorCategory::Unavailable,
        "resolver communication failed",
        true,
    )
}

pub fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(credentials.uid)
    }
}

pub fn peer_allowed(peer: u32, daemon: u32) -> bool {
    peer == daemon
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::materialize::ResolutionTimings;
    use crate::testutil::resolved;

    #[test]
    fn protocol_round_trip_rejects_oversize_and_auth_mismatch() {
        let value = ResolveResponse::Success {
            version: PROTOCOL_VERSION,
            resolution: Box::new(resolved()),
            timings: ResolutionTimings::default(),
            closure: None,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).unwrap();
        assert_eq!(
            read_frame::<ResolveResponse>(&mut bytes.as_slice()).unwrap(),
            value
        );

        let mut oversized = Vec::from(((MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
        oversized.resize(8, 0);
        assert_eq!(
            read_frame::<ResolveRequest>(&mut oversized.as_slice())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let too_large = ResolveResponse::Error {
            version: PROTOCOL_VERSION,
            error: ResolutionError::new(
                ErrorCategory::Internal,
                "x".repeat(MAX_FRAME_BYTES),
                false,
            ),
        };
        assert_eq!(
            write_frame(&mut Vec::new(), &too_large).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(1000, 0));
    }
}
