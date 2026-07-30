//! Deterministic packing of a source directory into an OCI tar layer.
//!
//! The layer places the source tree at `etc/imageless/` — the path the node
//! discovers embedded flakes at — and charges the node's staging budgets
//! while packing, so a directory this module accepts is one the node will
//! stage, and a refusal happens at authoring time with the offending path
//! in hand. The tar bytes are a pure function of the tree contents: entries
//! are walked in byte-lexicographic order, ownership is root, timestamps are
//! zero, and modes collapse to 0755/0644 by the same any-execute-bit rule
//! the node's staging applies.

use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use imageless::{MAX_STAGED_SOURCE_BYTES, MAX_STAGED_SOURCE_ENTRIES};

/// Directory inside the layer the source tree lands at — the parent of
/// [`imageless::EMBEDDED_FLAKE_PATH`], which the node's zero-config discovery
/// reads.
pub const LAYER_ROOT: &str = "etc/imageless";

const BLOCK: usize = 512;
const VCS_DIRECTORIES: [&str; 4] = [".git", ".hg", ".jj", ".svn"];

pub struct PackOptions {
    pub include_vcs: bool,
}

#[cfg_attr(test, derive(Debug))]
pub struct PackedLayer {
    pub tar: Vec<u8>,
    pub entries: usize,
    pub bytes: u64,
    pub skipped_vcs: Vec<PathBuf>,
    pub has_lock: bool,
}

/// Mirror of the node's staging budget: every staged node — the tree root,
/// each directory, each file — costs one entry before its type is even
/// considered, and file bytes accrue against the byte bound.
#[derive(Default)]
struct Budget {
    bytes: u64,
    entries: usize,
}

impl Budget {
    fn charge_entry(&mut self, path: &Path) -> Result<(), String> {
        self.entries += 1;
        if self.entries > MAX_STAGED_SOURCE_ENTRIES {
            return Err(format!(
                "{}: the tree exceeds the node's staging bound of {MAX_STAGED_SOURCE_ENTRIES} entries",
                path.display()
            ));
        }
        Ok(())
    }

    fn charge_bytes(&mut self, path: &Path, length: u64) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(length);
        if self.bytes > MAX_STAGED_SOURCE_BYTES {
            return Err(format!(
                "{}: the tree exceeds the node's staging bound of {MAX_STAGED_SOURCE_BYTES} bytes",
                path.display()
            ));
        }
        Ok(())
    }
}

pub fn pack_source(root: &Path, options: &PackOptions) -> Result<PackedLayer, String> {
    let metadata =
        std::fs::symlink_metadata(root).map_err(|error| format!("{}: {error}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{}: not a directory", root.display()));
    }
    let flake = root.join("flake.nix");
    match std::fs::symlink_metadata(&flake) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(format!(
                "{}: flake.nix must be a regular file (the node rejects symlinked flakes)",
                flake.display()
            ))
        }
        Err(_) => {
            return Err(format!(
                "{}: no flake.nix — an imageless seed is a directory whose flake builds the rootfs",
                root.display()
            ))
        }
    }
    let has_lock = std::fs::symlink_metadata(root.join("flake.lock"))
        .map(|metadata| metadata.is_file())
        .unwrap_or(false);

    let mut writer = LayerWriter::default();
    let mut budget = Budget::default();
    let mut skipped_vcs = Vec::new();
    // `etc/` is scaffolding around the staged tree, not part of it: the node
    // stages `/etc/imageless` as the tree root, so the root is the first
    // charged entry and the parent costs nothing.
    writer.directory("etc")?;
    budget.charge_entry(root)?;
    writer.directory(LAYER_ROOT)?;
    pack_directory(
        root,
        LAYER_ROOT,
        options,
        &mut writer,
        &mut budget,
        &mut skipped_vcs,
    )?;
    writer.finish();
    Ok(PackedLayer {
        tar: writer.tar,
        entries: budget.entries,
        bytes: budget.bytes,
        skipped_vcs,
        has_lock,
    })
}

