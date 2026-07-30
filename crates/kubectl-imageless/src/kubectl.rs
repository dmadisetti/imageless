//! Talking to the cluster by driving `kubectl`, not by being a client.
//!
//! This plugin has no Kubernetes client and no kubeconfig parser, and adding
//! them would mean reimplementing `KUBECONFIG` list merging, context and user
//! overrides, in-cluster service-account fallback, exec credential plugins,
//! client-certificate mTLS, and impersonation. Getting that 95% right produces
//! the worst failure a diagnostic can have: a confident report about the wrong
//! cluster. `kubectl` has already resolved all of it, is guaranteed present
//! (it is what invokes us), and `get --raw <path>` reaches any API path through
//! its transport — so the client library costs nothing by being absent.
//!
//! Connection flags are forwarded verbatim and never interpreted. This module
//! therefore never learns what namespace or context is in effect; whatever
//! kubectl resolves is the answer, which is the entire point.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Connection flags that take a separate value. `--flag=value` needs no entry:
/// it is one argument, and forwarding is positional either way.
const CONNECTION_VALUE_FLAGS: &[&str] = &[
    "--context",
    "--cluster",
    "--user",
    "--kubeconfig",
    "--namespace",
    "-n",
    "--as",
    "--as-group",
    "--as-uid",
    "--token",
    "--server",
    "-s",
    "--request-timeout",
    "--tls-server-name",
    "--client-certificate",
    "--client-key",
    "--certificate-authority",
];

/// Connection flags that take no value.
///
/// Every flag in both lists is one kubectl registers on its *root* command, so
/// it is accepted by each subcommand driven here. `--all-namespaces`/`-A` is
/// not: `get` defines it, `version` and `config view` do not, so forwarding it
/// would fail the first two checks with kubectl's "unknown flag" rather than
/// anything about the cluster. It is also not a connection flag — doctor lists
/// nothing namespaced — so it belongs in neither list, and reaches the user as
/// this command's own unknown-flag error instead.
const CONNECTION_SWITCHES: &[&str] = &["--insecure-skip-tls-verify"];

/// A diagnostic must not hang on a black-holed API server.
const DEFAULT_REQUEST_TIMEOUT: &str = "--request-timeout=10s";

#[cfg_attr(test, derive(Debug))]
pub(crate) struct Kubectl {
    program: String,
    connection: Vec<String>,
}

#[cfg_attr(test, derive(Debug))]
pub(crate) enum Failure {
    /// No `kubectl` to run at all.
    NotInstalled(String),
    /// It ran, but could not reach or authenticate to the API server. Its own
    /// diagnostic already reached the terminal.
    Probe(String),
    /// Authenticated, but not authorized for this resource.
    Forbidden(String),
    Other {
        code: Option<i32>,
        stderr: String,
    },
}

impl Kubectl {
    pub(crate) fn new(connection: Vec<String>) -> Result<Kubectl, String> {
        let program = match std::env::var("IMAGELESS_KUBECTL") {
            Ok(program) if !program.is_empty() => {
                // A bare name is resolved on PATH and an absolute path is
                // taken as given; a relative path with a separator would
                // execute whatever sits at that spot in the working
                // directory, which is the same rule credential-helper names
                // follow in `auth`.
                let path = Path::new(&program);
                if !path.is_absolute() && program.contains(std::path::MAIN_SEPARATOR) {
                    return Err(format!(
                        "IMAGELESS_KUBECTL `{program}` must be a bare command name or an \
                         absolute path"
                    ));
                }
                program
            }
            _ => "kubectl".to_string(),
        };
        Ok(Kubectl {
            program,
            connection,
        })
    }

