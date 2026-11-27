//! Package filename parsing and `pacsort --files` ordering (owner: agent A).
//!
//! # `parse` (paccache's awk `parse_filename`)
//!
//! `basename` is `sub(".*/", "")` (strip through the last `/`), then
//! `count = split(basename, parts, "-")`:
//!
//! * name = `parts[1]`, plus `"-" parts[i]` for `i = 2..=count-3`. So with
//!   fewer than 5 parts the name is just `parts[1]`, and with a single part it
//!   is the whole basename (`foo.pkg.tar.zst` -> name `foo.pkg.tar.zst`).
//! * arch = `substr(parts[count], 1, index(parts[count], ".") - 1)`: the last
//!   part up to its first `.`; empty if it has no `.` (awk `substr(s, 1, -1)`)
//!   or starts with `.`. An empty basename gives `count = 0`, name `""`, arch `""`.
//!
//! Work is bytewise; non-UTF-8 bytes are replaced lossily in `name`/`arch`.
//!
//! # `pacsort --files` (pacman-contrib 1.13.1, `src/pacsort.c`)
//!
//! For each input line (`data`, here the full path):
//!
//! * It is a "file" iff `fnmatch("*-*.pkg.tar*", data, 0)` matches the *whole
//!   path* (no `FNM_PATHNAME`: a `-` in a directory counts).
//! * Files: `pkgname` starts after the last `/`. `pkgver_end` = last `-` of the
//!   basename. Walk back from `pkgver_end - 1` to the previous `-` (pkgrel),
//!   step over it, walk back to the previous `-` (pkgver), step forward:
//!   `pkgver` = `[that, pkgver_end)` (i.e. `[epoch:]pkgver-pkgrel`),
//!   `pkgname_len = pkgver - pkgname - 1`. With fewer than 3 dashes the walks
//!   stop at the basename start and `pkgname_len` underflows (`size_t`).
//! * Two files compare by `memcmp(name1, name2, min(len1, len2))`, then by
//!   `(int)(len1 - len2)` (size_t difference truncated to int), then by
//!   `alpm_pkg_vercmp(pkgver1, pkgver2)`. The arch and extension (`.sig`,
//!   `.zst`, `.xz`, ...) are ignored: those are ties.
//! * Otherwise (either side is not a file): `alpm_pkg_vercmp(data1, data2)`.
//! * The array is sorted with glibc `qsort`, which (glibc >= 2.39, current
//!   2.44) is a *stable* top-down merge sort (left half `n/2`, take left on
//!   `cmp <= 0`) unless `malloc` fails. Ties therefore keep input order.
//!   Because the comparator is not guaranteed to be a total order, use
//!   [`pacsort_files_sort`], which replicates that exact merge sort, rather
//!   than `slice::sort_by` (which may order differently or even panic).
//! * Crash: a "file" whose basename has no `-` (the `-` is in a directory,
//!   e.g. `/my-cache/foo.pkg.tar.zst`) makes `strrchr` return NULL and pacsort
//!   segfaults while reading input, printing nothing (so the original finds no
//!   candidates in that cachedir). DIVERGENCE (decided by the coordinator): we
//!   do not reproduce the crash; such a line is compared as a non-file (plain
//!   `alpm_pkg_vercmp` of the whole path) and the list is sorted normally.
//!   [`pacsort_crashes`] remains as a diagnostic.
//! * Underflowed names (`foo-x86_64.pkg.tar.zst`, `a-b.pkg.tar`, ...):
//!   `memcmp` with a huge length reads the basename up to its NUL; we model the
//!   basename followed by a NUL and stop when both sides hit NUL together.
//!   Two such entries with *equal basenames* (e.g. the same file in two
//!   cachedirs) make glibc read past both NULs: undefined, not reproducible.
//! * `pacsort --files --key 3 --separator ' '` (paccache's `--min-atime` /
//!   `--min-mtime` path, lines `atime mtime path`) orders the same way on the
//!   paths, except that `nth_column` is also applied to the extracted pkgver,
//!   which only matters if the version contains spaces (not modelled).
use std::cmp::Ordering;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::vercmp::vercmp_bytes;

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

