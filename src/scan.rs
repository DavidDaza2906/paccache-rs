//! Listing package files in a cache dir (owner: agent C).
//!
//! Two modes, exactly like the script:
//!
//! * Without `--min-atime/--min-mtime`: `printf '%s\n' "$PWD"/*.pkg.tar!(*.sig)` with
//!   `nullglob` + `extglob`. Non-recursive, no dotfiles, any file type (regular files,
//!   directories, symlinks including dangling ones). A name matches iff it does not
//!   start with `.`, contains `.pkg.tar` and does not end in `.sig` (so `x.pkg.tar`,
//!   `x.pkg.tar.zst.part`, `x.pkg.tarsig` match; `x.pkg.tar.zst.sig` does not). Paths
//!   are `DIR + "/" + name` textually (so `/` gives `//name`, like bash).
//!   bash sorts glob results with `strcoll`; we do the same, see [`init_collation`].
//! * With times: `find "$PWD" -name '*.pkg.tar*.sig' -prune -o \( -name '*.pkg.tar*'
//!   -printf '%A@ %T@ %p\n' \)`. Recursive, dotfiles included, symlinks not followed —
//!   not even the starting point: a cache dir that is itself a symlink is not descended
//!   (only the link itself is listed if its own name matches). Entries come in find's
//!   order (readdir order, preorder; gnulib fts sorts chunks of more than 10000 entries
//!   by inode except on tmpfs/NFS/CIFS). Times are find's `%A@`/`%T@` text
//!   (`SEC.NSEC0`, find's quirky formatting of negative times included) parsed as f64,
//!   the same double awk's `strtonum` gets.
//!
//! The listing order matters because the final sort (`pacsort --files`) is stable and
//! many files compare equal (same name+version, different compression, `1` vs `01`).
//!
//! Not replicated: names containing a newline (the script splits them into two lines).
use crate::pkgname::{self, PkgFile};
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Entry {
    pub file: PkgFile,
    /// Seconds since epoch with fraction (find's %A@ / %T@). 0.0 when not requested.
    pub atime: f64,
    pub mtime: f64,
}

/// `with_times == false`: non-recursive glob `*.pkg.tar!(*.sig)` (no dotfiles).
/// `with_times == true`: recursive like `find DIR -name '*.pkg.tar*.sig' -prune -o -name '*.pkg.tar*'`.
/// `dir` must already be absolute — the logical `$PWD` bash has after `pushd cachedir`
/// (no `..`/`.` components, no trailing slash, symlinks NOT resolved). Result sorted
/// with `pacsort_files_cmp` (stable, from the shell's listing order).
///
/// Never fails in practice: like the script, an unreadable dir yields an empty glob,
/// and find-mode errors are reported on stderr (`find: ‘path’: Permission denied`)
/// while the walk continues.
pub fn scan(dir: &Path, with_times: bool) -> io::Result<Vec<Entry>> {
    let raw: Vec<(PathBuf, f64, f64)> = if with_times {
        list_find(dir)
            .into_iter()
            .map(|f| {
                let (a, m) = (f.atime_f64(), f.mtime_f64());
                (f.path, a, m)
            })
            .collect()
    } else {
        list_glob(dir).into_iter().map(|p| (p, 0.0, 0.0)).collect()
    };
    let mut entries: Vec<Entry> = raw
        .into_iter()
        .map(|(p, atime, mtime)| Entry { file: pkgname::parse(&p), atime, mtime })
        .collect();
    entries.sort_by(|a, b| pkgname::pacsort_files_cmp(&a.file.path, &b.file.path));
    Ok(entries)
}

/// Make glob ordering follow the user's `LC_COLLATE` (as bash does), by calling
/// `setlocale(LC_COLLATE, "")` once. Call it early from `main`; without it ordering is
/// bytewise (the C locale). Idempotent and thread-safe w.r.t. itself.
pub fn init_collation() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        libc::setlocale(libc::LC_COLLATE, c"".as_ptr());
    });
}

// ---------------------------------------------------------------------------
// glob mode

/// Does `name` match bash's extglob `*.pkg.tar!(*.sig)` (dotglob off)?
fn glob_matches(name: &[u8]) -> bool {
    !name.starts_with(b".") && contains(name, b".pkg.tar") && !name.ends_with(b".sig")
}

