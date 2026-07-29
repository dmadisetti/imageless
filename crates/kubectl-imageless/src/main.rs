//! kubectl plugin front end: hand-rolled argv, no framework.
//!
//! `kubectl imageless run ./dir --repo HOST/REPO --dry-run -- CMD` packs the
//! directory into a seed OCI image exactly as the node would stage it, prints
//! the layer/config/manifest digests on stderr, and prints the pod manifest
//! on stdout so it pipes straight into `kubectl apply -f -`.

mod oci;
mod pack;
mod podspec;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("run") => match parse_run(&arguments[1..]) {
            Ok(ParsedRun::Help) => help(),
            Ok(ParsedRun::Run(options)) => run(&options),
            Err(error) => {
                eprintln!("kubectl-imageless: {error}\n");
                usage()
            }
        },
        Some("version") => {
            println!("kubectl-imageless {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("help") | Some("--help") | Some("-h") => help(),
        Some(other) => {
            eprintln!("kubectl-imageless: unknown command `{other}`\n");
            usage()
        }
        None => usage(),
    }
}

const USAGE: &str = "kubectl imageless — run a directory on an imageless cluster\n\
     \n\
     Usage:\n\
     \x20 kubectl imageless run <dir> --repo HOST/REPO [flags] -- COMMAND [ARG...]\n\
     \x20 kubectl imageless version\n\
     \n\
     The directory must contain a flake.nix whose output builds the container\n\
     rootfs. It is packed into a seed OCI image under the same bounds the node\n\
     stages it with, so a refusal happens here, with a path, not on the node.\n\
     \n\
     Flags:\n\
     \x20 --repo HOST/REPO      repository the seed image belongs to (required)\n\
     \x20 --name NAME           pod name (default: derived from the directory)\n\
     \x20 --namespace NS        pod namespace\n\
     \x20 --runtime-class NAME  RuntimeClass of imageless nodes (default: imageless)\n\
     \x20 --output NAME         flake output to materialize (default: rootfs)\n\
     \x20 --arch ARCH           architecture in the image config — the seed itself is\n\
     \x20                       portable, but the node's runtime rejects a mismatch\n\
     \x20                       (default: amd64)\n\
     \x20 --include-vcs         pack .git/.hg/.jj/.svn instead of skipping them\n\
     \x20 --dry-run             print digests and the pod manifest; no network";

/// Requested help goes to stdout and succeeds; usage shown on a parse error
/// goes to stderr with the conventional usage-error exit code.
fn help() -> ExitCode {
    println!("{USAGE}");
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

#[cfg_attr(test, derive(Debug))]
enum ParsedRun {
    Help,
    Run(RunOptions),
}

#[cfg_attr(test, derive(Debug))]
struct RunOptions {
    source: PathBuf,
    repo: String,
    name: Option<String>,
    namespace: Option<String>,
    runtime_class: String,
    output: Option<String>,
    arch: String,
    include_vcs: bool,
    dry_run: bool,
    command: Vec<String>,
}

fn parse_run(arguments: &[String]) -> Result<ParsedRun, String> {
    let mut source = None;
    let mut repo = None;
    let mut name = None;
    let mut namespace = None;
    let mut runtime_class = "imageless".to_string();
    let mut output = None;
    let mut arch = "amd64".to_string();
    let mut include_vcs = false;
    let mut dry_run = false;
    let mut command = Vec::new();

    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let mut value = |flag: &str| {
            // A following flag is a missing value, not a value — accepting it
            // would validate a repo literally named `--dry-run`.
            match iterator.next() {
                Some(next) if !next.starts_with('-') => Ok(next.clone()),
                _ => Err(format!("{flag} requires a value")),
            }
        };
        match argument.as_str() {
            "--" => {
                command = iterator.cloned().collect();
                break;
            }
            "--help" | "-h" => return Ok(ParsedRun::Help),
            "--repo" => repo = Some(value("--repo")?),
            "--name" => name = Some(value("--name")?),
            "--namespace" => namespace = Some(value("--namespace")?),
            "--runtime-class" => runtime_class = value("--runtime-class")?,
            "--output" => output = Some(value("--output")?),
            "--arch" => arch = value("--arch")?,
            "--include-vcs" => include_vcs = true,
            "--dry-run" => dry_run = true,
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            positional if source.is_none() => source = Some(PathBuf::from(positional)),
            extra => return Err(format!("unexpected argument `{extra}`")),
        }
    }

    let source = source.ok_or("a source directory is required")?;
    let repo = repo.ok_or("--repo HOST/REPO is required")?;
    // Requiring a `/` enforces the promised HOST/REPO shape; a bare name
    // would silently resolve against docker.io/library on the node.
    if repo.is_empty()
        || !repo.contains('/')
        || repo.contains(char::is_whitespace)
        || repo.contains('@')
        || repo.contains("://")
    {
        return Err(format!(
            "--repo `{repo}` must be a bare repository like registry.example/team/app \
             (no scheme, tag, or digest)"
        ));
    }
    if command.is_empty() {
        return Err(
            "a workload command is required (the materialized rootfs chooses its own layout, \
             so there is no entrypoint to fall back on); pass it after `--`"
                .to_string(),
        );
    }
    Ok(ParsedRun::Run(RunOptions {
        source,
        repo,
        name,
        namespace,
        runtime_class,
        output,
        arch,
        include_vcs,
        dry_run,
        command,
    }))
}

