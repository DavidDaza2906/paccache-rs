//! Listing package files in a cache dir (owner: agent C).
use crate::pkgname::PkgFile;
use std::io;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Entry {
    pub file: PkgFile,
    /// Seconds since epoch with fraction (find's %A@ / %T@). 0.0 when not requested.
    pub atime: f64,
    pub mtime: f64,
}

/// `with_times == false`: non-recursive glob `*.pkg.tar!(*.sig)` (no dotfiles).
/// `with_times == true`: recursive like `find DIR -name '*.pkg.tar*.sig' -prune -o -name '*.pkg.tar*'`.
/// `dir` must already be absolute. Result sorted with `pacsort_files_cmp`.
pub fn scan(dir: &Path, with_times: bool) -> io::Result<Vec<Entry>> {
    let _ = (dir, with_times);
    todo!("agent C")
}
