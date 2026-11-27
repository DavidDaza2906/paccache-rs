//! libmakepkg-style messages and paccache's summary (owner: agent B).
use crate::actions::Op;
use std::path::PathBuf;

pub struct Out {
    /// Colors on: stderr is a tty and no --nocolor (applies to stdout msg too).
    pub color: bool,
    pub quiet: bool,
}

impl Out {
    /// `==> text` on stdout (suppressed by quiet).
    pub fn msg(&self, text: &str) {
        let _ = text;
        todo!("agent B")
    }
    /// `==> ERROR: text` on stderr (never suppressed).
    pub fn error(&self, text: &str) {
        let _ = text;
        todo!("agent B")
    }
}

/// awk size_to_human.
pub fn size_to_human(bytes: u64) -> String {
    let _ = bytes;
    todo!("agent B")
}

/// The script's `summarize`: prints candidate list (dry run + verbose) and final line.
/// `pkgs` = candidates WITH sigs (as the script passes them); `pkgcount` excludes sigs.
pub fn summarize(
    out: &Out,
    op: &Op,
    verbose: u8,
    null: bool,
    pkgcount: usize,
    pkgs: &[PathBuf],
    totalsaved: u64,
) {
    let _ = (out, op, verbose, null, pkgcount, pkgs, totalsaved);
    todo!("agent B")
}
