//! Differential compatibility battery: `/usr/bin/paccache` (pacman-contrib
//! 1.13.1) versus our binary.
//!
//! Every scenario is its own `#[test]` (named `<scenario>`), so `cargo test`
//! reports exactly which ones differ. For each scenario both programs run on
//! fresh, identical fake caches (one materialization per run) with `LANG=C`,
//! a controlled stdin and the same working directory layout; then exit code,
//! stdout, stderr and the final file tree are compared and a line diff is
//! printed for every mismatch.
//!
//! Environment knobs:
//! - `COMPAT_ORIGINAL_ONLY=1`: run the original twice and compare it with
//!   itself (checks the battery and the original's determinism without our
//!   binary).
//! - `COMPAT_SHOW=1`: print the (normalized) outcome of the reference run.
//! - `COMPAT_ORIGINAL=/path/to/paccache`: alternative reference script.
//!
//! Normalization is limited to the unavoidable: the per-run temporary root is
//! replaced by `<ROOT>`; our binary is invoked through a symlink called
//! `paccache` so `argv[0]` matches. With `-r`/`-m` plus `-v` the order of the
//! `removed '...'`/`renamed '...'` lines comes from awk's hash iteration in
//! the original, so those lines are compared as a multiset
//! ([`Order::Unordered`]). `-V/--version` only compares exit code and stderr.

mod support;

use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use support::CacheSpec;

// ---------------------------------------------------------------------------
// Scenario description
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Fixture {
    Standard(u64),
    Random(u64, usize),
    Installed(u64),
    Ages,
    NoPackages,
    Empty,
}

