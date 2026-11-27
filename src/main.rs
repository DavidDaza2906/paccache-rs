//! Orchestration: the top-level flow of `/usr/bin/paccache` (pacman-contrib 1.13.1),
//! step by step and in the same order, so that every message, exit code and effect
//! on disk happens when the script would produce it.
use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::io::{self, IsTerminal, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use paccache_rs::actions::{self, ApplyError, Op};
use paccache_rs::cli::{self, Config, Parsed};
use paccache_rs::output::{self, Out};
use paccache_rs::scan;
use paccache_rs::select::{self, Filter};

/// The real environment for option parsing.
struct SysEnv {
    argv0: OsString,
}

impl cli::Env for SysEnv {
    fn stdin_is_tty(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn read_stdin(&mut self) -> String {
        let mut buf = Vec::new();
        let _ = io::stdin().lock().read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// `read -r -d '' -a ign < <(pacman -Qq)`: the output is used whatever the
    /// exit status; `cli` splits it into lines.
    fn installed_packages(&mut self) -> Option<Vec<String>> {
        match Command::new("pacman")
            .arg("-Qq")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
        {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                (!s.is_empty()).then(|| vec![s])
            }
            Err(_) => {
                not_found(&self.argv0, 322, "pacman");
                None
            }
        }
    }

    fn parse_date(&self, text: &str) -> Option<i64> {
        cli::system_parse_date(text)
    }

    fn progname(&self) -> String {
        progname(&self.argv0)
    }
}

fn progname(argv0: &OsStr) -> String {
    let b = argv0.as_bytes();
    let base = match b.iter().rposition(|&c| c == b'/') {
        Some(i) => &b[i + 1..],
        None => b,
    };
    String::from_utf8_lossy(base).into_owned()
}

/// What bash prints when a command of the script cannot be found.
fn not_found(argv0: &OsStr, line: u32, cmd: &str) {
    eprintln!(
        "{}: line {line}: {cmd}: command not found",
        argv0.to_string_lossy()
    );
}

/// The logical working directory bash starts with: `$PWD` if it is absolute and
/// names the current directory, otherwise `getcwd()`.
fn startup_pwd() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(pwd) = std::env::var_os("PWD") {
        let pwd = PathBuf::from(pwd);
        if pwd.is_absolute() {
            if let (Ok(a), Ok(b)) = (std::fs::metadata(&pwd), std::fs::metadata(".")) {
                if a.dev() == b.dev() && a.ino() == b.ino() {
                    return pwd;
                }
            }
        }
    }
    cwd
}

/// bash's `sh_canonpath(path, PATH_CHECKDOTDOT | PATH_CHECKEXISTS)`: remove `.`,
/// `..` and repeated slashes textually (a leading `//` is kept, as POSIX allows),
/// checking that every directory a `..` steps out of and the result exist.
fn canonpath(path: &[u8]) -> Option<Vec<u8>> {
    let double = path.starts_with(b"//") && !path.starts_with(b"///");
    let root: &[u8] = if double { b"//" } else { b"/" };
    let mut parts: Vec<&[u8]> = Vec::new();
    let join = |parts: &[&[u8]]| {
        let mut out = root.to_vec();
        out.extend_from_slice(&parts.join(&b'/'));
        out
    };
    for comp in path.split(|&c| c == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                if !Path::new(OsStr::from_bytes(&join(&parts))).is_dir() {
                    return None;
                }
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let out = join(&parts);
    Path::new(OsStr::from_bytes(&out)).is_dir().then_some(out)
}

/// `$PWD` after `pushd DIR` (cd -L), starting from the logical `pwd`. Falls back
/// to the physical directory when canonicalization fails, as bash does.
fn logical_cd(pwd: &Path, dir: &Path) -> PathBuf {
    let raw = dir.as_os_str().as_bytes();
    let full = if raw.starts_with(b"/") {
        raw.to_vec()
    } else {
        let mut v = pwd.as_os_str().as_bytes().to_vec();
        if !v.ends_with(b"/") {
            v.push(b'/');
        }
        v.extend_from_slice(raw);
        v
    };
    match canonpath(&full) {
        Some(p) => PathBuf::from(OsString::from_vec(p)),
        None => {
            std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(OsString::from_vec(full)))
        }
    }
}

/// Would `chdir(dir)` succeed (search permission for the effective ids)?
fn can_chdir(dir: &Path) -> bool {
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
}

/// `mapfile -t cachedirs < <(pacman-conf CacheDir)`, `pacman-conf` found in PATH.
fn default_cachedirs(argv0: &OsStr) -> Vec<PathBuf> {
    let out = match Command::new("pacman-conf")
        .arg("CacheDir")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
    {
        Ok(o) => o.stdout,
        Err(_) => {
            not_found(argv0, 348, "pacman-conf");
            return Vec::new();
        }
    };
    let mut lines: Vec<&[u8]> = out.split(|&c| c == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|l| PathBuf::from(OsString::from_vec(l.to_vec())))
        .collect()
}

fn main() {
    scan::init_collation();
    // Rust ignores SIGPIPE; bash's printf does not (`paccache-rs -dv | head`).
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_else(|| "paccache-rs".into());
    let args: Vec<OsString> = args.collect();
    std::process::exit(run(&argv0, &args));
}

fn run(argv0: &OsStr, args: &[OsString]) -> i32 {
    let stderr_tty = io::stderr().is_terminal();
    let mut env = SysEnv {
        argv0: argv0.to_os_string(),
    };
    let cfg: Config = match cli::parse(args, &mut env) {
        Ok(Parsed::Run(cfg)) => cfg,
        Ok(Parsed::Help(text)) | Ok(Parsed::Version(text)) => {
            print!("{text}");
            return 0;
        }
        Err(e) => {
            e.emit(stderr_tty);
            return e.code;
        }
    };

    // [[ -t 2 ]] && (( USE_COLOR )) && colorize
    let out = Out {
        color: stderr_tty && !cfg.nocolor,
        quiet: cfg.quiet,
    };

    let cachedirs = if cfg.cachedirs.is_empty() {
        default_cachedirs(argv0)
    } else {
        cfg.cachedirs.clone()
    };

    let pwd = startup_pwd();
    let mut op = cfg.op.clone();
    let modifying = !matches!(op, Op::DryRun);
    let mut needsroot = false;

    // [[ $movedir && ! -d $movedir ]] && die ...
    if let Op::Move(movedir) = &mut op {
        if !movedir.as_os_str().is_empty() {
            if !movedir.is_dir() {
                out.error(&format!(
                    "destination directory '{}' does not exist or is not a directory",
                    movedir.to_string_lossy()
                ));
                return 1;
            }
            // movedir=$PWD/$movedir, textually.
            if !movedir.as_os_str().as_bytes().starts_with(b"/") {
                let mut abs = pwd.as_os_str().as_bytes().to_vec();
                abs.push(b'/');
                abs.extend_from_slice(movedir.as_os_str().as_bytes());
                *movedir = PathBuf::from(OsString::from_vec(abs));
            }
            needsroot |= actions::needs_root(&[], Some(movedir));
        }
    }

    let with_times = cfg.min_atime.is_some_and(|t| t != 0) || cfg.min_mtime.is_some_and(|t| t != 0);
    let filter = Filter {
        keep: cfg.keep,
        arch: cfg.arch.clone(),
        min_atime: cfg.min_atime.filter(|&t| t != 0),
        min_mtime: cfg.min_mtime.filter(|&t| t != 0),
        whitelist: cfg.whitelist.iter().cloned().collect::<HashSet<_>>(),
        blacklist: cfg.blacklist.iter().cloned().collect::<HashSet<_>>(),
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    for cachedir in &cachedirs {
        if !cachedir.is_dir() {
            out.error(&format!(
                "cachedir '{}' does not exist or is not a directory",
                cachedir.to_string_lossy()
            ));
            return 1;
        }
        if modifying {
            needsroot |= actions::needs_root(std::slice::from_ref(cachedir), None);
        }
        if !can_chdir(cachedir) {
            out.error(&format!(
                "failed to chdir to '{}'",
                cachedir.to_string_lossy()
            ));
            return 1;
        }
        let dir = logical_cd(&pwd, cachedir);
        let entries = scan::scan(&dir, with_times).unwrap_or_default();
        candidates.extend(
            select::candidates(entries, &filter)
                .into_iter()
                .map(|e| e.file.path),
        );
    }

    if candidates.is_empty() {
        out.msg("no candidate packages found for pruning");
        return 0;
    }

    let pkgcount = candidates.len();
    let candidates = actions::with_sigs(&candidates);
    let totalsaved = total_size(&candidates);

    match actions::apply(&op, &candidates, cfg.force, cfg.verbose > 0, needsroot) {
        Ok(0) => {}
        Ok(code) => return code,
        Err(ApplyError::NoSudo) => {
            out.error("Cannot find the sudo binary!");
            out.error("Either install \"sudo\" or run as root.");
            return 1;
        }
        Err(ApplyError::Spawn(_)) => {
            not_found(argv0, if op == Op::Remove { 426 } else { 428 }, "xargs");
            return 127;
        }
    }

    output::summarize(
        &out,
        &op,
        cfg.verbose,
        cfg.null,
        pkgcount,
        &candidates,
        totalsaved,
    );
    0
}

/// `xargs -0 stat -c %s | awk '{ sum += $1 }'`: files that cannot be stat'ed are
/// reported like stat does and left out of the sum.
fn total_size(paths: &[PathBuf]) -> u64 {
    actions::total_size(paths).unwrap_or_else(|_| {
        paths
            .iter()
            .map(|p| match actions::total_size(std::slice::from_ref(p)) {
                Ok(n) => n,
                Err(e) => {
                    let msg = e.to_string();
                    let msg = msg.split(" (os error").next().unwrap_or(&msg);
                    eprintln!("stat: cannot statx '{}': {msg}", p.to_string_lossy());
                    0
                }
            })
            .sum()
    })
}
