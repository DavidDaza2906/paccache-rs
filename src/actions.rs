//! Signatures, sizes and rm/mv (owner: agent C).
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    DryRun,
    Remove,
    Move(PathBuf),
}

/// Candidates plus each existing `X.sig`, right after its package (script order).
pub fn with_sigs(cands: &[PathBuf]) -> Vec<PathBuf> {
    let _ = cands;
    todo!("agent C")
}

/// Sum of file sizes (stat -c %s) of `paths`.
pub fn total_size(paths: &[PathBuf]) -> io::Result<u64> {
    let _ = paths;
    todo!("agent C")
}

/// Whether removing from `cachedirs` / moving into `movedir` needs root (not writable).
pub fn needs_root(cachedirs: &[PathBuf], movedir: Option<&Path>) -> bool {
    let _ = (cachedirs, movedir);
    todo!("agent C")
}

/// Perform Remove/Move. Output and errors must match `xargs -0 rm [-f] [-v]` /
/// `xargs -0 mv [-f] [-v] -t DIR` (coreutils), via `sudo` when `as_root` and euid != 0.
/// Returns the exit status the script would end with (errexit).
pub fn apply(op: &Op, paths: &[PathBuf], force: bool, verbose: bool, as_root: bool) -> i32 {
    let _ = (op, paths, force, verbose, as_root);
    todo!("agent C")
}
