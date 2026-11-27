//! Package filename parsing and `pacsort --files` ordering (owner: agent A).
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// A package file as paccache's awk `parse_filename` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgFile {
    /// Full path as listed (absolute, cachedir-joined).
    pub path: PathBuf,
    /// Name: dash-split parts 1..n-3 of the basename, rejoined with '-'.
    pub name: String,
    /// Arch: last dash-split part up to its first '.'.
    pub arch: String,
}

/// Parse like the awk `parse_filename` (no validation: mirror awk on odd names).
pub fn parse(path: &Path) -> PkgFile {
    let _ = path;
    todo!("agent A")
}

/// Ordering identical to `pacsort --files` for two package paths.
pub fn pacsort_files_cmp(a: &Path, b: &Path) -> Ordering {
    let _ = (a, b);
    todo!("agent A")
}
