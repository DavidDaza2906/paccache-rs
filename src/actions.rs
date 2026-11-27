//! Signatures, sizes and rm/mv (owner: agent C).
//!
//! Removal and moving are done by really running what the script runs:
//! `[sudo] xargs -0 rm [-f] [-v]` / `[sudo] xargs -0 mv [-f] [-v] -t DIR`, feeding the
//! NUL-separated paths on stdin. This keeps coreutils' `-v` lines (`removed 'x'`,
//! `renamed 'a' -> 'b'`), its quoting of odd names, its error messages and xargs'
//! batching and exit codes (123 when an invocation failed, 124/125/126/127, signals)
//! identical by construction, at the cost of a couple of processes that only exist on
//! the `-r`/`-m` path (listing, sorting, filtering and sizes stay in-process).
//!
//! Quirks replicated:
//! * `runasroot` does `type -p sudo`, which PRINTS the sudo path on stdout (e.g.
//!   `/usr/bin/sudo`, or `./sudo` for an empty/`.` PATH element) before running it —
//!   even with `-q`. Without sudo the script prints two errors and exits 1; `apply`
//!   returns [`ApplyError::NoSudo`] so `main` prints them with `output::Out::error`.
//! * `[[ -w DIR ]]` is `faccessat(..., W_OK, AT_EACCESS)` (effective ids, read-only
//!   filesystems count as not writable), as bash does on Linux.
//! * `[[ -f X.sig ]]` follows symlinks; `stat -c %s` (no `-L`) does not.
//! * Children get SIGPIPE back to its default disposition (Rust ignores it, bash does
//!   not), so `paccache-rs -rv | head -1` behaves like the script.
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    DryRun,
    Remove,
    Move(PathBuf),
}

/// Why `apply` could not run the command.
#[derive(Debug)]
pub enum ApplyError {
    /// Root needed, not root, and no `sudo` in PATH. The script prints
    /// `error 'Cannot find the sudo binary!'` and
    /// `die 'Either install "sudo" or run as root.'` → exit 1.
    NoSudo,
    /// Could not start the command at all (e.g. no `xargs`); bash would print
    /// `...: command not found` and exit 127.
    Spawn(io::Error),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::NoSudo => write!(f, "Cannot find the sudo binary!"),
            ApplyError::Spawn(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Candidates plus each existing `X.sig`, right after its package (script order).
/// Existence is `[[ -f X.sig ]]`: a regular file, following symlinks.
pub fn with_sigs(cands: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(cands.len() * 2);
    for c in cands {
        out.push(c.clone());
        let mut sig = c.as_os_str().to_os_string();
        sig.push(".sig");
        let sig = PathBuf::from(sig);
        if fs::metadata(&sig).is_ok_and(|m| m.is_file()) {
            out.push(sig);
        }
    }
    out
}

/// Sum of file sizes (stat -c %s, i.e. lstat: a symlink counts its own size) of `paths`.
/// Errors if a path cannot be lstat'ed (the script would print stat's error on stderr
/// and sum the rest; that only happens if files vanish between listing and here).
pub fn total_size(paths: &[PathBuf]) -> io::Result<u64> {
    let mut sum = 0u64;
    for p in paths {
        sum = sum.wrapping_add(fs::symlink_metadata(p)?.len());
    }
    Ok(sum)
}

/// Whether removing from `cachedirs` / moving into `movedir` needs root (not writable).
/// Only meaningful for `-r`/`-m` (the caller should not ask for `-d`).
pub fn needs_root(cachedirs: &[PathBuf], movedir: Option<&Path>) -> bool {
    movedir.is_some_and(|d| !d.as_os_str().is_empty() && !writable(d))
        || cachedirs.iter().any(|d| !writable(d))
}

/// bash `test -w`.
fn writable(p: &Path) -> bool {
    let Ok(c) = CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::W_OK, libc::AT_EACCESS) == 0 }
}