fn pack_directory(
    directory: &Path,
    layer_path: &str,
    options: &PackOptions,
    writer: &mut LayerWriter,
    budget: &mut Budget,
    skipped_vcs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut names = Vec::new();
    let listing = std::fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in listing {
        let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
        let name = entry.file_name();
        // The node stages raw byte names; refusing non-UTF-8 here is
        // deliberately stricter (the tar writer is String-based) and fails
        // closed at authoring time.
        match name.into_string() {
            Ok(name) => names.push(name),
            Err(name) => {
                return Err(format!(
                    "{}: file name is not valid UTF-8",
                    directory.join(name).display()
                ))
            }
        }
    }
    names.sort();
    for name in names {
        let path = directory.join(&name);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "{}: the node rejects embedded sources containing symlinks",
                path.display()
            ));
        }
        if file_type.is_dir() {
            if !options.include_vcs && VCS_DIRECTORIES.contains(&name.as_str()) {
                skipped_vcs.push(path);
                continue;
            }
            budget.charge_entry(&path)?;
            let child = format!("{layer_path}/{name}");
            writer.directory(&child)?;
            pack_directory(&path, &child, options, writer, budget, skipped_vcs)?;
        } else if file_type.is_file() {
            budget.charge_entry(&path)?;
            let data = read_regular(&path, budget)?;
            // Any execute bit counts, matching the node's staging collapse —
            // an owner-only test would silently strip group/other execute
            // bits and fork the staged tree's store hash.
            let mode = if metadata.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            writer.file(&format!("{layer_path}/{name}"), mode, &data)?;
        } else {
            return Err(format!(
                "{}: only regular files and directories can be staged",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Open without following symlinks, verify the opened descriptor is a regular
/// file, and read exactly the length fstat reports — a file swapped for a
/// symlink or special file, or one that changes size mid-pack, is an error
/// rather than a silent divergence between the packed digest and the tree.
fn read_regular(path: &Path, budget: &mut Budget) -> Result<Vec<u8>, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{}: replaced by a non-regular file while packing",
            path.display()
        ));
    }
    let expected = metadata.len();
    // Charge before reading so an oversized file fails on its stat size
    // instead of being pulled into memory first.
    budget.charge_bytes(path, expected)?;
    let mut data = Vec::with_capacity(expected as usize);
    file.take(expected + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if data.len() as u64 != expected {
        return Err(format!("{}: changed while packing", path.display()));
    }
    Ok(data)
}

/// The one ustar writer in the plugin. `placeholder` builds its layer with
/// this too, so the determinism rules above — and the golden digest test that
/// guards them — cover every layer the plugin can push.
#[derive(Default)]
pub(crate) struct LayerWriter {
    pub(crate) tar: Vec<u8>,
}

impl LayerWriter {
    pub(crate) fn directory(&mut self, path: &str) -> Result<(), String> {
        let header = header(&format!("{path}/"), 0o755, 0, b'5')?;
        self.tar.extend_from_slice(&header);
        Ok(())
    }

    pub(crate) fn file(&mut self, path: &str, mode: u32, data: &[u8]) -> Result<(), String> {
        let header = header(path, mode, data.len() as u64, b'0')?;
        self.tar.extend_from_slice(&header);
        self.tar.extend_from_slice(data);
        let padding = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.tar.extend_from_slice(&vec![0; padding]);
        Ok(())
    }

    pub(crate) fn finish(&mut self) {
        self.tar.extend_from_slice(&[0; 2 * BLOCK]);
    }
}

