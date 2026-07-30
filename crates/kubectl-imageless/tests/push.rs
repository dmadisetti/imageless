//! Push tests against a hand-rolled stub registry: a TcpListener speaking
//! just enough HTTP/1.1 for the distribution-spec push flow. One request per
//! connection, `Connection: close`, Content-Length bodies only — the stub
//! records every request so tests can assert auth and ordering, and it
//! recomputes digests so byte-identity is proven, not assumed.
//!
//! Hermeticity: every invocation gets its own DOCKER_CONFIG (the developer's
//! real credentials are never read) and has the proxy variables removed —
//! ureq takes its proxy from the environment, so a machine with `http_proxy`
//! set would route these loopback requests off-box.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "stub-token";

#[derive(Default)]
struct Behavior {
    require_token: bool,
    require_basic: Option<String>,
    reject_blob_put: bool,
    fail_token_endpoint: bool,
    /// `scheme://host:port` of a second listener the upload Location points
    /// at, standing in for a storage backend's presigned URL.
    upload_elsewhere: Option<String>,
}

#[derive(Default)]
struct State {
    blobs: HashMap<String, Vec<u8>>,
    manifests: HashMap<String, (String, Vec<u8>)>,
    requests: Vec<Request>,
}

struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
}

struct Stub {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl Stub {
    fn start(behavior: Behavior) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        // The acceptor blocks forever; the test process's exit reaps it.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                handle(stream, address, &behavior, &shared);
            }
        });
        Stub { address, state }
    }

    fn repo(&self) -> String {
        format!("{}/team/app", self.address)
    }
}

fn handle(stream: TcpStream, address: SocketAddr, behavior: &Behavior, state: &Arc<Mutex<State>>) {
    let mut stream = stream;
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return;
    };
    let (method, target) = (method.to_string(), target.to_string());
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    if headers.contains_key("transfer-encoding") {
        respond(&mut stream, "501 Not Implemented", &[], b"", &method);
        return;
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    if length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let authorization = headers.get("authorization").cloned();
    state.lock().unwrap().requests.push(Request {
        method: method.clone(),
        target: target.clone(),
        authorization: authorization.clone(),
    });

    if target.starts_with("/token") {
        if behavior.fail_token_endpoint {
            respond(&mut stream, "500 Internal Server Error", &[], b"", &method);
        } else {
            respond(
                &mut stream,
                "200 OK",
                &[("Content-Type", "application/json")],
                format!("{{\"token\":\"{TOKEN}\"}}").as_bytes(),
                &method,
            );
        }
        return;
    }
    let unauthorized = b"{\"errors\":[{\"code\":\"UNAUTHORIZED\",\
                          \"message\":\"authentication required\"}]}";
    if behavior.require_token && authorization.as_deref() != Some(&format!("Bearer {TOKEN}")) {
        let challenge = format!("Bearer realm=\"http://{address}/token\",service=\"stub-service\"");
        respond(
            &mut stream,
            "401 Unauthorized",
            &[("Www-Authenticate", &challenge)],
            unauthorized,
            &method,
        );
        return;
    }
    if let Some(expected) = &behavior.require_basic {
        if authorization.as_deref() != Some(expected.as_str()) {
            respond(
                &mut stream,
                "401 Unauthorized",
                &[("Www-Authenticate", "Basic realm=\"stub\"")],
                unauthorized,
                &method,
            );
            return;
        }
    }

    if method == "GET" && target == "/v2/" {
        respond(&mut stream, "200 OK", &[], b"{}", &method);
    } else if method == "HEAD" && target.contains("/blobs/sha256:") {
        let digest = target.rsplit('/').next().unwrap().to_string();
        let blob = state.lock().unwrap().blobs.get(&digest).cloned();
        match blob {
            // The body is passed for its Content-Length; HEAD suppresses it.
            Some(bytes) => respond(&mut stream, "200 OK", &[], &bytes, &method),
            None => respond(&mut stream, "404 Not Found", &[], b"", &method),
        }
    } else if method == "POST" && target == "/v2/team/app/blobs/uploads/" {
        let session = state.lock().unwrap().requests.len();
        // The query parameter forces the client onto its `&digest=` path.
        let location = match &behavior.upload_elsewhere {
            Some(elsewhere) => {
                format!("{elsewhere}/v2/team/app/blobs/uploads/session-{session}?state=opaque")
            }
            None => format!("/v2/team/app/blobs/uploads/session-{session}?state=opaque"),
        };
        respond(
            &mut stream,
            "202 Accepted",
            &[("Location", &location)],
            b"",
            &method,
        );
    } else if method == "PUT" && target.starts_with("/v2/team/app/blobs/uploads/") {
        let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
        let digest = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("digest="))
            .map(percent_decode)
            .unwrap_or_default();
        let computed = format!("sha256:{}", hex::encode(Sha256::digest(&body)));
        if behavior.reject_blob_put || computed != digest {
            respond(
                &mut stream,
                "400 Bad Request",
                &[("Content-Type", "application/json")],
                b"{\"errors\":[{\"code\":\"DIGEST_INVALID\",\
                   \"message\":\"provided digest did not match uploaded content\"}]}",
                &method,
            );
        } else {
            state.lock().unwrap().blobs.insert(digest.clone(), body);
            respond(
                &mut stream,
                "201 Created",
                &[("Docker-Content-Digest", &digest)],
                b"",
                &method,
            );
        }
    } else if method == "PUT" && target.starts_with("/v2/team/app/manifests/") {
        let reference = target.rsplit('/').next().unwrap().to_string();
        let content_type = headers.get("content-type").cloned().unwrap_or_default();
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&body)));
        state
            .lock()
            .unwrap()
            .manifests
            .insert(reference, (content_type, body));
        respond(
            &mut stream,
            "201 Created",
            &[("Docker-Content-Digest", &digest)],
            b"",
            &method,
        );
    } else {
        respond(&mut stream, "404 Not Found", &[], b"", &method);
    }
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    method: &str,
) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    if method != "HEAD" {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap(),
                16,
            ) {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).unwrap()
}