/// bash `type -p NAME`: first executable non-directory `DIR/NAME` along `path`
/// (an empty element means `.`, printed as `./NAME`). `path` None = PATH unset, where
/// bash uses its built-in default.
fn type_p(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let default = OsStr::new("/usr/local/sbin:/usr/local/bin:/usr/bin");
    let path = path.unwrap_or(default);
    for elem in path.as_bytes().split(|&c| c == b':') {
        let mut cand = if elem.is_empty() {
            b".".to_vec()
        } else {
            elem.to_vec()
        };
        cand.push(b'/');
        cand.extend_from_slice(name.as_bytes());
        let cand = PathBuf::from(OsString::from_vec(cand));
        let Ok(md) = fs::metadata(&cand) else {
            continue;
        };
        if md.is_dir() {
            continue;
        }
        let Ok(c) = CString::new(cand.as_os_str().as_bytes()) else {
            continue;
        };
        if unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) } == 0
        {
            return Some(cand);
        }
    }
    None
}

/// The argv the script runs for `op` (None for a dry run). `sudo` = path to prefix with.
pub fn command_argv(
    op: &Op,
    force: bool,
    verbose: bool,
    sudo: Option<&Path>,
) -> Option<Vec<OsString>> {
    let mut argv: Vec<OsString> = Vec::new();
    if let Some(s) = sudo {
        argv.push(s.as_os_str().to_os_string());
    }
    argv.push("xargs".into());
    argv.push("-0".into());
    match op {
        Op::DryRun => return None,
        Op::Remove => argv.push("rm".into()),
        Op::Move(_) => argv.push("mv".into()),
    }
    if force {
        argv.push("-f".into());
    }
    if verbose {
        argv.push("-v".into());
    }
    if let Op::Move(dir) = op {
        argv.push("-t".into());
        argv.push(dir.as_os_str().to_os_string());
    }
    Some(argv)
}

/// Perform Remove/Move. Output and errors match `xargs -0 rm [-f] [-v]` /
/// `xargs -0 mv [-f] [-v] -t DIR` (coreutils), via `sudo` when `as_root` and euid != 0.
/// `as_root` is the script's `needsroot` (see [`needs_root`]).
/// Returns the exit status the script would end with (errexit): 0 on success, xargs'
/// status otherwise (123 if rm/mv failed for some file), 128+N if killed by signal N.
/// A dry run does nothing and returns Ok(0).
///
/// Signature change vs. the scaffold: returns `Result<i32, ApplyError>` so `main` can
/// print the "no sudo" errors with its own formatting (colors) and exit 1.
pub fn apply(
    op: &Op,
    paths: &[PathBuf],
    force: bool,
    verbose: bool,
    as_root: bool,
) -> Result<i32, ApplyError> {
    if *op == Op::DryRun {
        return Ok(0);
    }
    let sudo = if as_root && unsafe { libc::geteuid() } != 0 {
        match type_p("sudo", std::env::var_os("PATH").as_deref()) {
            Some(s) => {
                // `type -p sudo` prints the path on stdout.
                let mut out = io::stdout().lock();
                let _ = out.write_all(s.as_os_str().as_bytes());
                let _ = out.write_all(b"\n");
                let _ = out.flush();
                Some(s)
            }
            None => return Err(ApplyError::NoSudo),
        }
    } else {
        None
    };
    let argv = command_argv(op, force, verbose, sudo.as_deref()).expect("not a dry run");
    run(&argv, paths, None)
        .map(|(code, _, _)| code)
        .map_err(ApplyError::Spawn)
}

type Captured = (i32, Vec<u8>, Vec<u8>);