fn header(path: &str, mode: u32, size: u64, typeflag: u8) -> Result<[u8; BLOCK], String> {
    let (prefix, name) = split_name(path)?;
    let mut block = [0u8; BLOCK];
    block[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut block[100..108], u64::from(mode));
    octal(&mut block[108..116], 0); // uid
    octal(&mut block[116..124], 0); // gid
    octal(&mut block[124..136], size);
    octal(&mut block[136..148], 0); // mtime
    block[148..156].fill(b' '); // checksum counts as spaces while summing
    block[156] = typeflag;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    // uname/gname stay empty and device numbers stay zeroed: the tree is
    // packed as root-owned content with no identity leaked from the client.
    block[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    let sum: u64 = block.iter().map(|byte| u64::from(*byte)).sum();
    let checksum = format!("{sum:06o}\0 ");
    block[148..156].copy_from_slice(checksum.as_bytes());
    Ok(block)
}

fn octal(field: &mut [u8], value: u64) {
    let digits = format!("{value:0width$o}", width = field.len() - 1);
    field[..digits.len()].copy_from_slice(digits.as_bytes());
    field[digits.len()] = 0;
}

/// Split a path into the ustar prefix/name fields: the whole path when it
/// fits in the 100-byte name, otherwise the rightmost `/` that leaves both
/// halves in bounds. A component that cannot be split fails closed instead
/// of truncating.
fn split_name(path: &str) -> Result<(&str, &str), String> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    let mut split = None;
    for (index, byte) in path.bytes().enumerate() {
        if byte == b'/' && index <= 155 && path.len() - index - 1 <= 100 && index + 1 < path.len() {
            split = Some(index);
        }
    }
    match split {
        Some(index) => Ok((&path[..index], &path[index + 1..])),
        None => Err(format!(
            "{path}: path does not fit in a tar header (name up to 100 bytes, \
             prefix up to 155); shorten the file name"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    const OPTIONS: PackOptions = PackOptions { include_vcs: false };

    fn temporary(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let path = std::env::temp_dir().join(format!(
            "kubectl-imageless-{label}-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn seed(label: &str) -> PathBuf {
        let root = temporary(label);
        std::fs::write(root.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
        root
    }

    fn sha256(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn packing_is_a_pure_function_of_the_tree() {
        let first = seed("determinism-a");
        let second = seed("determinism-b");
        // Create the same tree with different filesystem insertion orders.
        for (root, order) in [
            (&first, ["zeta", "alpha", "mid"]),
            (&second, ["mid", "zeta", "alpha"]),
        ] {
            for name in order {
                std::fs::write(root.join(name), name).unwrap();
            }
            std::fs::create_dir(root.join("sub")).unwrap();
            std::fs::write(root.join("sub/nested"), "nested").unwrap();
        }
        let left = pack_source(&first, &OPTIONS).unwrap();
        let right = pack_source(&second, &OPTIONS).unwrap();
        assert_eq!(left.tar, right.tar);
        // root + flake.nix + alpha/mid/zeta + sub + sub/nested
        assert_eq!(left.entries, 7);
        assert_eq!(left.entries, right.entries);
        std::fs::remove_dir_all(first).unwrap();
        std::fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn any_execute_bit_marks_the_file_executable_like_the_node() {
        // Group-execute without owner-execute must collapse the same way the
        // node stages it (0o111, not owner-only), or the packed tree forks
        // from a directly staged one.
        let group_exec = seed("mode-group-exec");
        let owner_exec = seed("mode-owner-exec");
        let plain = seed("mode-plain");
        for (root, mode) in [(&group_exec, 0o614), (&owner_exec, 0o755), (&plain, 0o644)] {
            std::fs::write(root.join("tool"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(root.join("tool"), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let group_exec_tar = pack_source(&group_exec, &OPTIONS).unwrap().tar;
        let owner_exec_tar = pack_source(&owner_exec, &OPTIONS).unwrap().tar;
        let plain_tar = pack_source(&plain, &OPTIONS).unwrap().tar;
        assert_eq!(group_exec_tar, owner_exec_tar);
        assert_ne!(group_exec_tar, plain_tar);
        for root in [group_exec, owner_exec, plain] {
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn golden_layer_digest() {
        let root = seed("golden");
        std::fs::write(root.join("data"), "golden").unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let packed = pack_source(&root, &OPTIONS).unwrap();
        assert_eq!(
            sha256(&packed.tar),
            "8d1edcec8bc7def96e4b49e47c0efea98d53c26411f2e7a2a23adca329946b66",
            "layer bytes changed: the packed digest is part of the tool's contract"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_flake_fails_at_authoring_time() {
        let root = temporary("no-flake");
        std::fs::write(root.join("data"), "x").unwrap();
        let error = pack_source(&root, &OPTIONS).unwrap_err();
        assert!(error.contains("no flake.nix"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlinks_are_rejected_by_name() {
        let root = seed("symlink");
        std::os::unix::fs::symlink("flake.nix", root.join("link")).unwrap();
        let error = pack_source(&root, &OPTIONS).unwrap_err();
        assert!(error.contains("link"), "{error}");
        assert!(error.contains("symlink"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn special_files_are_rejected_by_name() {
        let root = seed("fifo");
        let fifo = root.join("pipe");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
        let error = pack_source(&root, &OPTIONS).unwrap_err();
        assert!(error.contains("pipe"), "{error}");
        assert!(error.contains("regular files"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entry_budget_matches_the_node_including_the_tree_root() {
        let root = seed("entries");
        // Root + flake.nix leaves room for exactly MAX-2 more entries.
        for index in 0..MAX_STAGED_SOURCE_ENTRIES - 2 {
            std::fs::write(root.join(format!("f{index:05}")), "x").unwrap();
        }
        assert_eq!(
            pack_source(&root, &OPTIONS).unwrap().entries,
            MAX_STAGED_SOURCE_ENTRIES
        );
        std::fs::write(root.join("one-too-many"), "x").unwrap();
        let error = pack_source(&root, &OPTIONS).unwrap_err();
        assert!(error.contains("entries"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn byte_budget_fails_on_the_stat_size_with_the_offending_path() {
        let root = seed("bytes");
        let big = vec![0u8; MAX_STAGED_SOURCE_BYTES as usize + 1];
        std::fs::write(root.join("big"), big).unwrap();
        let error = pack_source(&root, &OPTIONS).unwrap_err();
        assert!(error.contains("big"), "{error}");
        assert!(error.contains("bytes"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_through_a_symlink_is_refused_by_the_kernel() {
        let root = temporary("nofollow");
        std::fs::write(root.join("target"), "secret").unwrap();
        std::os::unix::fs::symlink("target", root.join("swapped")).unwrap();
        let mut budget = Budget::default();
        let error = read_regular(&root.join("swapped"), &mut budget).unwrap_err();
        assert!(error.contains("swapped"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vcs_directories_are_skipped_unless_asked_for() {
        let root = seed("vcs");
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        let skipped = pack_source(&root, &OPTIONS).unwrap();
        assert_eq!(skipped.skipped_vcs, vec![root.join(".git")]);
        assert_eq!(skipped.entries, 2); // root + flake.nix only
        let packed = pack_source(&root, &PackOptions { include_vcs: true }).unwrap();
        assert!(packed.skipped_vcs.is_empty());
        assert_eq!(packed.entries, 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_paths_split_into_prefix_and_name_or_fail_closed() {
        let deep = "d".repeat(80);
        let leaf = "f".repeat(80);
        let long = format!("etc/imageless/{deep}/{leaf}");
        let split = split_name(&long).unwrap();
        assert_eq!(split.0, format!("etc/imageless/{deep}"));
        assert_eq!(split.1, leaf);
        let unsplittable = format!("etc/imageless/{}", "f".repeat(120));
        assert!(split_name(&unsplittable).is_err());

        let root = seed("longname");
        std::fs::create_dir(root.join(&deep)).unwrap();
        std::fs::write(root.join(&deep).join(&leaf), "deep").unwrap();
        pack_source(&root, &OPTIONS).unwrap();
        std::fs::write(root.join("f".repeat(120)), "wide").unwrap();
        let error = pack_source(&root, &OPTIONS).unwrap_err();
        assert!(error.contains("tar header"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gnu_tar_round_trips_the_layer() {
        let root = seed("roundtrip");
        std::fs::write(root.join("data"), "payload").unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let packed = pack_source(&root, &OPTIONS).unwrap();
        let work = temporary("roundtrip-extract");
        std::fs::write(work.join("layer.tar"), &packed.tar).unwrap();
        let extraction = std::process::Command::new("tar")
            .arg("-xf")
            .arg(work.join("layer.tar"))
            .arg("-C")
            .arg(&work)
            .output();
        let Ok(extraction) = extraction else {
            eprintln!("tar not available; skipping round-trip");
            return;
        };
        assert!(
            extraction.status.success(),
            "{}",
            String::from_utf8_lossy(&extraction.stderr)
        );
        let staged = work.join(LAYER_ROOT);
        assert_eq!(
            std::fs::read_to_string(staged.join("data")).unwrap(),
            "payload"
        );
        let mode = std::fs::metadata(staged.join("bin/tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(staged.join("flake.nix").is_file());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(work).unwrap();
    }
}