/// Bytes after the last '/', like awk `sub(".*/", "", str)` / `strrchr`.
fn basename(data: &[u8]) -> &[u8] {
    match data.iter().rposition(|&c| c == b'/') {
        Some(i) => &data[i + 1..],
        None => data,
    }
}

/// Parse like the awk `parse_filename` (no validation: mirror awk on odd names).
pub fn parse(path: &Path) -> PkgFile {
    let base = basename(path.as_os_str().as_bytes());
    let (name, arch): (&[u8], &[u8]) = if base.is_empty() {
        (b"", b"")
    } else {
        let parts: Vec<&[u8]> = base.split(|&c| c == b'-').collect();
        let count = parts.len();
        // parts[1] .. parts[count-3] (1-based), at least parts[1].
        let last_name_part = count.saturating_sub(3).max(1);
        let name_end: usize = parts[..last_name_part]
            .iter()
            .map(|p| p.len())
            .sum::<usize>()
            + (last_name_part - 1);
        let last = parts[count - 1];
        let arch = match last.iter().position(|&c| c == b'.') {
            Some(i) => &last[..i],
            None => b"",
        };
        (&base[..name_end], arch)
    };
    PkgFile {
        path: path.to_path_buf(),
        name: String::from_utf8_lossy(name).into_owned(),
        arch: String::from_utf8_lossy(arch).into_owned(),
    }
}

/// What pacsort's `input_new` extracts from one line.
struct Input<'a> {
    data: &'a [u8],
    file: Option<FileMeta<'a>>,
}

struct FileMeta<'a> {
    /// Basename (pkgname pointer up to the NUL).
    base: &'a [u8],
    /// `pkgname_len`, as a signed value; negative means a `size_t` underflow.
    name_len: i64,
    /// The `[pkgver, pkgver_end)` slice.
    ver: &'a [u8],
}

/// `fnmatch("*-*.pkg.tar*", data, 0)`.
fn is_file(data: &[u8]) -> bool {
    const PAT: &[u8] = b".pkg.tar";
    let Some(dash) = data.iter().position(|&c| c == b'-') else {
        return false;
    };
    data.len() >= PAT.len()
        && (dash + 1..=data.len() - PAT.len()).any(|j| &data[j..j + PAT.len()] == PAT)
}

/// True if pacsort 1.13.1 segfaults on this input line in `--files` mode.
/// Diagnostic only: [`pacsort_files_sort`] does not reproduce the crash.
pub fn pacsort_crashes(path: &Path) -> bool {
    let data = path.as_os_str().as_bytes();
    is_file(data) && !basename(data).contains(&b'-')
}

impl<'a> Input<'a> {
    fn new(data: &'a [u8]) -> Self {
        if !is_file(data) {
            return Input { data, file: None };
        }
        let base_start = data.len() - basename(data).len();
        let base = &data[base_start..];
        // pacsort would segfault here (see `pacsort_crashes`); we treat the line
        // as a non-file instead.
        let Some(end) = base.iter().rposition(|&c| c == b'-') else {
            return Input { data, file: None };
        };
        let end = end as i64;
        let at = |i: i64| base[i as usize];
        let mut p = end - 1;
        while p > 0 && at(p) != b'-' {
            p -= 1;
        }
        p -= 1;
        while p > 0 && at(p) != b'-' {
            p -= 1;
        }
        p += 1;
        let ver: &[u8] = if p >= 0 {
            &base[p as usize..end as usize]
        } else if base_start > 0 {
            // p == -1 (basename starts with its only '-'): reads the '/' before.
            &data[base_start - 1..base_start]
        } else {
            // Would read the byte before the heap buffer; take it as empty.
            b""
        };
        Input {
            data,
            file: Some(FileMeta {
                base,
                name_len: p - 1,
                ver,
            }),
        }
    }
}

/// `memcmp(base1, base2, min(len1, len2))` with `size_t` lengths, modelling the
/// bytes past each basename as a NUL terminator.
fn name_memcmp(a: &FileMeta, b: &FileMeta) -> Ordering {
    let n = (a.name_len as u64).min(b.name_len as u64);
    let mut i: u64 = 0;
    while i < n {
        let ca = a.base.get(i as usize).copied().unwrap_or(0);
        let cb = b.base.get(i as usize).copied().unwrap_or(0);
        if ca != cb {
            return ca.cmp(&cb);
        }
        if ca == 0 {
            // Both past their NUL: undefined in C, treat as equal.
            return Ordering::Equal;
        }
        i += 1;
    }
    Ordering::Equal
}