/// Run `argv` with `paths` NUL-separated on stdin. `capture` = Some(()) to capture
/// stdout/stderr (tests); otherwise they are inherited.
fn run(argv: &[OsString], paths: &[PathBuf], capture: Option<()>) -> io::Result<Captured> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::piped());
    if capture.is_some() {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_os_str().as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Write from a thread so captured output pipes cannot deadlock us.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input); // EPIPE if the child quits early: ignore
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let code = match (out.status.code(), out.status.signal()) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        _ => 1,
    };
    Ok((code, out.stdout, out.stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn tmpdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "paccache-rs-actions-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn argv_shapes() {
        let s = |v: Option<Vec<OsString>>| {
            v.map(|v| {
                v.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(command_argv(&Op::DryRun, true, true, None), None);
        assert_eq!(
            s(command_argv(&Op::Remove, false, false, None)).unwrap(),
            ["xargs", "-0", "rm"]
        );
        assert_eq!(
            s(command_argv(&Op::Remove, true, true, None)).unwrap(),
            ["xargs", "-0", "rm", "-f", "-v"]
        );
        assert_eq!(
            s(command_argv(
                &Op::Move(p("/m d")),
                true,
                true,
                Some(Path::new("/usr/bin/sudo"))
            ))
            .unwrap(),
            [
                "/usr/bin/sudo",
                "xargs",
                "-0",
                "mv",
                "-f",
                "-v",
                "-t",
                "/m d"
            ]
        );
        assert_eq!(
            s(command_argv(&Op::Move(p("/m")), false, true, None)).unwrap(),
            ["xargs", "-0", "mv", "-v", "-t", "/m"]
        );
        assert_eq!(
            s(command_argv(
                &Op::Remove,
                false,
                false,
                Some(Path::new("./sudo"))
            ))
            .unwrap(),
            ["./sudo", "xargs", "-0", "rm"]
        );
    }

    #[test]
    fn type_p_like_bash() {
        let d = tmpdir("typep");
        let (exe, noexe, isdir) = (d.join("exe"), d.join("noexe"), d.join("isdir"));
        for x in [&exe, &noexe, &isdir] {
            fs::create_dir(x).unwrap();
        }
        fs::create_dir(isdir.join("sudo")).unwrap();
        fs::write(noexe.join("sudo"), "x").unwrap();
        fs::write(exe.join("sudo"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(exe.join("sudo"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = |parts: &[&Path]| {
            OsString::from(
                parts
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(":"),
            )
        };
        let pa = path(&[&isdir, &noexe, &exe]);
        assert_eq!(type_p("sudo", Some(&pa)), Some(exe.join("sudo")));
        let pb = path(&[&isdir, &noexe]);
        assert_eq!(type_p("sudo", Some(&pb)), None);
        // compare with real bash
        for pv in [&pa, &pb] {
            let o = Command::new("/usr/bin/bash")
                .arg("-c")
                .arg("type -p sudo")
                .env("PATH", pv)
                .output()
                .unwrap();
            let ours = type_p("sudo", Some(pv))
                .map(|p| format!("{}\n", p.display()))
                .unwrap_or_default();
            assert_eq!(String::from_utf8_lossy(&o.stdout), ours);
        }
        // empty element = "./sudo" (relative to cwd; just check the spelling)
        let cwd = std::env::current_dir().unwrap();
        if type_p("sudo", Some(OsStr::new("."))).is_none() && !cwd.join("sudo").exists() {
            assert_eq!(type_p("sudo", Some(OsStr::new(":"))), None);
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sigs_and_sizes() {
        let d = tmpdir("sigs");
        let a = d.join("a-1-1-any.pkg.tar.zst");
        let b = d.join("b-1-1-any.pkg.tar.zst");
        let c = d.join("c-1-1-any.pkg.tar.zst");
        let e = d.join("e-1-1-any.pkg.tar.zst");
        fs::write(&a, vec![0u8; 1000]).unwrap();
        fs::write(d.join("a-1-1-any.pkg.tar.zst.sig"), vec![0u8; 10]).unwrap();
        fs::write(&b, vec![0u8; 2000]).unwrap();
        fs::create_dir(d.join("b-1-1-any.pkg.tar.zst.sig")).unwrap(); // dir: not -f
        fs::write(&c, vec![0u8; 5]).unwrap();
        symlink(
            d.join("a-1-1-any.pkg.tar.zst.sig"),
            d.join("c-1-1-any.pkg.tar.zst.sig"),
        )
        .unwrap(); // -f follows
        symlink("a-1-1-any.pkg.tar.zst", &e).unwrap();
        symlink("nowhere", d.join("e-1-1-any.pkg.tar.zst.sig")).unwrap(); // dangling: not -f
        let all = with_sigs(&[a.clone(), b.clone(), c.clone(), e.clone()]);
        let sig = |x: &Path| PathBuf::from(format!("{}.sig", x.display()));
        assert_eq!(
            all,
            vec![a.clone(), sig(&a), b.clone(), c.clone(), sig(&c), e.clone()]
        );
        // compare with stat -c %s (lstat)
        let o = Command::new("stat")
            .arg("-c")
            .arg("%s")
            .args(&all)
            .output()
            .unwrap();
        let expected: u64 = String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(|l| l.parse::<u64>().unwrap())
            .sum();
        assert_eq!(total_size(&all).unwrap(), expected);
        assert!(total_size(&[d.join("missing")]).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn writable_like_bash() {
        let d = tmpdir("w");
        let ro = d.join("ro");
        let rw = d.join("rw");
        fs::create_dir(&ro).unwrap();
        fs::create_dir(&rw).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        for x in [
            &ro,
            &rw,
            &d.join("missing"),
            &PathBuf::from("/"),
            &PathBuf::from("/proc/1"),
        ] {
            let st = Command::new("/usr/bin/bash")
                .arg("-c")
                .arg("[[ -w $1 ]]")
                .arg("bash")
                .arg(x)
                .status()
                .unwrap();
            assert_eq!(writable(x), st.success(), "{}", x.display());
        }
        let root = unsafe { libc::geteuid() } == 0;
        let rw1 = std::slice::from_ref(&rw);
        assert!(!needs_root(rw1, None));
        assert_eq!(needs_root(&[rw.clone(), ro.clone()], None), !root);
        assert_eq!(needs_root(rw1, Some(&ro)), !root);
        assert!(!needs_root(rw1, Some(&rw)));
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(&d).unwrap();
    }

    /// Build two identical trees, run our command in one and the script's pipeline in
    /// the other (via bash), and compare stdout/stderr (with the dir names normalised),
    /// exit status and the resulting trees.
    fn compare_with_script(
        tag: &str,
        op_of: impl Fn(&Path) -> Op,
        force: bool,
        verbose: bool,
        setup: impl Fn(&Path) -> Vec<String>,
    ) {
        let ours = tmpdir(&format!("{tag}-o"));
        let theirs = tmpdir(&format!("{tag}-t"));
        let names_o = setup(&ours);
        let names_t = setup(&theirs);
        assert_eq!(names_o, names_t);
        let paths_o: Vec<PathBuf> = names_o.iter().map(|n| ours.join(n)).collect();
        let op_o = op_of(&ours);
        let argv = command_argv(&op_o, force, verbose, None).unwrap();
        let (code_o, out_o, err_o) = run(&argv, &paths_o, Some(())).unwrap();

        let op_t = op_of(&theirs);
        let mut cmdopts = String::new();
        if force {
            cmdopts.push_str(" -f");
        }
        if verbose {
            cmdopts.push_str(" -v");
        }
        let script = match &op_t {
            Op::Remove => format!("printf '%s\\0' \"${{@:2}}\" | xargs -0 rm{cmdopts}"),
            Op::Move(_) => format!("printf '%s\\0' \"${{@:2}}\" | xargs -0 mv{cmdopts} -t \"$1\""),
            Op::DryRun => unreachable!(),
        };
        let movedir = match &op_t {
            Op::Move(d) => d.clone(),
            _ => PathBuf::new(),
        };
        let o = Command::new("/usr/bin/bash")
            .arg("-c")
            .arg(format!("set -o errexit\n{script}"))
            .arg("bash")
            .arg(&movedir)
            .args(names_t.iter().map(|n| theirs.join(n)))
            .output()
            .unwrap();
        let norm = |b: &[u8], dir: &Path| {
            String::from_utf8_lossy(b).replace(&dir.to_string_lossy().into_owned(), "DIR")
        };
        assert_eq!(code_o, o.status.code().unwrap(), "exit code");
        assert_eq!(norm(&out_o, &ours), norm(&o.stdout, &theirs), "stdout");
        assert_eq!(norm(&err_o, &ours), norm(&o.stderr, &theirs), "stderr");
        assert_eq!(listing(&ours), listing(&theirs), "tree");
        for d in [&ours, &theirs] {
            let _ = Command::new("chmod").arg("-R").arg("u+rwx").arg(d).status();
            fs::remove_dir_all(d).unwrap();
        }
    }

    fn listing(root: &Path) -> Vec<String> {
        let mut v = Vec::new();
        fn walk(base: &Path, dir: &Path, v: &mut Vec<String>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                v.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                if e.file_type().unwrap().is_dir() {
                    walk(base, &p, v);
                }
            }
        }
        walk(root, root, &mut v);
        v.sort();
        v
    }

    fn odd_files(d: &Path) -> Vec<String> {
        let names = [
            "a-1-1-any.pkg.tar.zst",
            "a-1-1-any.pkg.tar.zst.sig",
            "it's-1-1-any.pkg.tar.zst",
            "sp ace-1-1-any.pkg.tar.zst",
            "new\nline-1-1-any.pkg.tar.zst",
            "tab\there-1-1-any.pkg.tar.zst",
            "\u{e9}-1-1-any.pkg.tar.zst",
        ];
        for n in names {
            fs::write(d.join(n), "x").unwrap();
        }
        let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        v.push("missing-1-1-any.pkg.tar.zst".into());
        v
    }

    #[test]
    fn remove_matches_script() {
        for (force, verbose) in [(false, false), (false, true), (true, false), (true, true)] {
            compare_with_script("rm", |_| Op::Remove, force, verbose, odd_files);
        }
    }

    #[test]
    fn move_matches_script() {
        for (force, verbose) in [(false, true), (true, true), (false, false)] {
            compare_with_script(
                "mv",
                |d| Op::Move(d.join("dest dir")),
                force,
                verbose,
                |d| {
                    fs::create_dir(d.join("dest dir")).unwrap();
                    fs::write(d.join("dest dir/a-1-1-any.pkg.tar.zst"), "old").unwrap();
                    odd_files(d)
                },
            );
        }
    }

    #[test]
    fn move_into_missing_dir_matches_script() {
        compare_with_script(
            "mvmiss",
            |d| Op::Move(d.join("nope")),
            false,
            true,
            odd_files,
        );
    }

    #[test]
    fn remove_in_readonly_dir_matches_script() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        compare_with_script(
            "rmro",
            |_| Op::Remove,
            true,
            true,
            |d| {
                let ro = d.join("ro");
                fs::create_dir(&ro).unwrap();
                fs::write(ro.join("p-1-1-any.pkg.tar.zst"), "x").unwrap();
                fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
                vec!["ro/p-1-1-any.pkg.tar.zst".into()]
            },
        );
    }

    #[test]
    fn many_paths_batch_like_xargs() {
        // more than one xargs batch (128 KiB command buffer)
        compare_with_script(
            "many",
            |_| Op::Remove,
            false,
            true,
            |d| {
                (0..3000)
                    .map(|i| {
                        let n = format!("{}-{i}-1-any.pkg.tar.zst", "x".repeat(60));
                        fs::write(d.join(&n), "").unwrap();
                        n
                    })
                    .collect()
            },
        );
    }

    #[test]
    fn duplicated_paths_fail_with_123_like_script() {
        // `-c X -c X` lists every candidate twice: the second rm fails -> xargs 123.
        compare_with_script(
            "dup",
            |_| Op::Remove,
            false,
            true,
            |d| {
                fs::write(d.join("a-1-1-any.pkg.tar.zst"), "x").unwrap();
                vec![
                    "a-1-1-any.pkg.tar.zst".into(),
                    "a-1-1-any.pkg.tar.zst".into(),
                ]
            },
        );
    }

    #[test]
    fn dry_run_is_noop() {
        assert_eq!(
            apply(&Op::DryRun, &[p("/nonexistent")], true, true, true).unwrap(),
            0
        );
    }

    #[test]
    fn apply_remove_real() {
        let d = tmpdir("apply");
        let f = d.join("z-1-1-any.pkg.tar.zst");
        fs::write(&f, "x").unwrap();
        assert_eq!(
            apply(&Op::Remove, std::slice::from_ref(&f), false, false, false).unwrap(),
            0
        );
        assert!(!f.exists());
        assert_eq!(apply(&Op::Remove, &[f], false, false, false).unwrap(), 123);
        fs::remove_dir_all(&d).unwrap();
    }
}