/// The source directory and its own DOCKER_CONFIG, side by side so the config
/// is never packed into the image. Cleanup runs from Drop: an assertion that
/// fails must not leave the tree behind for a later run to trip over.
struct Workspace {
    root: PathBuf,
    source: PathBuf,
    docker: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn workspace(label: &str) -> Workspace {
    let root = std::env::temp_dir().join(format!(
        "kubectl-imageless-push-{label}-{}",
        std::process::id()
    ));
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
    let docker = root.join("docker");
    std::fs::create_dir_all(&docker).unwrap();
    Workspace {
        root,
        source,
        docker,
    }
}

impl Workspace {
    fn with_credentials(self, host: &str, auth: &str) -> Workspace {
        std::fs::write(
            self.docker.join("config.json"),
            format!("{{\"auths\":{{\"{host}\":{{\"auth\":\"{auth}\"}}}}}}"),
        )
        .unwrap();
        self
    }
}

fn run_push(stub: &Stub, workspace: &Workspace, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kubectl-imageless"));
    command
        .arg("run")
        .arg(&workspace.source)
        .args(["--repo", &stub.repo()])
        .args(extra)
        .args(["--", "/bin/true"])
        .env("DOCKER_CONFIG", &workspace.docker);
    // ureq reads its proxy from the environment; a developer's proxy would
    // send these loopback requests off-box and fail every test here.
    for variable in [
        "ALL_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "all_proxy",
        "https_proxy",
        "http_proxy",
    ] {
        command.env_remove(variable);
    }
    command.output().unwrap()
}

fn manifest_digest_from(pod_stdout: &str, stub: &Stub) -> String {
    let pod: serde_json::Value = serde_json::from_str(pod_stdout).unwrap();
    let image = pod["spec"]["containers"][0]["image"].as_str().unwrap();
    let prefix = format!("{}@", stub.repo());
    assert!(image.starts_with(&prefix), "{image}");
    image[prefix.len()..].to_string()
}

#[test]
fn anonymous_push_uploads_byte_identical_blobs_and_manifest() {
    let stub = Stub::start(Behavior::default());
    let space = workspace("anonymous");
    let output = run_push(&stub, &space, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The stream contract holds in push mode: stdout is one JSON document.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let digest = manifest_digest_from(&stdout, &stub);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!("pushed {}@{digest}", stub.repo())),
        "{stderr}"
    );

    let state = stub.state.lock().unwrap();
    assert_eq!(state.blobs.len(), 2, "layer and config blobs");
    for (stored_digest, bytes) in &state.blobs {
        assert_eq!(
            format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
            *stored_digest
        );
    }
    let (content_type, manifest) = state.manifests.get(&digest).expect("manifest by digest");
    assert_eq!(content_type, "application/vnd.oci.image.manifest.v1+json");
    assert_eq!(
        format!("sha256:{}", hex::encode(Sha256::digest(manifest))),
        digest
    );
    // Blobs land before the manifest that references them.
    let last_blob_put = state
        .requests
        .iter()
        .rposition(|request| request.method == "PUT" && request.target.contains("/blobs/uploads/"))
        .unwrap();
    let manifest_put = state
        .requests
        .iter()
        .position(|request| request.method == "PUT" && request.target.contains("/manifests/"))
        .unwrap();
    assert!(last_blob_put < manifest_put);
}

