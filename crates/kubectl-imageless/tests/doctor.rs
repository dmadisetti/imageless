//! `doctor`'s binary contract, against a stub `kubectl`.
//!
//! The stub is a shell script selected with `IMAGELESS_KUBECTL`, so these
//! tests exercise the real subprocess seam — argument forwarding, exit codes,
//! the stdout/stderr split — without a cluster. Each test gets its own stub
//! directory, and `KUBECONFIG` is cleared so a developer's real config can
//! never change an outcome.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PREPARED_CLASS: &str = r#"{"handler":"imageless","metadata":{"name":"imageless"},
  "scheduling":{"nodeSelector":{"imageless.run/runtime":"v2"}}}"#;

const READY_NODE: &str = r#"{"items":[{"metadata":{"name":"node-a",
  "labels":{"imageless.run/runtime":"v2"}},
  "status":{"conditions":[{"type":"Ready","status":"True"}],
  "nodeInfo":{"architecture":"amd64","containerRuntimeVersion":"containerd://2.1.4"}}}]}"#;

const CONTEXT: &str = r#"{"current-context":"kind-imageless",
  "clusters":[{"name":"kind-imageless","cluster":{"server":"https://127.0.0.1:6443"}}],
  "contexts":[{"context":{"namespace":"default"}}]}"#;

struct Workspace {
    root: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Workspace {
    /// `class` and `nodes` are the JSON the stub answers with; an empty string
    /// means "absent", which `--ignore-not-found` reports as empty stdout.
    fn new(label: &str, class: &str, nodes: &str) -> Workspace {
        let root = std::env::temp_dir().join(format!(
            "kubectl-imageless-doctor-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> "{recorded}"
case "$*" in
  "config view"*)   printf '%s' '{context}' ;;
  "version --client"*) printf '%s' '{{"clientVersion":{{"gitVersion":"v1.33.1"}}}}' ;;
  "get --raw /version"*) printf '%s' '{{"gitVersion":"v1.33.1"}}' ;;
  "get runtimeclass"*) printf '%s' '{class}' ;;
  "get nodes"*)     printf '%s' '{nodes}' ;;
  *) echo "stub: unexpected: $*" >&2; exit 1 ;;
esac
"#,
            recorded = root.join("calls").display(),
            context = CONTEXT,
        );
        let program = root.join("kubectl-stub");
        let mut file = std::fs::File::create(&program).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        // Close before exec: a descriptor still open for writing makes execve
        // fail with ETXTBSY.
        drop(file);
        std::fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        Workspace { root }
    }

    fn stub(&self) -> PathBuf {
        self.root.join("kubectl-stub")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.root.join("calls")).unwrap_or_default()
    }

    fn doctor(&self, arguments: &[&str]) -> Output {
        run(&self.stub(), arguments)
    }
}

