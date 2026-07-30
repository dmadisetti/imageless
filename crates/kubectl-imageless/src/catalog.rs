//! Resolving a human-friendly release name to the digest a node will accept.
//!
//! SPEC §6 draws the line this module sits on: a catalog MAY publish a
//! `refs/<name>/<channel>` index, and **nodes MUST ignore it**. Node-side
//! resolution of a mutable pointer is non-conforming, because it would make the
//! thing a node runs depend on what a catalog said at start time rather than on
//! what the pod's author approved. So the index reader lives here, in authoring
//! tooling, and not in the `imageless` library the shim links: there is no code
//! path by which a node could accidentally grow this capability.
//!
//! The output is always `issuer/name@sha256:<64 hex>` — the only release form
//! §6 lets an annotation carry. Everything this module does happens before that
//! string is written down.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// A channel pointer is a digest and nothing else.
///
/// The spec names the index but not its bytes, so this is the definition:
/// the file at `refs/<name>/<channel>` contains the 64 lowercase hex digits of
/// the manifest digest, with optional surrounding whitespace and nothing else.
/// A publisher writes one with `printf '%s' "$digest" > refs/agent/stable`,
/// which is the same bar §6 sets for manifests themselves — any CI that can
/// copy a closure to a cache can also publish a channel. A richer format would
/// buy extensibility this file does not need: it points at a manifest, and the
/// manifest is where structured data belongs.
const MAX_POINTER_BYTES: u64 = 128;

/// Where a client looks for an issuer's catalog.
///
/// Deliberately not the node's `IssuerPolicy`: that type carries substituters
/// and release allow-lists a client has no business enforcing, and reusing it
/// would invite exactly the confusion this module exists to prevent.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub enum Catalog {
    Local(PathBuf),
    Https(String),
}

impl Catalog {
    /// A path is a directory; anything else must be an HTTPS URL.
    ///
    /// Plain HTTP is refused with no override. The registry push has
    /// `--plain-http` because a loopback registry is a normal development
    /// setup, but a catalog pointer fetched over HTTP is a digest an attacker
    /// on the path chooses — and the whole point of the digest is that nobody
    /// downstream has to trust the fetch.
    pub fn parse(source: &str) -> Result<Catalog, String> {
        if source.is_empty() {
            return Err("catalog source is empty".to_string());
        }
        if let Some(rest) = source.strip_prefix("https://") {
            if rest.is_empty() || source.contains(['?', '#']) {
                return Err(format!(
                    "catalog URL `{source}` must have a host and no query or fragment"
                ));
            }
            return Ok(Catalog::Https(source.trim_end_matches('/').to_string()));
        }
        if source.starts_with("http://") {
            return Err(format!(
                "catalog URL `{source}` must use https — a pointer fetched over http is a \
                 digest chosen by whoever is on the path"
            ));
        }
        let path = PathBuf::from(source);
        if !path.is_dir() {
            return Err(format!(
                "catalog `{source}` is neither an https:// URL nor a directory"
            ));
        }
        Ok(Catalog::Local(path))
    }
}

/// `issuer/name` plus the channel to read, as typed on the command line.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct Coordinate {
    pub issuer: String,
    pub name: String,
    pub channel: String,
}

/// Parse `issuer/name[:channel]`, defaulting the channel to `stable`.
///
/// The grammar is deliberately the release reference's own, minus the digest:
/// `imageless::ReleaseReference` validates `issuer/name` after this resolves,
/// so anything accepted here that it would reject fails with its message, not
/// a second dialect of the same rules.
pub fn parse_coordinate(value: &str) -> Result<Coordinate, String> {
    if value.contains("@sha256:") {
        return Err(format!(
            "`{value}` is already pinned — `pin` resolves a channel, and a digest has nothing \
             left to resolve"
        ));
    }
    // Split the channel off the right, but only past the issuer separator: a
    // colon before any `/` means someone typed a registry reference.
    let (identity, channel) = match value.rsplit_once(':') {
        Some((identity, channel)) if identity.contains('/') => (identity, channel),
        Some(_) => {
            return Err(format!(
                "`{value}` is not issuer/name[:channel] — the channel follows the release name"
            ))
        }
        None => (value, "stable"),
    };
    let (issuer, name) = identity.split_once('/').ok_or_else(|| {
        format!("`{value}` is not issuer/name[:channel] — an issuer and a release name are both required")
    })?;
    check_channel(channel)?;
    if issuer.is_empty() || name.is_empty() {
        return Err(format!("`{value}` is not issuer/name[:channel]"));
    }
    Ok(Coordinate {
        issuer: issuer.to_string(),
        name: name.to_string(),
        channel: channel.to_string(),
    })
}