impl Fixture {
    fn spec(self) -> CacheSpec {
        match self {
            Fixture::Standard(s) => support::standard(s),
            Fixture::Random(s, n) => support::random(s, n),
            Fixture::Installed(s) => support::installed_mix(s),
            Fixture::Ages => support::ages(),
            Fixture::NoPackages => support::no_packages(),
            Fixture::Empty => CacheSpec::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    /// stdout must match byte for byte.
    Exact,
    /// Lines not starting with `==>` (rm/mv -v output, rm/mv errors) may come
    /// in any order, in stdout and in stderr.
    Unordered,
}

#[derive(Clone, Debug)]
struct Scenario {
    fixture: Fixture,
    /// Also create `{cache2}` from [`support::secondary`].
    second: bool,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
    order: Order,
    /// Extra empty directories to create under the root (e.g. odd movedirs).
    dirs: Vec<String>,
    /// Directories (created under the root) made unreadable (mode 000) for the
    /// run, restored before the tree is compared.
    locked: Vec<String>,
    /// Run with `LC_ALL=C.UTF-8` instead of `C` (locale-dependent quoting).
    utf8: bool,
    /// `-V`: output may differ; only exit code and stderr are compared.
    version_only: bool,
    /// Fill `defcache/` (what the `pacman-conf` shim reports) with
    /// [`support::secondary`]; otherwise it exists but is empty.
    defcache: bool,
}

/// Scenario with exactly these arguments.
fn sc(args: &[&str]) -> Scenario {
    Scenario {
        fixture: Fixture::Standard(1),
        second: false,
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        stdin: None,
        order: Order::Exact,
        dirs: Vec::new(),
        locked: Vec::new(),
        utf8: false,
        version_only: false,
        defcache: false,
    }
}

/// Scenario with `-c {cache}` followed by these arguments (for cases where
/// option processing stops early, see `k_*` scenarios).
fn ck(args: &[&str]) -> Scenario {
    let mut s = sc(&["-c", "{cache}"]);
    s.args.extend(args.iter().map(|a| (*a).to_owned()));
    s
}

/// Scenario with these arguments followed by `-c {cache}`.
fn c(args: &[&str]) -> Scenario {
    let mut s = sc(args);
    s.args.push("-c".into());
    s.args.push("{cache}".into());
    s
}

impl Scenario {
    fn fx(mut self, f: Fixture) -> Self {
        self.fixture = f;
        self
    }
    fn second(mut self) -> Self {
        self.second = true;
        self
    }
    fn stdin(mut self, s: &str) -> Self {
        self.stdin = Some(s.as_bytes().to_vec());
        self
    }
    fn unordered(mut self) -> Self {
        self.order = Order::Unordered;
        self
    }
    fn dirs(mut self, d: &[&str]) -> Self {
        self.dirs = d.iter().map(|s| (*s).to_owned()).collect();
        self
    }
    fn locked(mut self, d: &[&str]) -> Self {
        self.locked = d.iter().map(|s| (*s).to_owned()).collect();
        self
    }
    fn utf8(mut self) -> Self {
        self.utf8 = true;
        self
    }
    fn version_only(mut self) -> Self {
        self.version_only = true;
        self
    }
    fn defcache(mut self) -> Self {
        self.defcache = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Sorted listing of the root (`path` or `path/` for dirs, `path size`
    /// for files), excluding the `bin/` helper directory.
    tree: Vec<String>,
}

fn original_path() -> PathBuf {
    env::var_os("COMPAT_ORIGINAL").map_or_else(|| PathBuf::from("/usr/bin/paccache"), PathBuf::from)
}

fn ours_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_paccache-rs"))
}

fn flag(name: &str) -> bool {
    env::var(name).is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Build the run root: `cache/`, optional `cache2/`, `defcache/`, `dest/`, a
/// regular file `afile`, extra dirs, `bin/paccache` -> `prog` and the
/// `bin/pacman-conf` shim.
///
/// The shim (first in `PATH`) answers `pacman-conf CacheDir` with
/// `<ROOT>/defcache/`, so no scenario can ever reach `/var/cache/pacman`, even
/// those where the original falls back to the default cachedir (e.g. when an
/// arithmetic error in `-k` aborts its option loop before `-c` is seen). Any
/// other `pacman-conf` invocation is forwarded to the real one.
fn prepare(s: &Scenario, prog: &Path) -> (tempfile::TempDir, PathBuf) {
    // No '-' in the root path: pacsort 1.13.1 segfaults on a basename without
    // '-' (our `weird.pkg.tar.zst`) when the directory part contains a '-'.
    let tmp = tempfile::Builder::new()
        .prefix("paccache_compat.")
        .tempdir()
        .expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    assert!(
        !root.to_string_lossy().contains('-'),
        "temporary root {} contains '-', which crashes pacsort; set TMPDIR to a path without '-'",
        root.display()
    );
    let cache = root.join("cache");
    fs::create_dir(&cache).unwrap();
    s.fixture
        .spec()
        .materialize(&cache)
        .expect("materialize cache");
    if s.second {
        let c2 = root.join("cache2");
        fs::create_dir(&c2).unwrap();
        support::secondary(1)
            .materialize(&c2)
            .expect("materialize cache2");
    }
    let defcache = root.join("defcache");
    fs::create_dir(&defcache).unwrap();
    if s.defcache {
        support::secondary(2)
            .materialize(&defcache)
            .expect("materialize defcache");
    }
    fs::create_dir(root.join("dest")).unwrap();
    fs::write(root.join("afile"), b"x").unwrap();
    for d in &s.dirs {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    for d in &s.locked {
        let p = root.join(d);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("hidden-1-1-any.pkg.tar.zst"), b"x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    }
    fs::create_dir(root.join("bin")).unwrap();
    let link = root.join("bin").join("paccache");
    symlink(prog, &link).unwrap();
    let shim = root.join("bin").join("pacman-conf");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nif [ \"$#\" = 1 ] && [ \"$1\" = CacheDir ]; then\n  printf '%s\\n' '{}/defcache/'\n  exit 0\nfi\nexec /usr/bin/pacman-conf \"$@\"\n",
            root.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    (tmp, root)
}

fn expand(arg: &str, root: &Path) -> String {
    let r = root.to_str().expect("utf-8 tempdir");
    arg.replace("{cache2}", &format!("{r}/cache2"))
        .replace("{cache}", &format!("{r}/cache"))
        .replace("{dest}", &format!("{r}/dest"))
        .replace("{root}", r)
        .replace("{mid}", &support::T_MID.to_string())
        .replace("{far}", "4200000000")
}

fn tree(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if rel == "bin" {
                continue;
            }
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                out.push(format!("{rel}/"));
                walk(&p, root, out);
            } else {
                out.push(format!("{rel} {}", md.len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn normalize(bytes: &[u8], root: &Path) -> Vec<u8> {
    let needle = root.as_os_str().as_encoded_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(needle) {
            out.extend_from_slice(b"<ROOT>");
            i += needle.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Sort the lines that do not start with `==>`, keeping `==>` and empty lines
/// in place.
fn sort_free_lines(bytes: &[u8]) -> Vec<u8> {
    let lines: Vec<&[u8]> = bytes.split_inclusive(|&b| b == b'\n').collect();
    let movable = |l: &[u8]| !l.starts_with(b"==>") && l != b"\n";
    let mut free: Vec<&[u8]> = lines.iter().copied().filter(|l| movable(l)).collect();
    free.sort();
    let mut free = free.into_iter();
    lines
        .iter()
        .flat_map(|l| {
            if movable(l) { free.next().unwrap() } else { l }
                .iter()
                .copied()
        })
        .collect()
}

fn run_one(s: &Scenario, prog: &Path) -> Outcome {
    let (_tmp, root) = prepare(s, prog);
    let args: Vec<String> = s.args.iter().map(|a| expand(a, &root)).collect();
    let mut cmd = Command::new(root.join("bin").join("paccache"));
    let path = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    cmd.args(&args)
        .current_dir(&root)
        .env_clear()
        .env("PATH", format!("{}:{path}", root.join("bin").display()))
        .env("HOME", env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .env("LANG", "C")
        .env("LC_ALL", if s.utf8 { "C.UTF-8" } else { "C" })
        .stdin(if s.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("cannot spawn {}: {e}", prog.display()));
    if let Some(input) = &s.stdin {
        let mut pipe = child.stdin.take().unwrap();
        // Ignore EPIPE: the program may legitimately not read stdin.
        let _ = pipe.write_all(input);
    }
    let out = child.wait_with_output().unwrap();
    for d in &s.locked {
        fs::set_permissions(root.join(d), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut stdout = normalize(&out.stdout, &root);
    let mut stderr = normalize(&out.stderr, &root);
    if s.order == Order::Unordered {
        stdout = sort_free_lines(&stdout);
        stderr = sort_free_lines(&stderr);
    }
    Outcome {
        code: out.status.code(),
        stdout,
        stderr,
        tree: tree(&root),
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\0', "\\0")
}

/// Minimal LCS line diff (`-` expected / `+` actual).
fn diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    if a.len() * b.len() > 4_000_000 {
        return format!("--- expected\n{expected}\n+++ actual\n{actual}\n");
    }
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, String::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push_str(&format!("  {}\n", a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push_str(&format!("+ {}\n", b[j]));
            j += 1;
        } else {
            out.push_str(&format!("- {}\n", a[i]));
            i += 1;
        }
    }
    if expected.ends_with('\n') != actual.ends_with('\n') {
        out.push_str("(trailing newline differs)\n");
    }
    out
}

fn compare(
    name: &str,
    s: &Scenario,
    exp: &Outcome,
    act: &Outcome,
    labels: (&str, &str),
) -> Vec<String> {
    let mut problems = Vec::new();
    if exp.code != act.code {
        problems.push(format!(
            "exit code: {} {:?} vs {} {:?}",
            labels.0, exp.code, labels.1, act.code
        ));
    }
    if !s.version_only && exp.stdout != act.stdout {
        problems.push(format!(
            "stdout differs:\n{}",
            diff(&show(&exp.stdout), &show(&act.stdout))
        ));
    }
    if exp.stderr != act.stderr {
        problems.push(format!(
            "stderr differs:\n{}",
            diff(&show(&exp.stderr), &show(&act.stderr))
        ));
    }
    if exp.tree != act.tree {
        problems.push(format!(
            "final tree differs:\n{}",
            diff(&exp.tree.join("\n"), &act.tree.join("\n"))
        ));
    }
    problems
        .into_iter()
        .map(|p| {
            format!(
                "[{name}] ({} = expected '-', {} = actual '+') {p}",
                labels.0, labels.1
            )
        })
        .collect()
}

fn run(name: &str, s: Scenario) {
    let original = original_path();
    assert!(
        original.exists(),
        "reference script {} not found (install pacman-contrib 1.13.1)",
        original.display()
    );
    let reference = run_one(&s, &original);
    if flag("COMPAT_SHOW") {
        eprintln!(
            "=== {name}: paccache {:?}\n--- exit {:?}\n--- stdout\n{}--- stderr\n{}--- tree\n{}\n",
            s.args,
            reference.code,
            show(&reference.stdout),
            show(&reference.stderr),
            reference.tree.join("\n")
        );
    }
    let (other, labels) = if flag("COMPAT_ORIGINAL_ONLY") {
        (run_one(&s, &original), ("original#1", "original#2"))
    } else {
        (run_one(&s, &ours_path()), ("original", "paccache-rs"))
    };
    let problems = compare(name, &s, &reference, &other, labels);
    assert!(
        problems.is_empty(),
        "scenario `{name}` differs; args: {:?}\n\n{}",
        s.args,
        problems.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

macro_rules! scenarios {
    ($($name:ident => $s:expr;)*) => {
        $(
            #[test]
            fn $name() {
                run(stringify!($name), $s);
            }
        )*
        const SCENARIOS: &[&str] = &[$(stringify!($name)),*];
    };
}

#[test]
fn battery_has_enough_scenarios() {
    assert!(SCENARIOS.len() >= 60, "only {} scenarios", SCENARIOS.len());
}

#[test]
fn fixtures_are_deterministic() {
    assert_eq!(support::standard(7).files, support::standard(7).files);
    assert_eq!(support::random(7, 50).files, support::random(7, 50).files);
    assert_ne!(support::random(7, 50).files, support::random(8, 50).files);
}

use Fixture::*;

scenarios! {
    // --- dry run and --keep ------------------------------------------------
    d_default => c(&["-d"]);
    d_long => c(&["--dryrun"]);
    d_k0 => c(&["-d", "-k", "0"]);
    d_k1 => c(&["-d", "-k", "1"]);
    d_k2 => c(&["-d", "-k", "2"]);
    d_k3 => c(&["-d", "-k", "3"]);
    d_k10 => c(&["-d", "-k", "10"]);
    d_k007 => c(&["-d", "-v", "-k", "007"]);
    d_k_attached => c(&["-dk1"]);
    d_keep_eq => c(&["--dryrun", "--keep=2", "-v"]);
    d_keep_prefixes => c(&["--dry", "--ke", "1"]);
    // `-k` values that the original evaluates as bash arithmetic are in the
    // `divergence_*` group below.
    k_negative => c(&["-d", "-k", "-1"]);
    k_negative_expr => c(&["-d", "-k", "1-2"]);
    // --- verbosity / null ------------------------------------------------
    d_v => c(&["-d", "-v"]);
    d_vv => c(&["-d", "-vv"]);
    d_vvv => c(&["-d", "-vvv"]);
    d_v_k0 => c(&["-d", "-v", "-k", "0"]);
    d_vvv_k0 => c(&["-d", "-vvv", "-k0"]);
    d_vvvv_k1 => c(&["-d", "-vvvv", "-k1"]);
    d_verbose_long => c(&["-d", "--verbose", "--verbose", "-k1"]);
    d_v_z => c(&["-d", "-v", "-z", "-k1"]);
    d_vv_null => c(&["-d", "-vv", "--null", "-k1"]);
    d_vvvz => c(&["-dvvvz", "-k1"]);
    d_z_only => c(&["-d", "-z"]);
    d_grouped_dvvk1 => c(&["-dvvk1"]);
    d_grouped_cachedir => sc(&["-dvc{cache}"]);
    d_s_noop => c(&["-ds", "-v"]);
    d_nocolor => c(&["-d", "--nocolor", "-v", "-k1"]);
    // --- arch --------------------------------------------------------------
    a_x86_64 => c(&["-d", "-v", "-a", "x86_64", "-k1"]);
    a_any_long_eq => c(&["-d", "-v", "--arch=any", "-k0"]);
    a_i686 => c(&["-d", "-vvv", "-a", "i686", "-k1"]);
    a_nomatch => c(&["-d", "-a", "armv7h"]);
    // --- whitelist ---------------------------------------------------------
    w_two => c(&["-d", "-v", "-k1", "linux", "vim"]);
    w_with_space => c(&["-d", "-v", "-k0", "my pkg"]);
    w_missing => c(&["-d", "-k0", "nonexistent"]);
    w_before_opts => sc(&["linux", "-d", "-k1", "-v", "-c", "{cache}"]);
    w_after_dashdash => sc(&["-d", "-v", "-k1", "-c", "{cache}", "--", "-k", "linux"]);
    w_single_dash => c(&["-d", "-v", "-k0", "-"]);
    w_and_ignore => c(&["-d", "-v", "-k0", "-i", "linux", "linux", "vim"]);
    // --- ignore ------------------------------------------------------------
    i_comma => c(&["-d", "-v", "-k1", "-i", "linux,vim"]);
    i_repeat => c(&["-d", "-v", "-k1", "-i", "linux", "--ignore", "vim,glibc"]);
    i_empty_elems => c(&["-d", "-v", "-k1", "-i", ",linux,,"]);
    i_empty => c(&["-d", "-v", "-k1", "-i", ""]);
    i_with_space => c(&["-d", "-v", "-k0", "-i", "my pkg,firefox"]);
    i_stdin => c(&["-d", "-v", "-k1", "-i", "-"]).stdin("linux\nvim\nmy pkg\n");
    i_stdin_empty => c(&["-d", "-v", "-k1", "-i", "-"]).stdin("");
    i_stdin_no_newline => c(&["-d", "-v", "-k1", "-i", "-"]).stdin("linux");
    i_stdin_commas_literal => c(&["-d", "-v", "-k1", "-i", "-"]).stdin("linux,vim\nglibc\n");
    i_stdin_null => c(&["-d", "-v", "-k1", "-i", "-"]);
    i_stdin_and_comma => c(&["-d", "-v", "-k1", "-i", "-", "-i", "glibc"]).stdin("vim\n");
    i_stdin_twice => c(&["-d", "-v", "-k1", "-i", "-", "-i", "-"]).stdin("linux\n");
    // --- uninstalled (real pacman -Qq) -------------------------------------
    u_d => c(&["-d", "-v", "-k0", "-u"]).fx(Installed(1));
    u_k1_whitelist => c(&["-d", "-v", "-k1", "-u", "not-installed-xyz", "bash"]).fx(Installed(2));
    u_r => c(&["-r", "-k1", "--uninstalled"]).fx(Installed(3));
    // --- min-atime / min-mtime ---------------------------------------------
    t_atime_abs => c(&["-d", "-v", "-k0", "--min-atime", "@{mid}"]).fx(Ages);
    t_mtime_abs => c(&["-d", "-v", "-k0", "--min-mtime", "@{mid}"]).fx(Ages);
    t_both => c(&["-d", "-v", "-k0", "--min-atime", "@{mid}", "--min-mtime", "@{mid}"]).fx(Ages);
    t_mtime_k1 => c(&["-d", "-v", "-k1", "--min-mtime", "@{mid}"]).fx(Ages);
    t_atime_default_keep => c(&["-d", "-v", "--min-atime", "@{mid}"]).fx(Ages);
    t_atime_long_eq => c(&["-d", "-vvv", "-k0", "--min-atime=@{mid}"]).fx(Ages);
    t_atime_prefix => c(&["-d", "-v", "-k0", "--min-a", "@{mid}"]).fx(Ages);
    t_relative_date => c(&["-d", "-vv", "-k0", "--min-mtime", "30 days ago"]);
    t_recursive_vv => c(&["-d", "-vv", "-k1", "--min-atime", "@{far}"]);
    t_recursive_vvv => c(&["-d", "-vvv", "-k0", "--min-mtime", "@{far}"]);
    t_recursive_z => c(&["-d", "-v", "-z", "-k2", "--min-mtime", "@{far}"]);
    t_epoch_zero_disables => c(&["-d", "-v", "-k0", "--min-atime", "@0"]);
    t_negative_epoch => c(&["-d", "-k0", "--min-mtime", "@-100"]);
    t_invalid_atime => c(&["-d", "--min-atime", "not a date"]);
    t_invalid_mtime => c(&["-d", "--min-mtime", "yesterday-ish"]);
    t_remove => c(&["-r", "-k0", "--min-mtime", "@{mid}"]).fx(Ages);
    t_move_recursive => c(&["-m", "{dest}", "-k1", "--min-atime", "@{far}"]);
    t_unreadable_subdir => c(&["-d", "-vv", "-k0", "--min-mtime", "@{far}"]).locked(&["cache/locked"]);
    t_unreadable_subdir_utf8 => c(&["-d", "-vv", "-k0", "--min-mtime", "@{far}"]).locked(&["cache/locked"]).utf8();
    g_unreadable_subdir => c(&["-d", "-vv", "-k0"]).locked(&["cache/locked"]);
    // --- quiet ---------------------------------------------------------------
    q_d => c(&["-d", "-q"]);
    q_d_v => c(&["-d", "-q", "-v"]);
    q_no_candidates => c(&["-d", "-q", "-k10"]);
    q_error_still_printed => sc(&["-d", "-q", "-c", "{root}/nope"]);
    // --- remove ------------------------------------------------------------
    r_default => c(&["-r"]);
    r_long_prefix => c(&["--rem", "-k2"]);
    r_v => c(&["-r", "-v", "-k1"]).unordered();
    r_vv => c(&["-r", "-vv", "-k2"]).unordered();
    r_k0_f => c(&["-r", "-f", "-k0"]);
    r_f_v => c(&["-r", "-f", "-v", "-k1"]).unordered();
    r_q => c(&["-r", "-q", "-k0"]);
    r_whitelist => c(&["-r", "-k0", "firefox", "my pkg"]);
    r_arch => c(&["-r", "-k1", "-a", "i686"]);
    r_no_candidates => c(&["-r", "-k10"]);
    // --- move --------------------------------------------------------------
    m_default => c(&["-m", "{dest}"]);
    m_v => c(&["-m", "{dest}", "-v", "-k1"]).unordered();
    m_relative => c(&["-m", "dest", "-k1"]);
    m_relative_dot_slash => c(&["-m", "./dest/", "-k1"]);
    m_long_eq => c(&["--move={dest}", "-k0"]);
    m_missing => c(&["-m", "{root}/nope"]);
    m_missing_relative => c(&["-m", "nope"]);
    m_is_file => c(&["-m", "afile"]);
    m_space_in_name => c(&["-m", "dest dir", "-k1"]).dirs(&["dest dir"]);
    m_f_q => c(&["-m", "{dest}", "-f", "-q"]);
    // --- cachedirs ---------------------------------------------------------
    c_multi_d => sc(&["-d", "-v", "-k1", "-c", "{cache}", "-c", "{cache2}"]).second();
    c_multi_vvv => sc(&["-d", "-vvv", "-k1", "-c", "{cache2}", "-c", "{cache}"]).second();
    c_multi_r => sc(&["-r", "-k1", "-c", "{cache}", "-c", "{cache2}"]).second();
    c_multi_m => sc(&["-m", "{dest}", "-k2", "-c", "{cache}", "--cachedir={cache2}"]).second();
    c_second_missing => sc(&["-r", "-k0", "-c", "{cache}", "-c", "{root}/nope"]);
    c_first_missing => sc(&["-r", "-k0", "-c", "{root}/nope", "-c", "{cache}"]);
    c_missing => sc(&["-d", "-c", "{root}/nope"]);
    c_is_file => sc(&["-d", "-c", "{root}/afile"]);
    c_relative => sc(&["-d", "-vv", "-k1", "-c", "cache"]);
    c_trailing_slash => sc(&["-d", "-vv", "-k1", "-c", "{cache}/"]);
    c_same_twice => sc(&["-d", "-v", "-k1", "-c", "{cache}", "-c", "{cache}"]);
    c_same_twice_r => sc(&["-r", "-k1", "-c", "{cache}", "-c", "{cache}"]).unordered();
    c_default_d => sc(&["-d", "-vv", "-k1"]).defcache();
    c_default_r => sc(&["-r", "-k1"]).defcache();
    c_default_empty => sc(&["-d", "-k0"]);
    // --- special caches ----------------------------------------------------
    empty_d => c(&["-d"]).fx(Empty);
    empty_r => c(&["-r", "-k0"]).fx(Empty);
    empty_times => c(&["-d", "-k0", "--min-atime", "@{far}"]).fx(Empty);
    nopkgs_d => c(&["-d", "-v", "-k0"]).fx(NoPackages);
    nopkgs_times => c(&["-d", "-vv", "-k0", "--min-mtime", "@{far}"]).fx(NoPackages);
    // --- random caches -----------------------------------------------------
    rand1_vv => c(&["-d", "-vv", "-k1"]).fx(Random(1, 40));
    rand2_vvv => c(&["-d", "-vvv", "-k2"]).fx(Random(2, 40));
    rand3_r => c(&["-r", "-k1"]).fx(Random(3, 40));
    rand4_m_q => c(&["-m", "{dest}", "-k0", "-q"]).fx(Random(4, 40));
    rand5_times => c(&["-d", "-v", "-k1", "--min-mtime", "@{mid}"]).fx(Random(5, 40));
    rand6_default => c(&["-d", "-v"]).fx(Random(6, 200));
    // --- operations and option parsing -------------------------------------
    p_no_op => c(&[]);
    p_no_op_v => c(&["-v", "-k1"]);
    p_two_ops => c(&["-d", "-r"]);
    p_three_ops => c(&["-d", "-r", "-m", "{dest}"]);
    p_dryrun_twice => c(&["-d", "-d"]);
    p_unknown_short => c(&["-d", "-x"]);
    p_unknown_in_group => c(&["-dxv"]);
    p_unknown_long => c(&["-d", "--bogus"]);
    p_ambiguous_min => c(&["-d", "--min", "@1"]);
    p_ambiguous_ver => c(&["--ver"]);
    p_ambiguous_n => c(&["-d", "--n"]);
    p_ambiguous_m => c(&["-d", "--m", "x"]);
    p_missing_arg_short => sc(&["-d", "-c"]);
    p_missing_arg_long => sc(&["-d", "--cachedir"]);
    p_missing_arg_k_end => sc(&["-c", "{cache}", "-d", "-k"]);
    p_noarg_with_value => c(&["--dryrun=1"]);
    p_noop_before_cachedir => sc(&["-c", "{root}/nope"]);
    p_movedir_before_cachedir => sc(&["-m", "{root}/nope", "-c", "{root}/nope2"]);
    p_two_ops_before_movedir => sc(&["-r", "-m", "{root}/nope", "-c", "{cache}"]);
    // --- help / version ----------------------------------------------------
    h_short => sc(&["-h"]);
    h_long => sc(&["--help"]);
    h_prefix => sc(&["--he"]);
    h_grouped => sc(&["-dh"]);
    h_parse_error_first => sc(&["-h", "-x"]);
    h_help_before_bad_k => sc(&["-h", "-k", "x"]);
    v_short => sc(&["-V"]).version_only();
    v_long => sc(&["--version"]).version_only();
}

// ---------------------------------------------------------------------------
// Intentional divergences
// ---------------------------------------------------------------------------
//
// Behaviour where paccache-rs deliberately differs from the original (see the
// README, "Differences from paccache"). These tests do not compare with the
// original as-is: they assert what *we* do, either explicitly or by comparing
// with the original run on the equivalent, non-quirky arguments.
//
// 1. `-k` accepts only `[0-9]+`, base 10, up to i64::MAX. The original's
//    `keep` is `declare -i`, so the argument is evaluated as bash arithmetic
//    (`010` = 8, `0x2`, `2#11`, `1+1`, variable names, empty = 0, overflow
//    wraps) and an arithmetic error (`08`, `9z`, `1v`) aborts its option loop
//    and ends with exit 0.
// 2. A `%` in the `-m` directory is printed literally (the original uses the
//    summary line as a printf format).

const K_DIE: &str = "==> ERROR: argument to option -k must be a non-negative integer\n";

/// Our binary rejects the `-k` value: the die message, exit 1, nothing on
/// stdout, cache untouched.
fn ours_rejects_k(name: &str, s: Scenario) {
    let (_tmp, root) = prepare(&s, &ours_path());
    let pristine = tree(&root);
    drop(_tmp);
    let got = run_one(&s, &ours_path());
    assert_eq!(got.code, Some(1), "[{name}] exit code");
    assert_eq!(show(&got.stdout), "", "[{name}] stdout");
    assert_eq!(show(&got.stderr), K_DIE, "[{name}] stderr");
    assert_eq!(got.tree, pristine, "[{name}] tree");
}

/// Our binary with `ours` behaves exactly like the original with `original`.
fn ours_like_original_with(name: &str, ours: Scenario, original: Scenario) {
    let want = run_one(&original, &original_path());
    let got = run_one(&ours, &ours_path());
    let problems = compare(
        name,
        &ours,
        &want,
        &got,
        ("original(equivalent args)", "paccache-rs"),
    );
    assert!(
        problems.is_empty(),
        "divergence `{name}`: ours {:?} vs original {:?}\n\n{}",
        ours.args,
        original.args,
        problems.join("\n\n")
    );
}

macro_rules! rejects_k {
    ($($name:ident => $s:expr;)*) => {
        $(
            #[test]
            fn $name() {
                ours_rejects_k(stringify!($name), $s);
            }
        )*
    };
}

rejects_k! {
    divergence_k_overflow_u64_wrap => c(&["-d", "-v", "-k", "18446744073709551617"]);
    divergence_k_overflow_huge => c(&["-d", "-k", "99999999999999999999"]);
    divergence_k_overflow_i64_max_plus_1 => c(&["-d", "-k", "9223372036854775808"]);
    divergence_k_hex => c(&["-d", "-v", "-k", "0x2"]);
    divergence_k_base_syntax => c(&["-d", "-v", "-k", "2#11"]);
    divergence_k_expression => c(&["-d", "-v", "-k", "1+1"]);
    divergence_k_name_is_var_zero => c(&["-d", "-v", "-k", "abc"]);
    divergence_k_var_verbose => c(&["-d", "-vv", "-k", "verbose"]);
    divergence_k_empty => c(&["-d", "-v", "-k", ""]);
    divergence_k_empty_long_eq => c(&["-d", "-v", "--keep="]);
    divergence_k_space_padded => c(&["-d", "-v", "-k", " 2"]);
    divergence_k_plus => c(&["-d", "-v", "-k", "+1"]);
    divergence_k_abort_grouped_1v => ck(&["-dk1v", "vim"]);
    divergence_k_abort_ignores_later_opts => ck(&["-d", "-k", "9z", "-r", "-k0"]);
    divergence_k_bad_before_help => sc(&["-k", "x", "-h"]);
    divergence_k_error_before_op_check => sc(&["-k", "x", "-c", "{root}/nope"]);
    divergence_k_eats_next_option => c(&["-d", "-k"]);
}

/// `-k` with leading zeros is plain base 10 (the original: octal, or an
/// arithmetic error for `08` that aborts its option loop).
#[test]
fn divergence_k_010_is_decimal() {
    ours_like_original_with(
        "divergence_k_010_is_decimal",
        c(&["-d", "-v", "-k", "010", "many"]),
        c(&["-d", "-v", "-k", "10", "many"]),
    );
}

#[test]
fn divergence_k_08_is_decimal() {
    ours_like_original_with(
        "divergence_k_08_is_decimal",
        ck(&["-d", "-v", "-k", "08", "linux"]),
        ck(&["-d", "-v", "-k", "8", "linux"]),
    );
}

#[test]
fn divergence_k_08_then_cachedir() {
    ours_like_original_with(
        "divergence_k_08_then_cachedir",
        sc(&["-d", "-v", "-k", "08", "-c", "{cache}", "linux"]).defcache(),
        sc(&["-d", "-v", "-k", "8", "-c", "{cache}", "linux"]).defcache(),
    );
}

#[test]
fn divergence_k_i64_max_accepted() {
    ours_like_original_with(
        "divergence_k_i64_max_accepted",
        c(&["-d", "-v", "-k", "9223372036854775807"]),
        c(&["-d", "-v", "-k", "9223372036854775807"]),
    );
}

/// The `-m` directory is printed literally even if it contains `%`: same as
/// the original with a `%`-free directory name.
#[test]
fn divergence_m_percent_in_name() {
    let name = "divergence_m_percent_in_name";
    let ours = c(&["-m", "de%sst", "-k1"]).dirs(&["de%sst"]);
    let original = c(&["-m", "deXsst", "-k1"]).dirs(&["deXsst"]);
    let fix = |o: Outcome| {
        let r = |b: &[u8]| {
            String::from_utf8_lossy(b)
                .replace("deXsst", "de%sst")
                .into_bytes()
        };
        let mut tree: Vec<String> = o
            .tree
            .iter()
            .map(|l| l.replace("deXsst", "de%sst"))
            .collect();
        tree.sort();
        Outcome {
            code: o.code,
            stdout: r(&o.stdout),
            stderr: r(&o.stderr),
            tree,
        }
    };
    let want = fix(run_one(&original, &original_path()));
    let mut got = run_one(&ours, &ours_path());
    got.tree.sort();
    assert!(
        show(&got.stdout).contains("moved to '<ROOT>/de%sst'"),
        "[{name}] stdout: {}",
        show(&got.stdout)
    );
    let problems = compare(name, &ours, &want, &got, ("original(no %)", "paccache-rs"));
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}

/// Debug helper: `COMPAT_DUMP_DIR=/some/dir cargo test --test compat dump_fixture -- --ignored`
/// materializes the standard fixture (seed `COMPAT_DUMP_SEED`, default 1) there.
#[test]
#[ignore]
fn dump_fixture() {
    let dir = PathBuf::from(env::var_os("COMPAT_DUMP_DIR").expect("set COMPAT_DUMP_DIR"));
    let seed = env::var("COMPAT_DUMP_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    fs::create_dir_all(&dir).unwrap();
    support::standard(seed).materialize(&dir).unwrap();
}
