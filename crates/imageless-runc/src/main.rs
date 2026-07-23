//! runc-compatible imageless interposer for Docker and other OCI callers.

use imageless::{
    export_timing_events, remove_bundle_gc_roots, resolve_and_apply_bundle_detailed,
    MaterializerConfig,
};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;

const DELEGATE_ENV: &str = "IMAGELESS_RUNC";
const DELEGATE_BAKED: &str = match option_env!("IMAGELESS_RUNC") {
    Some(path) => path,
    None => "runc",
};
const TIMEOUT_ENV: &str = "IMAGELESS_REALIZATION_TIMEOUT_SECONDS";
const TIMEOUT_BAKED: &str = match option_env!("IMAGELESS_REALIZATION_TIMEOUT_SECONDS") {
    Some(value) => value,
    None => "300",
};
const OUTPUT_ENV: &str = "IMAGELESS_DEFAULT_OUTPUT";
const OUTPUT_BAKED: &str = match option_env!("IMAGELESS_DEFAULT_OUTPUT") {
    Some(value) => value,
    None => "rootfs",
};
const TELEMETRY_ENV: &str = "IMAGELESS_TELEMETRY_PATH";

fn nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn delegate() -> String {
    nonempty(DELEGATE_ENV).unwrap_or_else(|| DELEGATE_BAKED.to_string())
}

fn timeout_seconds() -> Result<u64, String> {
    let raw = nonempty(TIMEOUT_ENV).unwrap_or_else(|| TIMEOUT_BAKED.to_string());
    let seconds = raw
        .parse::<u64>()
        .map_err(|_| format!("{TIMEOUT_ENV} must be an integer number of seconds"))?;
    if !(1..=3600).contains(&seconds) {
        return Err(format!("{TIMEOUT_ENV} must be between 1 and 3600"));
    }
    Ok(seconds)
}

fn default_output() -> String {
    nonempty(OUTPUT_ENV).unwrap_or_else(|| OUTPUT_BAKED.to_string())
}

fn create_bundle(arguments: &[String]) -> Result<Option<PathBuf>, String> {
    let Some(create_index) = arguments.iter().position(|argument| argument == "create") else {
        return Ok(None);
    };
    let command_arguments = &arguments[create_index + 1..];
    for (index, argument) in command_arguments.iter().enumerate() {
        if argument == "--bundle" || argument == "-b" {
            return command_arguments
                .get(index + 1)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(Some)
                .ok_or_else(|| format!("{argument} requires a path"));
        }
        if let Some(value) = argument
            .strip_prefix("--bundle=")
            .or_else(|| argument.strip_prefix("-b="))
        {
            if value.is_empty() {
                return Err(format!("{argument} requires a path"));
            }
            return Ok(Some(PathBuf::from(value)));
        }
    }
    Ok(Some(PathBuf::from(".")))
}

fn canonical_bundle(arguments: &[String]) -> Result<Option<PathBuf>, String> {
    create_bundle(arguments)?
        .map(|bundle| {
            std::fs::canonicalize(&bundle)
                .map_err(|error| format!("canonicalize bundle {}: {error}", bundle.display()))
        })
        .transpose()
}

fn run_delegate(program: &str, arguments: &[String]) -> std::io::Result<ExitStatus> {
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

fn exit_like(status: ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    std::process::exit(128 + status.signal().unwrap_or(1));
}

fn fail(message: impl std::fmt::Display, code: i32) -> ! {
    eprintln!("imageless-runc: {message}");
    std::process::exit(code);
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let bundle = canonical_bundle(&arguments).unwrap_or_else(|error| fail(error, 1));
    let mut applied = None;

    if let Some(bundle) = &bundle {
        let timeout = timeout_seconds().unwrap_or_else(|error| fail(error, 1));
        let resolution = resolve_and_apply_bundle_detailed(
            &bundle.join("config.json"),
            bundle,
            &default_output(),
            timeout,
            &MaterializerConfig::from_environment(),
        )
        .unwrap_or_else(|error| fail(format_args!("bundle selection failed: {error}"), 1));
        if let Some(resolution) = resolution {
            if let Some(path) = nonempty(TELEMETRY_ENV) {
                let timings = &resolution.timings;
                let _ = export_timing_events(
                    Path::new(&path),
                    &resolution.resolution.identity,
                    &[
                        ("selection", timings.selection_us),
                        ("policy_verification", timings.policy_verification_us),
                        ("substitution", timings.substitution_us),
                        ("rewrite", timings.rewrite_us),
                    ],
                    Some("success"),
                );
            }
            applied = Some(resolution);
        }
    }

    let delegate = delegate();
    let delegate_started = Instant::now();
    let status = match run_delegate(&delegate, &arguments) {
        Ok(status) => status,
        Err(error) => {
            if let Some(bundle) = &bundle {
                if applied.is_some() {
                    let _ = remove_bundle_gc_roots(bundle);
                }
            }
            fail(format_args!("execute {delegate}: {error}"), 127)
        }
    };
    if let (Some(path), Some(resolution)) = (nonempty(TELEMETRY_ENV), &applied) {
        let _ = export_timing_events(
            Path::new(&path),
            &resolution.resolution.identity,
            &[("delegate_startup", elapsed_us(delegate_started))],
            Some(if status.success() { "success" } else { "error" }),
        );
    }
    if !status.success() {
        if let Some(bundle) = &bundle {
            if applied.is_some() {
                let _ = remove_bundle_gc_roots(bundle);
            }
        }
    }
    exit_like(status);
}

#[cfg(test)]
mod tests {
    use super::create_bundle;
    use std::path::PathBuf;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn only_create_selects_a_bundle() {
        assert_eq!(create_bundle(&args(&["start", "id"])).unwrap(), None);
        assert_eq!(
            create_bundle(&args(&["--root", "/run/runc", "create", "id"])).unwrap(),
            Some(PathBuf::from("."))
        );
    }

    #[test]
    fn parses_runc_bundle_flag_forms() {
        for arguments in [
            args(&["create", "--bundle", "/bundle", "id"]),
            args(&["create", "-b", "/bundle", "id"]),
            args(&["create", "--bundle=/bundle", "id"]),
            args(&["create", "-b=/bundle", "id"]),
        ] {
            assert_eq!(
                create_bundle(&arguments).unwrap(),
                Some(PathBuf::from("/bundle"))
            );
        }
    }

    #[test]
    fn rejects_empty_bundle_flag() {
        assert!(create_bundle(&args(&["create", "--bundle"])).is_err());
        assert!(create_bundle(&args(&["create", "--bundle="])).is_err());
    }
}