fn check_channel(channel: &str) -> Result<(), String> {
    if valid_segment(channel) {
        Ok(())
    } else {
        Err(format!(
            "channel `{channel}` must be 1-63 characters of [a-z0-9-], not starting or ending \
             with a dash"
        ))
    }
}

/// The same shape `imageless::release::valid_identifier` enforces, applied to a
/// channel so a pointer path cannot contain `..` or a separator.
fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Read `refs/<name>/<channel>` and return the digest it names.
pub fn resolve(
    catalog: &Catalog,
    coordinate: &Coordinate,
    timeout: Duration,
) -> Result<String, String> {
    // Both halves of the path are validated here rather than trusted from the
    // caller. `parse_coordinate` checks the same things, but `Coordinate`'s
    // fields are public, so that check belongs to whoever built the value —
    // and the guarantee this join needs is that no segment is `..` or carries a
    // separator. Checking twice costs a byte scan; not checking costs a read
    // outside the catalog.
    check_channel(&coordinate.channel)?;
    let relative = format!(
        "refs/{}/{}",
        segments(&coordinate.name)?.join("/"),
        coordinate.channel
    );
    let raw = match catalog {
        Catalog::Local(directory) => read_local(directory, &relative)?,
        Catalog::Https(base) => read_https(base, &relative, timeout)?,
    };
    digest_of(&raw, &relative)
}

/// A release name may contain `/` (SPEC's `valid_release_name` allows it), and
/// each segment has to survive the same check as the channel.
fn segments(name: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = name.split('/').collect();
    if !parts.iter().all(|part| valid_segment(part)) {
        return Err(format!(
            "release name `{name}` must be [a-z0-9-] segments separated by `/`"
        ));
    }
    Ok(parts)
}

fn read_local(directory: &Path, relative: &str) -> Result<String, String> {
    let path = directory.join(relative);
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            // A mistyped channel is the likeliest failure here, and the answer
            // is one readdir away — so give it rather than the raw errno.
            return not_published(&path);
        }
        format!("{}: {error}", path.display())
    })?;
    if metadata.is_symlink() {
        // A symlink is how a pointer would escape the catalog, and following
        // one silently would resolve a channel against a file the publisher
        // never put there.
        return Err(format!(
            "{}: channel pointers must be regular files, not symlinks",
            path.display()
        ));
    }
    if metadata.len() > MAX_POINTER_BYTES {
        return Err(format!(
            "{}: channel pointer is {} bytes; a digest is 64",
            path.display(),
            metadata.len()
        ));
    }
    std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
}

/// `refs/<name>/<channel>` is absent: name the channel, and list its siblings
/// when the release itself is published.
fn not_published(path: &Path) -> String {
    let channel = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(directory) = path.parent() else {
        return format!("channel `{channel}` is not published");
    };
    let mut published: Vec<String> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if published.is_empty() {
        return format!(
            "channel `{channel}` is not published — no channels exist at {}",
            directory.display()
        );
    }
    published.sort();
    format!(
        "channel `{channel}` is not published; {} has: {}",
        directory.display(),
        published.join(", ")
    )
}

fn read_https(base: &str, relative: &str, timeout: Duration) -> Result<String, String> {
    let url = format!("{base}/{relative}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        // A redirect is another host's chance to answer for this one; the
        // library's manifest fetch refuses them for the same reason.
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_POINTER_BYTES + 1)
        .read_to_string()
        .map_err(|error| format!("{url}: {error}"))
}

/// The pointer's bytes, validated as a digest.
fn digest_of(raw: &str, relative: &str) -> Result<String, String> {
    let digest = raw.trim();
    let valid = digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(format!(
            "{relative} does not contain a manifest digest: expected 64 lowercase hex digits, \
             got {}",
            preview(digest)
        ));
    }
    Ok(digest.to_string())
}

