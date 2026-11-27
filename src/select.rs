//! Candidate selection, the awk `pkgfilter` END block (owner: agent C).
use crate::scan::Entry;
use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub keep: usize,
    pub arch: Option<String>,
    /// Epoch seconds; None = not set (the script's 0).
    pub min_atime: Option<i64>,
    pub min_mtime: Option<i64>,
    /// Non-empty = only these names.
    pub whitelist: HashSet<String>,
    pub blacklist: HashSet<String>,
}

/// Entries come sorted (pacsort order). Returns candidates (all but the newest `keep`
/// per (name, arch), subject to arch/time/white/blacklist rules), order unspecified
/// like awk's (callers sort when printing).
pub fn candidates(entries: Vec<Entry>, f: &Filter) -> Vec<Entry> {
    let _ = (entries, f);
    todo!("agent C")
}