    /// The one call that lets kubectl own the terminal.
    ///
    /// stderr is inherited rather than captured: an exec credential plugin
    /// with `interactiveMode: Always` writes its prompt there and then blocks
    /// on stdin, and capturing would turn that into a silent hang. Inheriting
    /// also primes the credential cache, so every later call can safely
    /// capture. The cost is that we cannot tell "unreachable" from
    /// "unauthenticated" in our own words — but kubectl's line is already on
    /// the user's terminal, and both are the same verdict.
    pub(crate) fn probe(&self, arguments: &[&str]) -> Result<String, Failure> {
        let output = self
            .command(arguments)
            .stdin(Stdio::inherit())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| self.spawn_failure(error))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        Err(Failure::Probe(
            "kubectl could not reach or authenticate to the API server; its own diagnostic \
             is printed above"
                .to_string(),
        ))
    }

    pub(crate) fn json(&self, arguments: &[&str]) -> Result<Value, Failure> {
        let text = self.capture(arguments)?;
        serde_json::from_str(&text).map_err(|error| Failure::Other {
            code: None,
            stderr: format!("kubectl printed output this plugin could not parse: {error}"),
        })
    }

    /// `None` when the object does not exist.
    ///
    /// Absence is answered by `--ignore-not-found`'s empty stdout and a zero
    /// exit, never by matching kubectl's NotFound prose — which is another
    /// tool's user-facing text and free to change.
    pub(crate) fn json_or_absent(&self, arguments: &[&str]) -> Result<Option<Value>, Failure> {
        let mut arguments = arguments.to_vec();
        arguments.push("--ignore-not-found");
        let text = self.capture(&arguments)?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| Failure::Other {
                code: None,
                stderr: format!("kubectl printed output this plugin could not parse: {error}"),
            })
    }

    fn capture(&self, arguments: &[&str]) -> Result<String, Failure> {
        let output = self
            .command(arguments)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| self.spawn_failure(error))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        Err(classify(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        ))
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.args(arguments);
        command.args(&self.connection);
        if !self.connection.iter().any(is_request_timeout) {
            command.arg(DEFAULT_REQUEST_TIMEOUT);
        }
        command.stdout(Stdio::piped());
        command
    }

    fn spawn_failure(&self, error: std::io::Error) -> Failure {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Failure::NotInstalled(format!(
                "`{}` is not on PATH — this plugin drives kubectl rather than speaking to the \
                 API server itself, so your context, credentials, and exec plugins are whatever \
                 kubectl already resolves; set IMAGELESS_KUBECTL to its path",
                self.program
            ));
        }
        Failure::Other {
            code: None,
            stderr: format!("could not run `{}`: {error}", self.program),
        }
    }
}

fn is_request_timeout(argument: &String) -> bool {
    argument == "--request-timeout" || argument.starts_with("--request-timeout=")
}

/// Two substrings of kubectl's own prose are matched, and only these two:
/// authorization is the one failure worth a distinct verdict, because a
/// developer who cannot read cluster-scoped resources is not a broken cluster.
/// Everything else is forwarded verbatim rather than re-worded.
pub(crate) fn classify(code: Option<i32>, stderr: &str) -> Failure {
    if stderr.contains("(Forbidden)") || stderr.contains("is forbidden") {
        return Failure::Forbidden(stderr.trim().to_string());
    }
    Failure::Other {
        code,
        stderr: stderr.trim().to_string(),
    }
}

