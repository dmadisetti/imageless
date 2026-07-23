//! Bundle-scoped Nix GC-root lifecycle.

use crate::materialize::{ErrorCategory, ResolutionError};
use crate::{GC_ROOTS_DIR_NAME, GC_ROOT_NAME};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn prepare_gc_root(root: &Path) -> Result<(), ResolutionError> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(root).map_err(|_| {
                ResolutionError::new(
                    ErrorCategory::RootRegistration,
                    "stale bundle GC root could not be removed",
                    true,
                )
            })
        }
        Ok(_) => Err(ResolutionError::new(
            ErrorCategory::RootCollision,
            "reserved bundle GC-root path is not a symbolic link",
            false,
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ResolutionError::new(
            ErrorCategory::RootRegistration,
            "bundle GC-root path could not be inspected",
            true,
        )),
    }
}

pub(crate) fn gc_root_path(bundle: &Path, index: usize) -> Result<PathBuf, ResolutionError> {
    if index == 0 {
        return Ok(bundle.join(GC_ROOT_NAME));
    }
    let directory = bundle.join(GC_ROOTS_DIR_NAME);
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(ResolutionError::new(
                ErrorCategory::RootCollision,
                "reserved bundle GC-root directory is not a directory",
                false,
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(&directory).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    ResolutionError::new(
                        ErrorCategory::RootCollision,
                        "reserved bundle GC-root directory changed during registration",
                        true,
                    )
                } else {
                    ResolutionError::new(
                        ErrorCategory::RootRegistration,
                        "bundle GC-root directory could not be created",
                        true,
                    )
                }
            })?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).map_err(
                |_| {
                    ResolutionError::new(
                        ErrorCategory::RootRegistration,
                        "bundle GC-root directory permissions could not be secured",
                        true,
                    )
                },
            )?;
        }
        Err(_) => {
            return Err(ResolutionError::new(
                ErrorCategory::RootRegistration,
                "bundle GC-root directory could not be inspected",
                true,
            ))
        }
    }
    Ok(directory.join(index.to_string()))
}

pub(crate) fn validate_registered_root(
    root: &Path,
    store_path: &str,
    category: ErrorCategory,
) -> Result<(), ResolutionError> {
    let metadata = std::fs::symlink_metadata(root).map_err(|_| {
        ResolutionError::new(
            category.clone(),
            "Nix did not create the bundle GC root",
            true,
        )
    })?;
    if !metadata.file_type().is_symlink() {
        return Err(ResolutionError::new(
            category,
            "Nix created an invalid bundle GC root",
            false,
        ));
    }
    let target = std::fs::read_link(root).map_err(|_| {
        ResolutionError::new(category.clone(), "bundle GC root could not be read", true)
    })?;
    if target != Path::new(store_path) {
        return Err(ResolutionError::new(
            category,
            "bundle GC root points at a different store path",
            false,
        ));
    }
    Ok(())
}

pub fn remove_gc_root(root: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::remove_file(root),
        // A passthrough bundle is independent of imageless state and may have
        // an unrelated entry with this name. Never remove a non-symlink.
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Remove only the indirect Nix roots owned by imageless. Unexpected regular
/// files and directories at reserved paths are preserved rather than treated
/// as cleanup authority.
pub fn remove_bundle_gc_roots(bundle: &Path) -> io::Result<()> {
    remove_gc_root(&bundle.join(GC_ROOT_NAME))?;
    let directory = bundle.join(GC_ROOTS_DIR_NAME);
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<usize>().is_ok() {
            remove_gc_root(&entry.path())?;
        }
    }
    match std::fs::remove_dir(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error),
    }
}