fn run(stub: &Path, arguments: &[&str]) -> Output {
    // A sibling test thread that forks while this file's write descriptor is
    // still open leaves the child holding it, and execve then refuses the
    // freshly written script. Retrying the spawn is the only fix available to
    // a test that writes its own executable.
    for _ in 0..8 {
        let output = Command::new(env!("CARGO_BIN_EXE_kubectl-imageless"))
            .arg("doctor")
            .args(arguments)
            .env("IMAGELESS_KUBECTL", stub)
            .env_remove("KUBECONFIG")
            .env_remove("KUBERNETES_SERVICE_HOST")
            .output()
            .unwrap();
        if !String::from_utf8_lossy(&output.stderr).contains("Text file busy") {
            return output;
        }
    }
    panic!("stub kubectl stayed busy across retries");
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn a_prepared_cluster_passes_and_says_it_is_not_proof() {
    let workspace = Workspace::new("prepared", PREPARED_CLASS, READY_NODE);
    let output = workspace.doctor(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let report = stdout(&output);
    assert!(report.contains("pass  runtime-class"), "{report}");
    assert!(report.contains("pass  scheduling"), "{report}");
    assert!(report.contains("1 of 1 nodes"), "{report}");
    // node-config can never pass: it has no API representation.
    assert!(report.contains("skip  node-config"), "{report}");
    assert!(report.contains("not proof the seam"), "{report}");
}

#[test]
fn an_absent_runtime_class_fails_and_points_at_the_example() {
    let workspace = Workspace::new("no-class", "", READY_NODE);
    let output = workspace.doctor(&[]);
    assert_eq!(output.status.code(), Some(1));
    let report = stdout(&output);
    assert!(report.contains("fail  runtime-class"), "{report}");
    assert!(report.contains("runtimeclass.yaml"), "{report}");
}

#[test]
fn a_cluster_with_no_labelled_node_is_a_failure_not_a_warning() {
    let unlabelled = r#"{"items":[{"metadata":{"name":"plain","labels":{}},
      "status":{"conditions":[{"type":"Ready","status":"True"}],"nodeInfo":{}}}]}"#;
    let workspace = Workspace::new("no-node", PREPARED_CLASS, unlabelled);
    let output = workspace.doctor(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout(&output).contains("stay Pending"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn json_output_is_exactly_one_document() {
    let workspace = Workspace::new("json", PREPARED_CLASS, READY_NODE);
    let output = workspace.doctor(&["--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["schema"], "imageless.doctor.v1");
    assert_eq!(report["probe_failed"], false);
    let ids: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"runtime-class"), "{ids:?}");
    assert!(ids.contains(&"node-config"), "{ids:?}");
}

#[test]
fn connection_flags_reach_kubectl_verbatim() {
    let workspace = Workspace::new("flags", PREPARED_CLASS, READY_NODE);
    let output = workspace.doctor(&["--context", "kind-imageless", "--namespace=apps"]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let calls = workspace.calls();
    assert!(calls.contains("--context kind-imageless"), "{calls}");
    assert!(calls.contains("--namespace=apps"), "{calls}");
    // A diagnostic must not hang on a black-holed API server.
    assert!(calls.contains("--request-timeout=10s"), "{calls}");
}

#[test]
fn a_missing_kubectl_is_could_not_look_not_broken() {
    let workspace = Workspace::new("missing", PREPARED_CLASS, READY_NODE);
    let output = run(Path::new("kubectl-does-not-exist"), &[]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a cluster we could not probe is a different answer from a broken one"
    );
    let report = stdout(&output);
    assert!(report.contains("fail  kubectl"), "{report}");
    assert!(report.contains("IMAGELESS_KUBECTL"), "{report}");
    // The cluster checks must not claim anything either way.
    assert!(report.contains("skip  runtime-class"), "{report}");
    drop(workspace);
}

#[test]
fn strict_promotes_a_warning_to_a_failure() {
    // A RuntimeClass with no nodeSelector: pods can land on unprepared nodes.
    let no_selector = r#"{"handler":"imageless","metadata":{"name":"imageless"}}"#;
    let workspace = Workspace::new("strict", no_selector, READY_NODE);
    assert_eq!(workspace.doctor(&[]).status.code(), Some(0));
    assert_eq!(workspace.doctor(&["--strict"]).status.code(), Some(1));
}

#[test]
fn offline_checks_still_run_when_the_cluster_cannot_be_probed() {
    let workspace = Workspace::new("offline", PREPARED_CLASS, READY_NODE);
    let policy = workspace.root.join("policy.json");
    std::fs::write(
        &policy,
        br#"{"system":"x86_64-linux","cache_only":false,
             "eval_allowed_uri_prefixes":["github:acme/"]}"#,
    )
    .unwrap();
    let output = run(
        Path::new("kubectl-does-not-exist"),
        &[
            "--policy",
            policy.to_str().unwrap(),
            "--source",
            "github:acme/agent/0123456789abcdef0123456789abcdef01234567",
        ],
    );
    // Debugging a policy file should not require a cluster.
    let report = stdout(&output);
    assert!(report.contains("pass  policy"), "{report}");
    assert!(report.contains("pass  source"), "{report}");
    // The report is still incomplete, so the verdict is still "could not look".
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn a_source_denied_by_the_policy_names_the_prefix_to_ask_for() {
    let workspace = Workspace::new("denied", PREPARED_CLASS, READY_NODE);
    let policy = workspace.root.join("policy.json");
    std::fs::write(
        &policy,
        br#"{"system":"x86_64-linux","cache_only":false,
             "eval_allowed_uri_prefixes":["github:acme/"]}"#,
    )
    .unwrap();
    let output = workspace.doctor(&[
        "--policy",
        policy.to_str().unwrap(),
        "--source",
        "github:other/agent/0123456789abcdef0123456789abcdef01234567",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let report = stdout(&output);
    assert!(report.contains("fail  source"), "{report}");
    assert!(report.contains("github:other/agent/"), "{report}");
}
