//! Bounded Nix subprocess execution and output validation.

use crate::spec::validate_store_path;
use crate::MAX_CAPTURE_BYTES;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn parse_materialized_path(stdout: &str) -> io::Result<String> {
    let paths: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let path = match paths.as_slice() {
        [path] => (*path).to_string(),
        [] => return Err(io::Error::other("Nix produced no store path")),
        _ => return Err(io::Error::other("Nix produced multiple store paths")),
    };
    validate_store_path("materializer output", &path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(path)
}

/// `nix-store --realise PATH --add-root ROOT` has emitted both forms across
/// supported Nix versions: some print the realized store path, while current
/// Nix prints the indirect root path. Accept exactly one matching line and rely
/// on `validate_registered_root` to authenticate the resulting symlink target.
/// Multiple or unrelated paths remain fail-closed.
pub(crate) fn validate_realise_output(
    stdout: &str,
    store_path: &str,
    root: &Path,
) -> io::Result<()> {
    let paths: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let path = match paths.as_slice() {
        [path] => *path,
        [] => return Err(io::Error::other("Nix produced no realization path")),
        _ => return Err(io::Error::other("Nix produced multiple realization paths")),
    };
    if path == store_path || root.to_str() == Some(path) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Nix produced an unrelated realization path",
        ))
    }
}

pub(crate) fn run_command(command: &mut Command, timeout: Duration) -> io::Result<String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                libc::_exit(125);
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("materializer stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("materializer stderr was not captured"))?;
    let stdout_reader = thread::spawn(move || capture_output(stdout));
    let stderr_reader = thread::spawn(move || capture_output(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_capture(stdout_reader);
            let _ = join_capture(stderr_reader);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Nix operation timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    // A helper can outlive the top-level Nix process while retaining an
    // inherited stdout/stderr fd. Keep pipe draining inside the same deadline;
    // otherwise joining the readers could wait forever after `try_wait` reports
    // the parent exited.
    while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
        if started.elapsed() >= timeout {
            kill_process_group(child.id());
            let _ = join_capture(stdout_reader);
            let _ = join_capture(stderr_reader);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Nix operation output did not close before the deadline",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let stdout = join_capture(stdout_reader)?;
    let stderr = join_capture(stderr_reader)?;
    if stdout.truncated {
        return Err(io::Error::other("Nix stdout exceeded the capture limit"));
    }
    if !status.success() {
        let _ = stderr;
        return Err(io::Error::other("Nix operation failed"));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn capture_output(mut pipe: impl Read) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = pipe.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok(CapturedOutput { bytes, truncated })
}

fn join_capture(
    handle: thread::JoinHandle<io::Result<CapturedOutput>>,
) -> io::Result<CapturedOutput> {
    handle
        .join()
        .map_err(|_| io::Error::other("materializer output reader panicked"))?
}

pub(crate) fn kill_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::STORE;

    #[test]
    fn realise_output_accepts_store_or_indirect_root_but_rejects_ambiguity() {
        let root = Path::new("/tmp/imageless-test/.imageless-rootfs-gcroot");
        validate_realise_output(&format!("{STORE}\n"), STORE, root).unwrap();
        validate_realise_output(
            "/tmp/imageless-test/.imageless-rootfs-gcroot\n",
            STORE,
            root,
        )
        .unwrap();
        assert!(validate_realise_output("/tmp/unrelated\n", STORE, root).is_err());
        assert!(validate_realise_output(
            &format!("{STORE}\n/tmp/imageless-test/.imageless-rootfs-gcroot\n"),
            STORE,
            root,
        )
        .is_err());
    }

    #[test]
    fn command_deadline_includes_inherited_output_pipes() {
        let started = Instant::now();
        let error = run_command(
            Command::new("sh").args(["-c", "sleep 30 & exit 0"]),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