/// Unsorted-by-pacsort glob listing, in bash's order (strcoll).
pub(crate) fn list_glob(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<CString> = rd
        .filter_map(Result::ok)
        .map(|e| e.file_name().into_vec())
        .filter(|n| glob_matches(n))
        .filter_map(|n| CString::new(n).ok())
        .collect();
    names.sort_by(|a, b| unsafe { libc::strcoll(a.as_ptr(), b.as_ptr()) }.cmp(&0));
    let mut prefix = dir.as_os_str().as_bytes().to_vec();
    prefix.push(b'/');
    names
        .into_iter()
        .map(|n| {
            let mut p = prefix.clone();
            p.extend_from_slice(n.as_bytes());
            PathBuf::from(OsString::from_vec(p))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// find mode

/// One `find -printf '%A@ %T@ %p'` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub path: PathBuf,
    pub atime: (i64, i64),
    pub mtime: (i64, i64),
}

/// find's `%A@`-style text: seconds, '.', nanoseconds padded to 9 digits, then '0'.
/// (For negative times with a fraction this is not the true value; find does this too.)
pub(crate) fn find_time_text((sec, nsec): (i64, i64)) -> String {
    format!("{sec}.{nsec:09}0")
}

impl Found {
    fn atime_f64(&self) -> f64 {
        find_time_text(self.atime).parse().unwrap_or(0.0)
    }
    fn mtime_f64(&self) -> f64 {
        find_time_text(self.mtime).parse().unwrap_or(0.0)
    }
}

/// fnmatch(`*.pkg.tar*`) without FNM_PERIOD.
fn find_matches(name: &[u8]) -> bool {
    contains(name, b".pkg.tar")
}

/// fnmatch(`*.pkg.tar*.sig`).
fn find_prunes(name: &[u8]) -> bool {
    match find_sub(name, b".pkg.tar") {
        Some(i) => name.len() >= i + 8 + 4 && name.ends_with(b".sig"),
        None => false,
    }
}

const FTS_INODE_SORT_THRESHOLD: usize = 10_000;
const FTS_MAX_READDIR_ENTRIES: usize = 100_000;
const S_MAGIC_CIFS: i64 = 0xFF53_4D42;
const S_MAGIC_NFS: i64 = 0x6969;
const S_MAGIC_TMPFS: i64 = 0x0102_1994;

/// Unsorted find listing, in find's traversal order. Errors go to stderr like find.
pub(crate) fn list_find(dir: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    let bytes = dir.as_os_str().as_bytes();
    let trimmed = {
        let t = bytes.iter().rposition(|&c| c != b'/').map_or(&bytes[..0], |i| &bytes[..=i]);
        if t.is_empty() && !bytes.is_empty() {
            &bytes[..1]
        } else {
            t
        }
    };
    let name = match trimmed.iter().rposition(|&c| c == b'/') {
        Some(i) if trimmed.len() > 1 => &trimmed[i + 1..],
        _ => trimmed,
    };
    match fs::symlink_metadata(dir) {
        Ok(md) => visit(dir, name, &md, &mut out),
        Err(e) => find_error(dir, &e),
    }
    out
}

fn visit(path: &Path, name: &[u8], md: &fs::Metadata, out: &mut Vec<Found>) {
    if find_prunes(name) {
        return;
    }
    if find_matches(name) {
        out.push(Found {
            path: path.to_path_buf(),
            atime: (md.atime(), md.atime_nsec()),
            mtime: (md.mtime(), md.mtime_nsec()),
        });
    }
    if !md.file_type().is_dir() {
        return;
    }
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return find_error(path, &e),
    };
    let mut children: Vec<(OsString, u64)> = Vec::new();
    for ent in rd {
        match ent {
            Ok(ent) => children.push((ent.file_name(), ent.ino())),
            Err(e) => {
                find_error(path, &e);
                break;
            }
        }
    }
    if children.len() > FTS_INODE_SORT_THRESHOLD && inode_sort_useful(path) {
        for chunk in children.chunks_mut(FTS_MAX_READDIR_ENTRIES) {
            if chunk.len() > FTS_INODE_SORT_THRESHOLD {
                chunk.sort_by_key(|c| c.1);
            }
        }
    }
    let mut base = path.as_os_str().as_bytes().to_vec();
    if !base.ends_with(b"/") {
        base.push(b'/');
    }
    for (child, _) in children {
        let mut p = base.clone();
        p.extend_from_slice(child.as_bytes());
        let p = PathBuf::from(OsString::from_vec(p));
        match fs::symlink_metadata(&p) {
            Ok(md) => visit(&p, child.as_bytes(), &md, out),
            Err(e) => find_error(&p, &e),
        }
    }
}

fn inode_sort_useful(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else { return true };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return true;
    }
    #[allow(clippy::unnecessary_cast)]
    let t = st.f_type as i64;
    !matches!(t, S_MAGIC_CIFS | S_MAGIC_NFS | S_MAGIC_TMPFS)
}