fn run(options: &RunOptions) -> ExitCode {
    if !options.dry_run {
        return fail(
            "pushing to a registry is not implemented yet; pass --dry-run to print the \
             digests and pod manifest"
                .to_string(),
        );
    }
    let packed = match pack::pack_source(
        &options.source,
        &pack::PackOptions {
            include_vcs: options.include_vcs,
        },
    ) {
        Ok(packed) => packed,
        Err(error) => return fail(error),
    };
    for path in &packed.skipped_vcs {
        eprintln!(
            "notice: skipped version-control directory {} (pass --include-vcs to pack it)",
            path.display()
        );
    }
    if !packed.has_lock {
        eprintln!(
            "warning: {} has no flake.lock — the node will lock inputs at run time, so two\n\
             runs of the same seed can materialize different closures; commit a flake.lock",
            options.source.display()
        );
    }
    eprintln!(
        "packed {} entries, {} bytes (node bounds: {} entries, {} bytes)",
        packed.entries,
        packed.bytes,
        imageless::MAX_STAGED_SOURCE_ENTRIES,
        imageless::MAX_STAGED_SOURCE_BYTES
    );
    let image = oci::assemble(packed.tar, &options.arch);
    eprintln!(
        "layer    {} ({} bytes)",
        image.layer_digest,
        image.layer.len()
    );
    eprintln!(
        "config   {} ({} bytes)",
        image.config_digest,
        image.config.len()
    );
    eprintln!(
        "manifest {} ({} bytes)",
        image.manifest_digest,
        image.manifest.len()
    );

    let name = options
        .name
        .clone()
        .unwrap_or_else(|| podspec::derive_name(&options.source));
    let reference = format!("{}@{}", options.repo, image.manifest_digest);
    let pod = podspec::seed_pod(&podspec::PodSpec {
        name: &name,
        namespace: options.namespace.as_deref(),
        image: &reference,
        runtime_class: &options.runtime_class,
        output: options.output.as_deref(),
        command: &options.command,
    });
    match pod {
        Ok(pod) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&pod).expect("pod manifest serializes")
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(error),
    }
}

fn fail(message: String) -> ExitCode {
    eprintln!("kubectl-imageless: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn parse(words: &[&str]) -> Result<RunOptions, String> {
        match parse_run(&arguments(words))? {
            ParsedRun::Run(options) => Ok(options),
            ParsedRun::Help => panic!("unexpected help request"),
        }
    }

    #[test]
    fn run_parses_the_full_flag_set() {
        let options = parse(&[
            "./app",
            "--repo",
            "registry.example/team/app",
            "--name",
            "demo",
            "--namespace",
            "staging",
            "--runtime-class",
            "imageless-arm",
            "--output",
            "server",
            "--arch",
            "arm64",
            "--include-vcs",
            "--dry-run",
            "--",
            "/bin/server",
            "--port=8080",
        ])
        .unwrap();
        assert_eq!(options.source, PathBuf::from("./app"));
        assert_eq!(options.repo, "registry.example/team/app");
        assert_eq!(options.name.as_deref(), Some("demo"));
        assert_eq!(options.namespace.as_deref(), Some("staging"));
        assert_eq!(options.runtime_class, "imageless-arm");
        assert_eq!(options.output.as_deref(), Some("server"));
        assert_eq!(options.arch, "arm64");
        assert!(options.include_vcs);
        assert!(options.dry_run);
        assert_eq!(options.command, arguments(&["/bin/server", "--port=8080"]));
    }

    #[test]
    fn runtime_class_and_arch_default_when_not_given() {
        let options = parse(&["./app", "--repo", "r.example/app", "--", "/bin/true"]).unwrap();
        assert_eq!(options.runtime_class, "imageless");
        assert_eq!(options.arch, "amd64");
    }

    #[test]
    fn run_requires_repo_command_and_source() {
        let missing_repo = parse(&["./app", "--", "/bin/true"]).unwrap_err();
        assert!(missing_repo.contains("--repo"), "{missing_repo}");
        let missing_command = parse(&["./app", "--repo", "r.example/app"]).unwrap_err();
        assert!(missing_command.contains("command"), "{missing_command}");
        let missing_source = parse(&["--repo", "r.example/app", "--", "/bin/true"]).unwrap_err();
        assert!(
            missing_source.contains("source directory"),
            "{missing_source}"
        );
    }

    #[test]
    fn repo_must_be_a_bare_host_repo_reference() {
        for repo in [
            "https://r.example/app",
            "r.example/app@sha256:00",
            "bare-name-without-a-host",
            "",
        ] {
            let error = parse(&["./app", "--repo", repo, "--", "/bin/true"]).unwrap_err();
            assert!(error.contains("--repo"), "{error}");
        }
    }

    #[test]
    fn flags_reject_missing_values_and_unknown_names() {
        let dangling = parse(&["./app", "--repo"]).unwrap_err();
        assert!(dangling.contains("requires a value"), "{dangling}");
        let unknown = parse(&["./app", "--frobnicate"]).unwrap_err();
        assert!(unknown.contains("--frobnicate"), "{unknown}");
    }

    #[test]
    fn a_flag_is_never_swallowed_as_a_value() {
        let error = parse(&["./app", "--repo", "--dry-run", "--", "/bin/true"]).unwrap_err();
        assert!(error.contains("--repo requires a value"), "{error}");
    }

    #[test]
    fn run_help_is_help_not_a_usage_error() {
        assert!(matches!(
            parse_run(&arguments(&["--help"])).unwrap(),
            ParsedRun::Help
        ));
        assert!(matches!(
            parse_run(&arguments(&["./app", "-h"])).unwrap(),
            ParsedRun::Help
        ));
    }
}
