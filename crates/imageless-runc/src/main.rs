//! runc-compatible imageless interposer for Docker and other OCI callers.

use imageless::{export_timing_events, prepare_bundle, remove_bundle_gc_roots, PrepareBundle};
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

/// runc global flags that consume a following value when written as
/// `--flag value` (the `--flag=value` form carries its own value). Every other
/// pre-subcommand token starting with `-` is a boolean switch (`--debug`,
/// `--systemd-cgroup`, ...). This table exists only to walk past the global
/// prefix and land on the subcommand token.
const GLOBAL_VALUE_FLAGS: &[&str] = &["--root", "--log", "--log-format", "--criu", "--rootless"];

/// Index of the runc subcommand: the first argv element that is neither a
/// global flag nor the value of one. Searching argv for the literal string
/// `create` instead would mis-locate the subcommand whenever a container ID,
/// bundle path, or log path is itself named `create` — and then every later
/// token, including a real `--bundle`, is parsed against the wrong offset.
fn subcommand_index(arguments: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        if argument == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if !argument.starts_with('-') {
            return Some(index);
        }
        index += if GLOBAL_VALUE_FLAGS.contains(&argument) {
            2
        } else {
            1
        };
    }
    None
}

fn create_bundle(arguments: &[String]) -> Result<Option<PathBuf>, String> {
    let Some(create_index) = subcommand_index(arguments) else {
        return Ok(None);
    };
    if arguments[create_index] != "create" {
        return Ok(None);
    }
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
        let mut prepare = PrepareBundle::new(bundle.join("config.json"), bundle);
        prepare.default_output = default_output();
        prepare.timeout_seconds = timeout_seconds().unwrap_or_else(|error| fail(error, 1));
        let prepare_started = Instant::now();
        let resolution = prepare_bundle(&prepare).unwrap_or_else(|error| {
            // The sink recorded successes only: every failure exited here,
            // before the export below. A node that never starts a pod is
            // exactly the node an operator needs a record from, and the
            // duration is the useful half — it separates a fast refusal
            // (policy, a contract error) from a create that burned its whole
            // deadline. No release identity exists yet, so the event is named
            // for the stage rather than for a release never selected.
            if let Some(path) = nonempty(TELEMETRY_ENV) {
                let _ = export_timing_events(
                    Path::new(&path),
                    "unresolved",
                    &[("preparation", elapsed_us(prepare_started))],
                    Some("error"),
                );
            }
            // "selection" named one stage of several: a substitution timeout
            // read as a selection problem. Nothing in the tree or in CI greps
            // this string.
            fail(format_args!("bundle preparation failed: {error}"), 1)
        });
        if let Some(resolution) = resolution {
            if let Some(path) = nonempty(TELEMETRY_ENV) {
                let timings = &resolution.timings;
                let _ = export_timing_events(
                    Path::new(&path),
                    &resolution.resolution.identity,
                    // The four original stages keep their names, their order,
                    // and their spans; the rest are carved out of two of them,
                    // so `selection + policy_verification + substitution +
                    // rewrite` still totals the create and the finer stages
                    // explain where two of those went rather than adding to
                    // them. Anything summing these must pick one set.
                    &[
                        ("selection", timings.selection_us),
                        ("policy_verification", timings.policy_verification_us),
                        ("substitution", timings.substitution_us),
                        ("rewrite", timings.rewrite_us),
                        ("manifest_fetch", timings.manifest_fetch_us),
                        ("staging", timings.staging_us),
                        ("evaluation", timings.evaluation_us),
                        ("root_registration", timings.root_registration_us),
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
    fn a_container_id_named_create_is_not_the_subcommand() {
        // `runc start create` starts a container whose ID is "create". The
        // shim must not treat it as a create, and must not misread the
        // remaining argv against that offset.
        assert_eq!(create_bundle(&args(&["start", "create"])).unwrap(), None);
        assert_eq!(create_bundle(&args(&["delete", "create"])).unwrap(), None);
        assert_eq!(
            create_bundle(&args(&["exec", "create", "--bundle", "/not-a-bundle"])).unwrap(),
            None
        );
        // A global flag whose value is "create" likewise names no subcommand.
        assert_eq!(
            create_bundle(&args(&["--root", "create", "state", "id"])).unwrap(),
            None
        );
        // ...and a real create with a bundle path named "create" still parses.
        assert_eq!(
            create_bundle(&args(&[
                "--root", "create", "create", "--bundle", "/b", "create"
            ]))
            .unwrap(),
            Some(PathBuf::from("/b"))
        );
    }

    #[test]
    fn walks_past_global_flag_forms() {
        for arguments in [
            args(&[
                "--debug",
                "--systemd-cgroup",
                "create",
                "-b",
                "/bundle",
                "id",
            ]),
            args(&["--log=/tmp/log", "create", "-b", "/bundle", "id"]),
            args(&[
                "--log", "/tmp/log", "--debug", "create", "-b", "/bundle", "id",
            ]),
            args(&["--rootless", "true", "create", "-b", "/bundle", "id"]),
        ] {
            assert_eq!(
                create_bundle(&arguments).unwrap(),
                Some(PathBuf::from("/bundle"))
            );
        }
        // Global flags with no subcommand at all.
        assert_eq!(create_bundle(&args(&["--version"])).unwrap(), None);
        assert_eq!(create_bundle(&args(&[])).unwrap(), None);
        assert_eq!(create_bundle(&args(&["--root"])).unwrap(), None);
    }

    #[test]
    fn rejects_empty_bundle_flag() {
        assert!(create_bundle(&args(&["create", "--bundle"])).is_err());
        assert!(create_bundle(&args(&["create", "--bundle="])).is_err());
    }
}