/// Split argv into connection flags to forward and everything else.
///
/// kubectl stops collecting the plugin command path at the first `-`-prefixed
/// argument, so `kubectl --context x imageless doctor` never dispatches here at
/// all — the flags have to come after `imageless`. Modern kubectl `syscall.Exec`s
/// the plugin with the environment unchanged and sets no `KUBECTL_PLUGINS_*`
/// variables (that mechanism was removed in 1.12), so there is nothing else to
/// read: do not "helpfully" consult a stale exported one, which would silently
/// retarget every call below.
pub(crate) fn split_connection_flags(arguments: &[String]) -> (Vec<String>, Vec<String>) {
    let mut connection = Vec::new();
    let mut rest = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        let name = argument.split_once('=').map_or(argument, |(name, _)| name);
        if CONNECTION_VALUE_FLAGS.contains(&name) {
            connection.push(arguments[index].clone());
            // A `--flag=value` argument carries its own value.
            if !argument.contains('=') {
                if let Some(value) = arguments.get(index + 1) {
                    connection.push(value.clone());
                }
                index += 1;
            }
        } else if CONNECTION_SWITCHES.contains(&argument) {
            connection.push(arguments[index].clone());
        } else {
            rest.push(arguments[index].clone());
        }
        index += 1;
    }
    (connection, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn connection_flags_are_separated_in_both_spellings() {
        let (connection, rest) = split_connection_flags(&words(&[
            "--context",
            "kind-imageless",
            "--namespace=apps",
            "--json",
            "--runtime-class",
            "imageless",
        ]));
        assert_eq!(
            connection,
            words(&["--context", "kind-imageless", "--namespace=apps"])
        );
        assert_eq!(rest, words(&["--json", "--runtime-class", "imageless"]));
    }

    #[test]
    fn a_switch_takes_no_value_with_it() {
        let (connection, rest) =
            split_connection_flags(&words(&["--insecure-skip-tls-verify", "--json"]));
        assert_eq!(connection, words(&["--insecure-skip-tls-verify"]));
        assert_eq!(rest, words(&["--json"]));
    }

    #[test]
    fn all_namespaces_is_not_forwarded_and_so_becomes_an_unknown_flag() {
        // `get` defines -A; `version` and `config view` do not. Forwarding it
        // would fail doctor's first two checks with kubectl's "unknown flag"
        // instead of anything about the cluster, so it stays in `rest` and the
        // caller rejects it with its own message.
        let (connection, rest) = split_connection_flags(&words(&["-A", "--json"]));
        assert!(connection.is_empty(), "{connection:?}");
        assert_eq!(rest, words(&["-A", "--json"]));
        let (connection, _) = split_connection_flags(&words(&["--all-namespaces"]));
        assert!(connection.is_empty(), "{connection:?}");
    }

    #[test]
    fn forbidden_is_the_only_re_worded_failure() {
        assert!(matches!(
            classify(Some(1), "Error from server (Forbidden): nodes is forbidden"),
            Failure::Forbidden(_)
        ));
        match classify(Some(1), "some other failure") {
            Failure::Other { code, stderr } => {
                assert_eq!(code, Some(1));
                // Verbatim: re-wording another tool's diagnostic loses detail
                // and drifts as that tool changes.
                assert_eq!(stderr, "some other failure");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_relative_override_with_a_separator_is_refused() {
        // Same rule credential-helper names follow: this would execute
        // whatever happens to sit there in the working directory.
        temp_env("IMAGELESS_KUBECTL", Some("./kubectl"), || {
            let error = Kubectl::new(Vec::new()).unwrap_err();
            assert!(error.contains("absolute path"), "{error}");
        });
        temp_env("IMAGELESS_KUBECTL", Some("/usr/bin/kubectl"), || {
            assert!(Kubectl::new(Vec::new()).is_ok());
        });
        temp_env("IMAGELESS_KUBECTL", Some("kubectl-stub"), || {
            assert!(Kubectl::new(Vec::new()).is_ok());
        });
        // An emptied override is not an override.
        temp_env("IMAGELESS_KUBECTL", Some(""), || {
            assert_eq!(Kubectl::new(Vec::new()).unwrap().program, "kubectl");
        });
    }

    /// Cargo runs tests in threads of one process, so an environment variable
    /// set by one test is visible to every other. Every test that touches this
    /// variable goes through here, and none of them run concurrently with each
    /// other because they are the same test.
    fn temp_env(key: &str, value: Option<&str>, body: impl FnOnce()) {
        let previous = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        body();
        match previous {
            Some(previous) => std::env::set_var(key, previous),
            None => std::env::remove_var(key),
        }
    }
}