/// `find: ‘PATH’: strerror` (find's locale quoting in a UTF-8 locale).
fn find_error(path: &Path, e: &io::Error) {
    eprintln!("find: \u{2018}{}\u{2019}: {}", path.to_string_lossy(), strerror(e));
}

/// Plain C `strerror` text of an io::Error (without Rust's " (os error N)").
pub(crate) fn strerror(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => unsafe {
            std::ffi::CStr::from_ptr(libc::strerror(code)).to_string_lossy().into_owned()
        },
        None => e.to_string(),
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find_sub(hay, needle).is_some()
}

#[cfg(test)]
fn os(b: &[u8]) -> &std::ffi::OsStr {
    std::ffi::OsStr::from_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    fn tmpdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "paccache-rs-scan-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn touch(p: &Path) {
        fs::write(p, b"x").unwrap();
    }

    /// A tree full of odd cases. `root` itself is the cache dir.
    fn weird_tree(root: &Path) {
        let names: &[&[u8]] = &[
            b"a-1-1-x86_64.pkg.tar.zst",
            b"a-1-1-x86_64.pkg.tar.zst.sig",
            b"a-2-1-x86_64.pkg.tar.xz",
            b"x.pkg.tar",
            b"x.pkg.tar.sig",
            b"x.pkg.tarsig",
            b"x.pkg.tar.sigx",
            b"y.pkg.tar.zst.part",
            b".hid-1-1-any.pkg.tar.zst",
            b".pkg.tar",
            b"sp ace-1-1-any.pkg.tar.xz",
            b"a.pkg.tar.sig.pkg.tar.zst",
            b"z.pkg.tar.sig.x",
            b"\xff-1-1-any.pkg.tar.zst",
            b"q*.pkg.tar",
            b"x.PKG.TAR",
            b"pkg.tar",
            b"Foo-1-1-any.pkg.tar.zst",
            b"foo-1+0-1-any.pkg.tar.zst",
            b"foo-1.0-1-any.pkg.tar.zst",
            b"foo-1_0-1-any.pkg.tar.zst",
            b"foo-1-1-ANY.pkg.tar.zst",
            b"foo-1-1-any.pkg.tar.zst",
            b"\xc3\xa9-1-1-any.pkg.tar.zst",
            b"e-1-1-any.pkg.tar.zst",
            b"plain.txt",
            b".sig",
        ];
        for n in names {
            touch(&root.join(os(n)));
        }
        for d in ["d.pkg.tar.zst", "sub", ".hidden", "pruned.pkg.tar.sig", "sub/deeper"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in [
            "d.pkg.tar.zst/inner-1-1-any.pkg.tar.zst",
            "sub/s-1-1-any.pkg.tar.zst",
            "sub/s-1-1-any.pkg.tar.zst.sig",
            "sub/deeper/t-1-1-any.pkg.tar.gz",
            ".hidden/h-1-1-any.pkg.tar.zst",
            "pruned.pkg.tar.sig/never-1-1-any.pkg.tar.zst",
        ] {
            touch(&root.join(f));
        }
        symlink("nowhere", root.join("broken.pkg.tar.zst")).unwrap();
        symlink("sub", root.join("link.pkg.tar.zst")).unwrap();
        symlink("sub", root.join("linkdir")).unwrap();
        symlink("a-1-1-x86_64.pkg.tar.zst", root.join("alias-1-1-x86_64.pkg.tar.zst")).unwrap();
        // pre-epoch / fractional times
        let t = root.join("e-1-1-any.pkg.tar.zst");
        let st = Command::new("touch").arg("-d").arg("1960-01-01 00:00:00.25 UTC").arg(&t).status();
        assert!(st.unwrap().success());
        let t = root.join("x.pkg.tar");
        Command::new("touch").arg("-d").arg("@0.5").arg(&t).status().unwrap();
        Command::new("touch").arg("-a").arg("-d").arg("@-0.5").arg(&t).status().unwrap();
    }

    fn bash_glob(dir: &Path) -> Vec<PathBuf> {
        let o = Command::new("/usr/bin/bash")
            .arg("-c")
            .arg("shopt -s nullglob extglob\ncd \"$1\" || exit 1\nprintf '%s\\0' \"$PWD\"/*.pkg.tar!(*.sig)")
            .arg("bash")
            .arg(dir)
            .output()
            .unwrap();
        assert!(o.status.success());
        o.stdout
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(os(s)))
            .collect()
    }

    fn real_find(dir: &Path) -> (Vec<u8>, String) {
        let o = Command::new("/usr/bin/bash")
            .arg("-c")
            .arg("cd \"$1\" || exit 1\nfind \"$PWD\" -name '*.pkg.tar*.sig' -prune -o \\( -name '*.pkg.tar*' -printf '%A@ %T@ %p\\n' \\)")
            .arg("bash")
            .arg(dir)
            .output()
            .unwrap();
        (o.stdout, String::from_utf8_lossy(&o.stderr).into_owned())
    }

    fn our_find_text(dir: &Path) -> Vec<u8> {
        let mut s = Vec::new();
        for f in list_find(dir) {
            s.extend_from_slice(find_time_text(f.atime).as_bytes());
            s.push(b' ');
            s.extend_from_slice(find_time_text(f.mtime).as_bytes());
            s.push(b' ');
            s.extend_from_slice(f.path.as_os_str().as_bytes());
            s.push(b'\n');
        }
        s
    }

    #[test]
    fn glob_rule() {
        assert!(glob_matches(b"x.pkg.tar"));
        assert!(glob_matches(b"x.pkg.tar.zst"));
        assert!(glob_matches(b"x.pkg.tar.zst.part"));
        assert!(glob_matches(b"x.pkg.tarsig"));
        assert!(glob_matches(b"a.pkg.tar.sig.x"));
        assert!(!glob_matches(b"x.pkg.tar.zst.sig"));
        assert!(!glob_matches(b"x.pkg.tar.sig"));
        assert!(!glob_matches(b".x.pkg.tar"));
        assert!(!glob_matches(b"x.PKG.TAR"));
        assert!(find_prunes(b"x.pkg.tar.sig"));
        assert!(find_prunes(b"x.pkg.tar.zst.sig"));
        assert!(!find_prunes(b"x.pkg.tarsig"));
        assert!(!find_prunes(b"x.pkg.tar"));
        assert!(!find_prunes(b".sig"));
        assert!(find_matches(b".pkg.tar"));
    }

    #[test]
    fn time_text() {
        assert_eq!(find_time_text((1795746737, 947979688)), "1795746737.9479796880");
        assert_eq!(find_time_text((-1, 500000000)), "-1.5000000000");
        assert_eq!(find_time_text((0, 0)), "0.0000000000");
    }

    #[test]
    fn glob_matches_bash() {
        init_collation();
        let root = tmpdir("glob");
        weird_tree(&root);
        assert_eq!(list_glob(&root), bash_glob(&root));
        // a symlinked cache dir is followed by the glob
        let link = root.join("linkdir");
        assert_eq!(list_glob(&link), bash_glob(&link));
        // empty and nonexistent dirs
        let empty = root.join("empty");
        fs::create_dir(&empty).unwrap();
        assert_eq!(list_glob(&empty), Vec::<PathBuf>::new());
        assert_eq!(list_glob(&root.join("nope")), Vec::<PathBuf>::new());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn glob_root_dir_double_slash() {
        // "$PWD"/x with PWD=/ gives //x; check textual join without touching '/'.
        let p = Path::new("/");
        let mut v = p.as_os_str().as_bytes().to_vec();
        v.push(b'/');
        assert_eq!(v, b"//");
    }

    #[test]
    fn find_matches_real_find() {
        let root = tmpdir("find");
        weird_tree(&root);
        let (out, err) = real_find(&root);
        assert_eq!(String::from_utf8_lossy(&our_find_text(&root)), String::from_utf8_lossy(&out));
        assert_eq!(err, "");
        // the start dir itself may match; a symlinked start dir is not descended
        let named = root.join("cache.pkg.tar.d");
        fs::create_dir(&named).unwrap();
        touch(&named.join("k-1-1-any.pkg.tar.zst"));
        assert_eq!(our_find_text(&named), real_find(&named).0);
        let lnk = root.join("lnk.pkg.tar.d");
        symlink(&named, &lnk).unwrap();
        assert_eq!(our_find_text(&lnk), real_find(&lnk).0);
        assert_eq!(list_find(&lnk).len(), 1);
        let pruned = root.join("pruned.pkg.tar.sig");
        assert_eq!(our_find_text(&pruned), real_find(&pruned).0);
        assert!(list_find(&pruned).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn find_unreadable_subdir_continues() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything
        }
        let root = tmpdir("perm");
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        touch(&locked.join("l-1-1-any.pkg.tar.zst"));
        touch(&root.join("ok-1-1-any.pkg.tar.zst"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let (out, _err) = real_find(&root);
        assert_eq!(our_find_text(&root), out);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn time_values_parse_like_awk() {
        let f = Found { path: PathBuf::new(), atime: (1795746737, 999999999), mtime: (-1, 500000000) };
        // 1795746737.9999999990 rounds to the next integer as a double, like strtod
        assert_eq!(f.atime_f64(), 1795746738.0);
        assert_eq!(f.mtime_f64(), -1.5);
    }
}