#[test]
fn existing_blobs_are_skipped_after_a_head_hit() {
    let stub = Stub::start(Behavior::default());
    let space = workspace("repush");
    for _ in 0..2 {
        let output = run_push(&stub, &space, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let state = stub.state.lock().unwrap();
    let uploads_started = state
        .requests
        .iter()
        .filter(|request| request.method == "POST")
        .count();
    assert_eq!(uploads_started, 2, "second push must upload nothing");
    let head_hits = state
        .requests
        .iter()
        .filter(|request| request.method == "HEAD")
        .count();
    assert_eq!(head_hits, 4, "both blobs are checked on both pushes");
    let manifest_puts = state
        .requests
        .iter()
        .filter(|request| request.method == "PUT" && request.target.contains("/manifests/"))
        .count();
    assert_eq!(manifest_puts, 2, "the manifest PUT itself is idempotent");
}

#[test]
fn bearer_dance_attaches_the_issued_token_to_every_subsequent_request() {
    let stub = Stub::start(Behavior {
        require_token: true,
        ..Behavior::default()
    });
    let space = workspace("bearer");
    let output = run_push(&stub, &space, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = stub.state.lock().unwrap();
    let token_fetches = state
        .requests
        .iter()
        .filter(|request| request.target.starts_with("/token"))
        .count();
    assert_eq!(token_fetches, 1, "the token is cached for the process");
    // Only the very first /v2/ probe goes out anonymously.
    for (index, request) in state
        .requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.target.starts_with("/v2/"))
    {
        if index == 0 {
            assert_eq!(request.authorization, None, "{}", request.target);
        } else {
            assert_eq!(
                request.authorization.as_deref(),
                Some(format!("Bearer {TOKEN}").as_str()),
                "{} {}",
                request.method,
                request.target
            );
        }
    }
}

#[test]
fn digest_mismatch_400_names_the_blob_and_calls_it_a_bug() {
    let stub = Stub::start(Behavior {
        reject_blob_put: true,
        ..Behavior::default()
    });
    let space = workspace("mismatch");
    let output = run_push(&stub, &space, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("digest mismatch"), "{stderr}");
    assert!(stderr.contains("imageless bug"), "{stderr}");
}

#[test]
fn token_endpoint_failure_is_fail_closed() {
    let stub = Stub::start(Behavior {
        require_token: true,
        fail_token_endpoint: true,
        ..Behavior::default()
    });
    let space = workspace("token-down");
    let output = run_push(&stub, &space, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("token endpoint"), "{stderr}");
    let state = stub.state.lock().unwrap();
    assert!(state.blobs.is_empty(), "nothing may upload without auth");
    assert!(state.manifests.is_empty());
}

#[test]
fn tag_flag_issues_a_second_manifest_put_with_identical_bytes() {
    let stub = Stub::start(Behavior::default());
    let space = workspace("tagged");
    let output = run_push(&stub, &space, &["--tag", "v1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let digest = manifest_digest_from(&String::from_utf8(output.stdout).unwrap(), &stub);
    let state = stub.state.lock().unwrap();
    let (_, by_digest) = state.manifests.get(&digest).expect("manifest by digest");
    let (_, by_tag) = state.manifests.get("v1").expect("manifest by tag");
    assert_eq!(by_digest, by_tag);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("also tagged"), "{stderr}");
}

#[test]
fn a_basic_challenge_is_answered_from_config_json() {
    // base64("stub:s3cret") — the credential the stub demands.
    let expected = "Basic c3R1YjpzM2NyZXQ=";
    let stub = Stub::start(Behavior {
        require_basic: Some(expected.to_string()),
        ..Behavior::default()
    });
    let space = workspace("basic").with_credentials(&stub.address.to_string(), "c3R1YjpzM2NyZXQ=");
    let output = run_push(&stub, &space, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = stub.state.lock().unwrap();
    assert_eq!(state.blobs.len(), 2);
    // Every request after the challenged probe carries the header.
    let authorized = state
        .requests
        .iter()
        .filter(|request| request.authorization.as_deref() == Some(expected))
        .count();
    assert!(authorized >= 4, "{authorized} authorized requests");
}

#[test]
fn a_missing_basic_credential_fails_closed_naming_the_config() {
    let stub = Stub::start(Behavior {
        require_basic: Some("Basic never-supplied".to_string()),
        ..Behavior::default()
    });
    let space = workspace("basic-missing");
    let output = run_push(&stub, &space, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("refused authentication"), "{stderr}");
    assert!(stderr.contains("docker login"), "{stderr}");
    assert!(stub.state.lock().unwrap().blobs.is_empty());
}

#[test]
fn an_upload_redirected_to_another_host_never_carries_the_token() {
    let storage = Stub::start(Behavior::default());
    let registry = Stub::start(Behavior {
        require_token: true,
        upload_elsewhere: Some(format!("http://{}", storage.address)),
        ..Behavior::default()
    });
    let space = workspace("elsewhere");
    let output = run_push(&registry, &space, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let storage_state = storage.state.lock().unwrap();
    let uploads: Vec<_> = storage_state
        .requests
        .iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(uploads.len(), 2, "both blobs went to the storage host");
    for upload in uploads {
        assert_eq!(
            upload.authorization, None,
            "the registry's token must not reach {}",
            upload.target
        );
    }
    // The manifest still went to the registry, authenticated.
    let registry_state = registry.state.lock().unwrap();
    let manifest = registry_state
        .requests
        .iter()
        .find(|request| request.method == "PUT" && request.target.contains("/manifests/"))
        .expect("manifest PUT");
    assert_eq!(
        manifest.authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}