/// Another server's response body reaches a terminal here, so it is bounded and
/// stripped of anything that could rewrite the line around it.
fn preview(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|character| !character.is_control())
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "nothing printable".to_string()
    } else {
        format!("`{cleaned}`")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kubectl-imageless-catalog-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_channel_defaults_to_stable_and_is_split_off_the_right() {
        assert_eq!(
            parse_coordinate("example/agent").unwrap(),
            Coordinate {
                issuer: "example".to_string(),
                name: "agent".to_string(),
                channel: "stable".to_string(),
            }
        );
        assert_eq!(
            parse_coordinate("example/agent:edge").unwrap().channel,
            "edge"
        );
        // A release name may itself contain slashes.
        let nested = parse_coordinate("example/team/agent:edge").unwrap();
        assert_eq!(nested.name, "team/agent");
        assert_eq!(nested.channel, "edge");
    }

    #[test]
    fn an_already_pinned_reference_is_refused_rather_than_re_resolved() {
        // Silently returning it would make `pin` look like it consulted a
        // catalog that may not even list this digest.
        let error = parse_coordinate(&format!("example/agent@sha256:{DIGEST}")).unwrap_err();
        assert!(error.contains("already pinned"), "{error}");
    }

    #[test]
    fn a_channel_cannot_walk_out_of_the_catalog() {
        // Refused by either gate is fine — what matters is that no traversable
        // segment reaches the `refs/<name>/<channel>` join.
        let root = temporary("traversal");
        for value in [
            "example/agent:../../etc",
            "example/../agent",
            "example/agent:a/b",
            "example/agent:.",
        ] {
            let refused = match parse_coordinate(value) {
                Err(_) => true,
                Ok(coordinate) => resolve(
                    &Catalog::Local(root.clone()),
                    &coordinate,
                    Duration::from_secs(5),
                )
                .is_err_and(|error| error.contains("[a-z0-9-]")),
            };
            assert!(refused, "`{value}` was not refused");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_local_pointer_resolves_with_or_without_a_trailing_newline() {
        let root = temporary("local");
        let channel = root.join("refs/agent");
        std::fs::create_dir_all(&channel).unwrap();
        std::fs::write(channel.join("stable"), format!("{DIGEST}\n")).unwrap();
        std::fs::write(channel.join("edge"), DIGEST).unwrap();
        let catalog = Catalog::Local(root.clone());
        for name in ["stable", "edge"] {
            let coordinate = parse_coordinate(&format!("example/agent:{name}")).unwrap();
            assert_eq!(
                resolve(&catalog, &coordinate, Duration::from_secs(5)).unwrap(),
                DIGEST
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pointer_that_is_not_a_digest_says_what_it_found() {
        let root = temporary("garbage");
        let channel = root.join("refs/agent");
        std::fs::create_dir_all(&channel).unwrap();
        std::fs::write(channel.join("stable"), "<html>404</html>").unwrap();
        let coordinate = parse_coordinate("example/agent").unwrap();
        let error = resolve(
            &Catalog::Local(root.clone()),
            &coordinate,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("64 lowercase hex"), "{error}");
        assert!(error.contains("html"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_symlinked_pointer_is_refused_rather_than_followed() {
        let root = temporary("symlink");
        let channel = root.join("refs/agent");
        std::fs::create_dir_all(&channel).unwrap();
        std::fs::write(root.join("elsewhere"), DIGEST).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), channel.join("stable")).unwrap();
        let coordinate = parse_coordinate("example/agent").unwrap();
        let error = resolve(
            &Catalog::Local(root.clone()),
            &coordinate,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_refuses_a_traversing_channel_it_did_not_parse_itself() {
        // `Coordinate`'s fields are public, so the traversal guard cannot live
        // only in `parse_coordinate`: this is the shape that would reach the
        // path join if a future caller built the value some other way.
        let root = temporary("hand-built");
        let error = resolve(
            &Catalog::Local(root.clone()),
            &Coordinate {
                issuer: "example".to_string(),
                name: "agent".to_string(),
                channel: "../../etc/passwd".to_string(),
            },
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.contains("[a-z0-9-]"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pointer_response_cannot_rewrite_the_terminal() {
        // The body is another server's; control characters never reach stderr.
        let noisy = format!("\u{1b}[2Kfake success{DIGEST}");
        let error = digest_of(&noisy, "refs/agent/stable").unwrap_err();
        assert!(!error.contains('\u{1b}'), "{error}");
    }

    #[test]
    fn http_is_refused_with_no_override() {
        let error = Catalog::parse("http://catalog.example").unwrap_err();
        assert!(error.contains("https"), "{error}");
        assert!(matches!(
            Catalog::parse("https://catalog.example/base/").unwrap(),
            Catalog::Https(base) if base == "https://catalog.example/base"
        ));
    }
}
