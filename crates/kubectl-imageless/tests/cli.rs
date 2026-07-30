//! The binary's stream contract: stdout carries exactly the pod manifest so
//! it pipes into `kubectl apply -f -`; digests and notices go to stderr.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kubectl-imageless"))
}

fn seed(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-cli-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
    root
}

#[test]
fn stdout_is_exactly_the_pod_manifest() {
    let root = seed("manifest");
    let output = binary()
        .arg("run")
        .arg(&root)
        .args([
            "--repo",
            "registry.example/team/app",
            "--dry-run",
            "--",
            "/bin/server",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // One JSON document and nothing else: from_str fails on trailing content.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let pod: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(pod["kind"], "Pod");
    let image = pod["spec"]["containers"][0]["image"].as_str().unwrap();
    assert!(
        image.starts_with("registry.example/team/app@sha256:"),
        "{image}"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("manifest sha256:"), "{stderr}");
    assert!(stderr.contains("flake.lock"), "{stderr}");
    std::fs::remove_dir_all(root).unwrap();
}

/// A pinned reference, so the tests exercise the mode rather than the refusal.
const PINNED: &str = "github:acme/agent/0123456789abcdef0123456789abcdef01234567";

#[test]
fn an_external_pod_names_the_reference_and_a_placeholder_image() {
    let output = binary()
        .args(["run", "--external", PINNED])
        .args([
            "--repo",
            "registry.example/team/agent",
            "--dry-run",
            "--",
            "/bin/agent",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pod: serde_json::Value = serde_json::from_str(&String::from_utf8(output.stdout).unwrap())
        .expect("stdout is exactly one JSON document");
    assert_eq!(
        pod["metadata"]["annotations"]["run.imageless.source"], PINNED,
        "the pod must name the reference, not the packed-seed source"
    );
    // The name comes from the repository; a revision would make two deploys of
    // the same flake differ only in 40 hex digits.
    assert_eq!(pod["metadata"]["name"], "agent");
    let image = pod["spec"]["containers"][0]["image"].as_str().unwrap();
    assert!(
        image.starts_with("registry.example/team/agent@sha256:"),
        "{image}"
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    for expected in [
        "moves trust from the image to the node",
        // The narrowest boundary-terminated prefix: any revision of that
        // repository, and nothing else.
        "`github:acme/agent/`",
        "run.imageless.* through",
        "the image is a placeholder",
    ] {
        assert!(stderr.contains(expected), "missing {expected:?}: {stderr}");
    }
}

#[test]
fn an_unpinned_reference_is_refused_before_anything_is_pushed() {
    let output = binary()
        .args(["run", "--external", "github:acme/agent"])
        .args(["--repo", "registry.example/team/agent", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "a refused run must print no pod manifest"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("is not pinned"), "{stderr}");
    assert!(stderr.contains("--unpinned"), "{stderr}");
}

#[test]
fn image_mode_uses_the_reference_verbatim_and_stays_offline() {
    let output = binary()
        .args(["run", "--external", PINNED])
        // No --dry-run and no --repo: with --image there is nothing to push,
        // so this run must complete without touching the network at all.
        .args(["--image", "registry.k8s.io/pause:3.10", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pod: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(
        pod["spec"]["containers"][0]["image"],
        "registry.k8s.io/pause:3.10"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("used verbatim"), "{stderr}");
    assert!(stderr.contains("not digest-pinned"), "{stderr}");
    assert!(
        !stderr.contains("manifest sha256:"),
        "nothing is assembled in --image mode: {stderr}"
    );
}

#[test]
fn a_reference_typed_without_the_flag_is_a_missing_directory_with_a_hint() {
    let output = binary()
        .args(["run", "github:acme/agent"])
        .args(["--repo", "registry.example/team/agent", "--", "/bin/agent"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    // The mode never switches on the argument's shape — it fails as a path,
    // and only then suggests the flag.
    assert!(stderr.contains("No such file or directory"), "{stderr}");
    assert!(stderr.contains("pass --external"), "{stderr}");
}

#[test]
fn requested_help_lands_on_stdout_and_succeeds() {
    for arguments in [&["help"][..], &["--help"], &["run", "--help"]] {
        let output = binary().args(arguments).output().unwrap();
        assert!(output.status.success(), "{arguments:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage:"), "{arguments:?}: {stdout}");
    }
}
