//! Pure-Rust port of libalpm's `alpm_pkg_vercmp` (owner: agent A).
use std::cmp::Ordering;

/// Compare two full versions (`[epoch:]pkgver[-pkgrel]`) exactly like `vercmp(8)`.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    let _ = (a, b);
    todo!("agent A")
}