fn compare_inputs(a: &Input, b: &Input) -> Ordering {
    match (&a.file, &b.file) {
        (Some(fa), Some(fb)) => name_memcmp(fa, fb)
            .then_with(|| {
                if fa.name_len != fb.name_len {
                    // (int)(len1 - len2) in size_t arithmetic.
                    let d = (fa.name_len as u64).wrapping_sub(fb.name_len as u64) as i32;
                    d.cmp(&0)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| vercmp_bytes(fa.ver, fb.ver)),
        _ => vercmp_bytes(a.data, b.data),
    }
}

/// Ordering identical to `pacsort --files` for two package paths.
pub fn pacsort_files_cmp(a: &Path, b: &Path) -> Ordering {
    compare_inputs(
        &Input::new(a.as_os_str().as_bytes()),
        &Input::new(b.as_os_str().as_bytes()),
    )
}

/// Sort `items` exactly like `pacsort --files` would print their paths,
/// including glibc's stable merge sort (but not its crash, see module docs).
pub fn pacsort_files_sort<T, F>(items: &mut Vec<T>, path: F)
where
    F: Fn(&T) -> &Path,
{
    let order = {
        let inputs: Vec<Input> = items
            .iter()
            .map(|it| Input::new(path(it).as_os_str().as_bytes()))
            .collect();
        glibc_sort_order(inputs.len(), &|x, y| compare_inputs(&inputs[x], &inputs[y]))
    };
    apply_order(items, order);
}

/// Sort `items` with glibc's `qsort` merge sort and an arbitrary comparator
/// (which need not be a total order; it never panics).
pub fn glibc_sort_by<T, C>(items: &mut Vec<T>, cmp: C)
where
    C: Fn(&T, &T) -> Ordering,
{
    let order = glibc_sort_order(items.len(), &|x, y| cmp(&items[x], &items[y]));
    apply_order(items, order);
}

fn glibc_sort_order(n: usize, cmp: &dyn Fn(usize, usize) -> Ordering) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut tmp = vec![0usize; n];
    msort(&mut idx, &mut tmp, cmp);
    idx
}

fn apply_order<T>(items: &mut Vec<T>, order: Vec<usize>) {
    let mut slots: Vec<Option<T>> = items.drain(..).map(Some).collect();
    items.extend(
        order
            .into_iter()
            .map(|i| slots[i].take().expect("permutation")),
    );
}

/// glibc `msort_with_tmp`: top-down, `n1 = n / 2`, take left when `cmp <= 0`.
fn msort(b: &mut [usize], tmp: &mut [usize], cmp: &dyn Fn(usize, usize) -> Ordering) {
    let n = b.len();
    if n <= 1 {
        return;
    }
    let n1 = n / 2;
    {
        let (l, r) = b.split_at_mut(n1);
        msort(l, tmp, cmp);
        msort(r, tmp, cmp);
    }
    let (mut i, mut j, mut k) = (0, n1, 0);
    while i < n1 && j < n {
        if cmp(b[i], b[j]) != Ordering::Greater {
            tmp[k] = b[i];
            i += 1;
        } else {
            tmp[k] = b[j];
            j += 1;
        }
        k += 1;
    }
    // Remaining left elements go after; remaining right ones are in place.
    while i < n1 {
        tmp[k] = b[i];
        i += 1;
        k += 1;
    }
    b[..k].copy_from_slice(&tmp[..k]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercmp::tests::{random_version, Rng};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn na(s: &str) -> (String, String) {
        let f = parse(Path::new(s));
        (f.name, f.arch)
    }

    #[test]
    fn parse_regular_and_awk_quirks() {
        let cases: &[(&str, &str, &str)] = &[
            ("/c/foo-1.0-1-x86_64.pkg.tar.zst", "foo", "x86_64"),
            (
                "/c/lib32-foo-bar-2:1.0.r3.gabc-2-any.pkg.tar.xz",
                "lib32-foo-bar",
                "any",
            ),
            ("foo-1.0-1-x86_64.pkg.tar.zst.sig", "foo", "x86_64"),
            // < 5 parts: name is just parts[1].
            ("/c/a-b-c-d.pkg.tar", "a", "d"),
            ("/c/a-b-c.pkg.tar", "a", "c"),
            ("/c/foo-x86_64.pkg.tar.zst", "foo", "x86_64"),
            // single part: name = whole basename, arch = up to first '.'.
            ("/c/foo.pkg.tar.zst", "foo.pkg.tar.zst", "foo"),
            // last part without '.': substr(s, 1, -1) = "".
            ("/c/foo-1-1-x86_64", "foo", ""),
            // last part starting with '.': empty arch.
            ("/c/foo-1-1-.pkg.tar", "foo", ""),
            // empty dash parts.
            ("/c/--1-1-any.pkg.tar", "-", "any"),
            ("/c/-1-1-any.pkg.tar", "", "any"),
            ("/c/foo-1-1-", "foo", ""),
            // dashes in directories are ignored (basename only).
            ("/my-cache/dir-x/foo-1-1-any.pkg.tar", "foo", "any"),
            ("", "", ""),
            ("/c/", "", ""),
        ];
        for &(path, name, arch) in cases {
            assert_eq!(na(path), (name.into(), arch.into()), "{path:?}");
        }
    }

    /// Random basename, mostly realistic, sometimes malformed.
    fn random_name(rng: &mut Rng) -> String {
        const NAMES: &[&str] = &[
            "foo",
            "lib32-foo",
            "python-foo-bar",
            "a",
            "x-",
            "gcc",
            "gcc-libs",
            "linux",
            "linux-lts",
            "r",
            "zlib-ng",
            "foo.bar",
            "foo+",
            "foo_bar",
            "Foo",
            "",
            "-",
        ];
        const ARCH: &[&str] = &[
            "x86_64",
            "any",
            "i686",
            "aarch64",
            "x86_64_v3",
            "",
            "pentium4",
        ];
        const EXT: &[&str] = &[
            ".pkg.tar.zst",
            ".pkg.tar.zst",
            ".pkg.tar.xz",
            ".pkg.tar.zst.sig",
            ".pkg.tar.xz.sig",
            ".pkg.tar",
            ".pkg.tar.gz",
            ".pkg.tar.zst.part",
            "",
            ".tar",
            ".pkg.tar.zst.sig.sig",
        ];
        let mut s = String::from(rng.pick(NAMES));
        match rng.below(12) {
            // Missing components.
            0 => s.push_str(&format!("-{}{}", rng.pick(ARCH), rng.pick(EXT))),
            1 => s.push_str(&format!(
                "-{}-{}{}",
                random_version(rng),
                rng.pick(ARCH),
                rng.pick(EXT)
            )),
            2 => s.push_str(rng.pick(EXT)),
            _ => {
                let mut v = random_version(rng);
                if !v.contains('-') {
                    v.push_str(&format!("-{}", rng.below(4)));
                }
                s.push_str(&format!("-{v}-{}{}", rng.pick(ARCH), rng.pick(EXT)));
            }
        }
        s
    }

    fn random_path(rng: &mut Rng, dirs: &[&str]) -> String {
        format!("{}{}", rng.pick(dirs), random_name(rng))
    }

    #[test]
    fn parse_differential_vs_gawk() {
        let mut rng = Rng(42);
        let dirs = ["/var/cache/pacman/pkg/", "/a-b/c/", "", "/"];
        let paths: Vec<String> = (0..50_000).map(|_| random_path(&mut rng, &dirs)).collect();
        // The awk body of paccache's parse_filename (atime/mtime part removed).
        let prog = r#"
            function basename(str) { sub(".*/", "", str); return str; }
            {
                count = split(basename($0), parts, "-")
                i = 1
                pkgname = parts[i++]
                while (i <= count - 3) { pkgname = pkgname "-" parts[i++] }
                arch = substr(parts[count], 1, index(parts[count], ".") - 1)
                print pkgname "\t" arch
                delete parts
            }"#;
        let mut child = Command::new("gawk")
            .arg(prog)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("gawk");
        let mut stdin = child.stdin.take().unwrap();
        let input = paths.join("\n") + "\n";
        let w = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
        let out = child.wait_with_output().unwrap();
        w.join().unwrap();
        let out = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), paths.len());
        let mut bad = 0;
        for (path, line) in paths.iter().zip(lines) {
            let (name, arch) = na(path);
            if format!("{name}\t{arch}") != line {
                bad += 1;
                eprintln!("MISMATCH {path:?}: ours {name:?} {arch:?}, awk {line:?}");
            }
        }
        assert_eq!(bad, 0);
    }

    /// Run `pacsort --files` on the given lines.
    fn pacsort(lines: &[String]) -> (Vec<String>, Option<i32>) {
        let mut child = Command::new("pacsort")
            .arg("--files")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("pacsort");
        let mut stdin = child.stdin.take().unwrap();
        let input = lines.join("\n") + "\n";
        let w = std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let out = child.wait_with_output().unwrap();
        w.join().unwrap();
        let s = String::from_utf8(out.stdout).unwrap();
        (s.lines().map(String::from).collect(), out.status.code())
    }

    fn ours(lines: &[String]) -> Vec<String> {
        let mut v: Vec<PathBuf> = lines.iter().map(PathBuf::from).collect();
        pacsort_files_sort(&mut v, |p| p.as_path());
        v.into_iter()
            .map(|p| p.into_os_string().into_string().unwrap())
            .collect()
    }

    #[test]
    fn cmp_basics() {
        use Ordering::*;
        let c = |a: &str, b: &str| pacsort_files_cmp(&p(a), &p(b));
        assert_eq!(
            c("/c/foo-1-1-any.pkg.tar.zst", "/c/foo-2-1-any.pkg.tar.zst"),
            Less
        );
        assert_eq!(
            c("/c/foo-1:1-1-any.pkg.tar.zst", "/c/foo-2-1-any.pkg.tar.zst"),
            Greater
        );
        // Name before version; shorter prefix name first.
        assert_eq!(
            c(
                "/c/foo-9-1-any.pkg.tar.zst",
                "/c/foo-bar-1-1-any.pkg.tar.zst"
            ),
            Less
        );
        assert_eq!(
            c("/c/foo-9-1-any.pkg.tar.zst", "/c/fop-1-1-any.pkg.tar.zst"),
            Less
        );
        // Arch, extension, .sig and directory are ties.
        assert_eq!(
            c(
                "/a/foo-1-1-any.pkg.tar.zst.sig",
                "/b/foo-1-1-x86_64.pkg.tar.xz"
            ),
            Equal
        );
        // Underflowed name (single dash) sorts before the same prefix.
        assert_eq!(
            c("/a/foo-any.pkg.tar.zst", "/a/fo-1-1-any.pkg.tar.zst"),
            Less
        );
        // Not a "file" (no '-' anywhere): vercmp of full paths.
        assert_eq!(c("/c/foo.pkg.tar", "/c/bar-1-1-any.pkg.tar"), Greater);
        assert!(pacsort_crashes(&p("/a-b/foo.pkg.tar.zst")));
        assert!(!pacsort_crashes(&p("/ab/foo.pkg.tar.zst")));
    }

    #[test]
    fn sort_is_stable_on_ties() {
        let lines: Vec<String> = [
            "/c/foo-1-1-x86_64.pkg.tar.zst",
            "/c/foo-1-1-any.pkg.tar.xz",
            "/c/foo-1-1-x86_64.pkg.tar.xz",
            "/c/foo-0-1-any.pkg.tar.xz",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            ours(&lines),
            [&lines[3], &lines[0], &lines[1], &lines[2]].map(|s| s.clone())
        );
    }

    /// Two entries (even identical lines) whose names underflowed and whose
    /// basenames are equal make pacsort's `memcmp` read past both NULs into
    /// heap memory: the result is undefined. Drop all but the first of those.
    fn drop_undefined_pairs(lines: Vec<String>) -> (Vec<String>, usize) {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let before = lines.len();
        let kept: Vec<String> = lines
            .into_iter()
            .filter(|l| match Input::new(l.as_bytes()).file {
                Some(f) if f.name_len < 0 => {
                    if seen.iter().any(|b| b == f.base) {
                        false
                    } else {
                        seen.push(f.base.to_vec());
                        true
                    }
                }
                _ => true,
            })
            .collect();
        let dropped = before - kept.len();
        (kept, dropped)
    }

    #[test]
    #[ignore = "spawns pacsort a few thousand times"]
    fn differential_vs_pacsort_random() {
        let mut rng = Rng(7);
        let plain = [
            "/var/cache/pacman/pkg/",
            "/var/cache/pacman/pkg/",
            "/c/",
            "",
            "/a/b/",
        ];
        // A '-' in a directory makes dash-less basenames crash pacsort.
        let dashed = ["/var/cache/pacman/pkg/", "/x-y/", "/a/b-c/d/"];
        let mut all: Vec<Vec<String>> = Vec::new();
        let mut skipped = 0;
        for round in 0..4000 {
            let dirs: &[&str] = if round % 4 == 0 { &dashed } else { &plain };
            let n = 1 + rng.below(if round % 10 == 0 { 400 } else { 40 }) as usize;
            // Small path pool so groups, duplicates and ties are frequent.
            let pool: Vec<String> = (0..1 + n / 3)
                .map(|_| random_path(&mut rng, dirs))
                .collect();
            let lines: Vec<String> = (0..n)
                .map(|_| {
                    if rng.below(3) == 0 {
                        random_path(&mut rng, dirs)
                    } else {
                        pool[rng.below(pool.len() as u64) as usize].clone()
                    }
                })
                .collect();
            let (lines, dropped) = drop_undefined_pairs(lines);
            skipped += dropped;
            all.push(lines);
        }
        let lists = all.len();
        let items: usize = all.iter().map(Vec::len).sum();
        let chunk = lists.div_ceil(16);
        let handles: Vec<_> = all
            .chunks(chunk)
            .map(|c| c.to_vec())
            .map(|c| {
                std::thread::spawn(move || {
                    let mut crashes = 0;
                    for lines in c {
                        let (want, code) = pacsort(&lines);
                        let got = ours(&lines);
                        let predicted = lines.iter().any(|l| pacsort_crashes(Path::new(l)));
                        if code != Some(0) {
                            crashes += 1;
                            assert!(predicted && want.is_empty(), "crash mismatch: {lines:?}");
                            // DIVERGENCE: we sort normally instead of crashing.
                            assert_eq!(got.len(), lines.len());
                            continue;
                        }
                        assert!(!predicted, "predicted crash did not happen: {lines:?}");
                        assert_eq!(got, want, "order mismatch for input {lines:?}");
                    }
                    crashes
                })
            })
            .collect();
        let crashes: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        eprintln!(
            "pacsort differential: {lists} lists, {items} paths, {crashes} crash lists, \
             {skipped} paths dropped (undefined memcmp), 0 mismatches"
        );
    }

    #[test]
    #[ignore = "reads /var/cache/pacman/pkg"]
    fn differential_vs_pacsort_real_cache() {
        // Exactly `ls /var/cache/pacman/pkg | sed 's|^|/var/cache/pacman/pkg/|'`.
        let ls = Command::new("ls")
            .arg("/var/cache/pacman/pkg")
            .output()
            .unwrap();
        let ls_order: Vec<String> = String::from_utf8(ls.stdout)
            .unwrap()
            .lines()
            .map(|l| format!("/var/cache/pacman/pkg/{l}"))
            .collect();
        let mut byte_order = ls_order.clone();
        byte_order.sort();
        let mut reversed = byte_order.clone();
        reversed.reverse();
        // Ties (pkg vs .sig, .xz vs .zst) keep input order, so try several.
        for (what, lines) in [
            ("ls", &ls_order),
            ("bytes", &byte_order),
            ("reversed", &reversed),
        ] {
            let (want, code) = pacsort(lines);
            assert_eq!(code, Some(0));
            let got = ours(lines);
            assert_eq!(got.len(), want.len());
            let first_diff = got.iter().zip(&want).position(|(a, b)| a != b);
            assert_eq!(
                first_diff, None,
                "{what}: first difference at {first_diff:?}"
            );
            eprintln!(
                "real cache ({what} input order): {} paths, identical order",
                lines.len()
            );
        }
    }
}
