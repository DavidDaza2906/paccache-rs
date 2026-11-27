//! Option parsing compatible with libmakepkg `parseopts` (owner: agent B).
//!
//! Parsing happens in the same two stages as the script:
//!
//! 1. [`parseopts`]: a byte-exact port of libmakepkg's `parseopts` (pacman 7.x),
//!    producing the normalised `OPTRET` array or one of its error lines.
//! 2. [`parse`]: the script's `while :; case $1 in ...` loop over `OPTRET`,
//!    executing each option *when it is reached* (so `-u`, `-i -`, `-h`, `-V` and
//!    the `die` of `-k` / `--min-atime` happen in command-line order), followed by
//!    the operation sanity check (none / more than one).
//!
//! Quirks of the original that are reproduced on purpose:
//!
//! * `case $shortopts in *$opt:*)` uses `$opt` unquoted, so `-*`, `-?` and `-\`
//!   behave as options *requiring an argument* and `-:` as a flag. The script's
//!   loop ignores these entries, but whatever they swallowed as "argument" is then
//!   interpreted by the loop like any other `OPTRET` word (`-\ -r` == `-r`).
//! * `-c ''` as the *first* cachedir makes the script fall back to `pacman-conf`
//!   (`[[ -z ${cachedirs[0]} ]]`), discarding the other `-c` values.
//! * `--min-atime @0` is the same as not giving the option (the script uses 0 as
//!   "unset").
//! * The `die` of the operation check happens after `colorize`, the ones inside
//!   the loop (`-k`, `--min-*`, `-u`) before it, so only the former are colored.
//!
//! Deliberate divergences:
//! * `-k` (DIVERGENCE, decided by the coordinator): the script's `declare -i keep`
//!   evaluates the argument as bash arithmetic; we require `[0-9]+` in base 10.
//! * When the quirks above make an option swallow the final `--` of `OPTRET`, bash
//!   loops forever; we stop at the end of the list.
//! * Short options are split into characters as in a UTF-8 locale (bash's
//!   `${1:i:1}`); in the C locale bash would split bytes.
//!
//! Dates: [`system_parse_date`] runs `date -d TEXT +%s` exactly like the script
//! (stderr inherited), see its documentation for the reasoning.
use crate::actions::Op;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub op: Op,
    pub arch: Option<String>,
    /// Empty = use `pacman-conf CacheDir`.
    pub cachedirs: Vec<PathBuf>,
    pub force: bool,
    /// From -i (commas or stdin) and -u (pacman -Qq), already merged.
    pub blacklist: Vec<String>,
    pub keep: usize,
    pub min_atime: Option<i64>,
    pub min_mtime: Option<i64>,
    pub nocolor: bool,
    pub null: bool,
    pub quiet: bool,
    pub verbose: u8,
    /// Positional args.
    pub whitelist: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Config),
    /// Text to print on stdout, exit 0.
    Help(String),
    Version(String),
}

/// Failure: exact message(s) and exit code the script produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// The text without trailing newline. For `!is_die` it is a raw parseopts
    /// line (`paccache: invalid option -- 'x'`); for `is_die` it is the text given
    /// to `die`/`error` (without the `==> ERROR: ` prefix). Use [`CliError::emit`].
    pub message: String,
    pub is_die: bool,
    pub code: i32,
    /// The `die` happened after the script's `colorize` step (operation check)
    /// and `--nocolor` was not given, so it is colored when stderr is a tty.
    /// Dies inside the option loop are never colored.
    pub colorable: bool,
}

impl CliError {
    fn raw(message: String) -> Self {
        CliError {
            message,
            is_die: false,
            code: 1,
            colorable: false,
        }
    }
    fn die(message: &str, colorable: bool) -> Self {
        CliError {
            message: message.to_string(),
            is_die: true,
            code: 1,
            colorable,
        }
    }

    /// Print the error on stderr exactly as the script would. `color` is whether
    /// stderr is a tty (`--nocolor` is already folded into `colorable`).
    pub fn emit(&self, color: bool) {
        let mut err = std::io::stderr().lock();
        let _ = self.emit_to(&mut err, color);
    }

    /// [`CliError::emit`] into any writer.
    pub fn emit_to(&self, w: &mut dyn std::io::Write, color: bool) -> std::io::Result<()> {
        if self.is_die {
            let pal = if color && self.colorable {
                crate::output::Palette::detect()
            } else {
                crate::output::Palette::none()
            };
            crate::output::write_error(w, pal, &self.message)
        } else {
            writeln!(w, "{}", self.message)
        }
    }
}

/// Environment hooks so parsing is testable.
pub trait Env {
    fn stdin_is_tty(&self) -> bool;
    fn read_stdin(&mut self) -> String;
    /// `pacman -Qq` output lines, or None on failure / empty.
    fn installed_packages(&mut self) -> Option<Vec<String>>;
    /// `date -d TEXT +%s` equivalent; None when unparsable.
    fn parse_date(&self, text: &str) -> Option<i64>;
    /// Basename of argv[0] (parseopts messages use it).
    fn progname(&self) -> String;
    /// Called after the option loop and before the operation check, when no
    /// cachedir was given: the point where the script runs `pacman-conf CacheDir`
    /// (so its stderr comes before a "no operation" error). `main` runs it here
    /// and keeps the result; the default does nothing.
    fn default_cachedirs_needed(&mut self) {}
}

pub const MYNAME: &str = "paccache";
pub const MYVER: &str = "1.13.1";

/// The script's `usage()` with `myname=paccache`, `myver=1.13.1`.
pub const USAGE: &str = r#"paccache v1.13.1

Flexible pacman cache cleaning utility.

Usage: paccache <operation> [options] [target ...]

Operations:
  -d, --dryrun      perform a dry run, only finding candidate packages
  -m, --move <dir>  move candidate packages to "dir"
  -r, --remove      remove candidate packages

Options:
  -a, --arch <arch>       scan for "arch" (default: all architectures)
  -c, --cachedir <dir>    scan "dir" for packages. can be used more than once
                          (default: read from /etc/pacman.conf)
  -f, --force             apply force to mv(1) and rm(1) operations
  -h, --help              display this help message and exit
  -i, --ignore <pkgs>     ignore "pkgs", comma-separated. Alternatively, specify
                          "-" to read package names from stdin, newline-
                          delimited
  -k, --keep <num>        keep "num" of each package in the cache (default: 3)
      --min-atime <time>
      --min-mtime <time>  keep packages with an atime/mtime that is not older
                          than the time given, even if this means keeping more
                          than specified through the '--keep' option. Accepts
                          arguments according to 'info "Date input formats"',
                          e.g. '30 days ago'
      --nocolor           do not colorize output
  -z, --null              use null delimiters for candidate names (only with -v
                          and -vv)
  -q, --quiet             minimize output
  -u, --uninstalled       target uninstalled packages
  -v, --verbose           increase verbosity. specify up to 3 times
  -V, --version           display version information and exit
"#;

/// `-V` text (the only output allowed to differ from the original).
pub const VERSION: &str = concat!(
    "paccache-rs ",
    env!("CARGO_PKG_VERSION"),
    " (compatible with paccache 1.13.1)\n",
    "Copyright (C) 2011 Dave Reisner <dreisner@archlinux.org>\n",
    "Rust rewrite: Copyright (C) 2026 the paccache-rs contributors\n",
);

/// The script's `OPT_SHORT` / `OPT_LONG`.
const OPT_LONG: &[&str] = &[
    "arch:",
    "cachedir:",
    "dryrun",
    "force",
    "help",
    "ignore:",
    "keep:",
    "min-atime:",
    "min-mtime:",
    "move:",
    "nocolor",
    "null",
    "quiet",
    "remove",
    "uninstalled",
    "verbose",
    "version",
];

const MSG_K: &str = "argument to option -k must be a non-negative integer";
const MSG_ATIME: &str = "argument to option --min-atime must be of the form described by 'info \"Date input formats\" '.";
const MSG_MTIME: &str = "argument to option --min-mtime must be of the form described by 'info \"Date input formats\" '.";
const MSG_U: &str = "failed to retrieve the list of installed packages";
const MSG_NOOP: &str = "no operation specified (use -h for help)";
const MSG_MANYOP: &str = "only one operation may be used at a time";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShortKind {
    Arg,
    Flag,
    Invalid,
}

/// `case $shortopts in *$opt:*) ... *$opt\?*) ... *$opt*) ...` for
/// `OPT_SHORT=':a:c:dfhi:k:m:qrsuVvz'`, including the glob characters that the
/// unquoted `$opt` turns into patterns (verified with bash for all of ASCII).
fn short_kind(c: &[u8]) -> ShortKind {
    match c {
        b"a" | b"c" | b"i" | b"k" | b"m" | b"*" | b"?" | b"\\" => ShortKind::Arg,
        b"d" | b"f" | b"h" | b"q" | b"r" | b"s" | b"u" | b"V" | b"v" | b"z" | b":" => {
            ShortKind::Flag
        }
        _ => ShortKind::Invalid,
    }
}

/// Split into "characters" as bash does in a UTF-8 locale: valid UTF-8
/// sequences are one character, every invalid byte is one character.
fn chars_of(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let n = match std::str::from_utf8(rest) {
            Ok(v) => v.chars().next().map_or(1, char::len_utf8),
            Err(e) if e.valid_up_to() > 0 => std::str::from_utf8(&rest[..e.valid_up_to()])
                .ok()
                .and_then(|v| v.chars().next())
                .map_or(1, char::len_utf8),
            Err(_) => 1,
        };
        out.push(&rest[..n]);
        rest = &rest[n..];
    }
    out
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `IFS='=' read -r opt optarg <<< "$s"`: first line only, first field to
/// `opt`, rest to `optarg` with bash's "single trailing delimiter" stripping.
fn read_eq_split(s: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let line = match s.iter().position(|&b| b == b'\n') {
        Some(p) => &s[..p],
        None => s,
    };
    match line.iter().position(|&b| b == b'=') {
        None => (line.to_vec(), Vec::new()),
        Some(p) => {
            let opt = line[..p].to_vec();
            let mut rest = &line[p + 1..];
            if let Some((&b'=', head)) = rest.split_last() {
                if !head.contains(&b'=') {
                    rest = head;
                }
            }
            (opt, rest.to_vec())
        }
    }
}

/// libmakepkg `parseopts` with paccache's option tables. Returns `OPTRET` or
/// the raw error line (without newline; exit status 1).
pub fn parseopts(args: &[Vec<u8>], progname: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut optret: Vec<Vec<u8>> = Vec::new();
    let mut unused: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.as_slice() == b"--" {
            i += 1;
            break;
        } else if a.len() >= 2 && a[0] == b'-' && a[1] != b'-' {
            let chars = chars_of(a);
            let n = chars.len();
            let mut k = 1;
            while k < n {
                let opt = chars[k];
                match short_kind(opt) {
                    ShortKind::Arg => {
                        let mut o = b"-".to_vec();
                        o.extend_from_slice(opt);
                        if k < n - 1 {
                            optret.push(o);
                            optret.push(chars[k + 1..].concat());
                        } else if i + 1 < args.len() {
                            optret.push(o);
                            optret.push(args[i + 1].clone());
                            i += 1;
                        } else {
                            return Err(format!(
                                "{progname}: option requires an argument -- '{}'",
                                lossy(opt)
                            ));
                        }
                        break;
                    }
                    ShortKind::Flag => {
                        let mut o = b"-".to_vec();
                        o.extend_from_slice(opt);
                        optret.push(o);
                    }
                    ShortKind::Invalid => {
                        return Err(format!("{progname}: invalid option -- '{}'", lossy(opt)));
                    }
                }
                k += 1;
            }
        } else if a.len() > 2 && a.starts_with(b"--") {
            let (opt, optarg) = read_eq_split(&a[2..]);
            let has_eq = a.contains(&b'=');
            // longoptmatch
            let mut matches: Vec<&str> = Vec::new();
            for o in OPT_LONG {
                let name = o.trim_end_matches(':');
                if name.as_bytes() == opt.as_slice() {
                    matches = vec![o];
                    break;
                }
                if name.as_bytes().starts_with(&opt) {
                    matches.push(o);
                }
            }
            match matches.len() {
                0 => return Err(format!("{progname}: invalid option '--{}'", lossy(&opt))),
                1 => {
                    let name = matches[0].trim_end_matches(':');
                    let long = format!("--{name}").into_bytes();
                    if matches[0].ends_with(':') {
                        if has_eq {
                            optret.push(long);
                            optret.push(optarg);
                        } else if i + 1 < args.len() {
                            optret.push(long);
                            optret.push(args[i + 1].clone());
                            i += 1;
                        } else {
                            return Err(format!(
                                "{progname}: option '--{name}' requires an argument"
                            ));
                        }
                    } else if has_eq {
                        return Err(format!(
                            "{progname}: option '--{name}' does not allow an argument"
                        ));
                    } else {
                        optret.push(long);
                    }
                }
                _ => {
                    let mut m = format!(
                        "{progname}: option '--{}' is ambiguous; possibilities:",
                        lossy(&opt)
                    );
                    for o in &matches {
                        m.push_str(&format!(" '{}'", o.trim_end_matches(':')));
                    }
                    return Err(m);
                }
            }
        } else {
            unused.push(a.clone());
        }
        i += 1;
    }
    optret.push(b"--".to_vec());
    optret.extend(unused);
    optret.extend(args[i.min(args.len())..].iter().cloned());
    Ok(optret)
}

/// `-k` argument: `[0-9]+` in base 10, at most `i64::MAX` (bash integers are
/// 64-bit signed: the original rejects 2^63..2^64-1 too, as they wrap negative).
fn parse_keep(a: &[u8]) -> Option<usize> {
    if a.is_empty() || !a.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v = a.iter().try_fold(0i64, |v, &d| {
        v.checked_mul(10)?.checked_add((d - b'0') as i64)
    })?;
    usize::try_from(v).ok()
}

/// `IFS=',' read -r -a ign <<< "$s"`.
fn split_commas(s: &[u8]) -> Vec<String> {
    let line = match s.iter().position(|&b| b == b'\n') {
        Some(p) => &s[..p],
        None => s,
    };
    if line.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&[u8]> = line.split(|&b| b == b',').collect();
    if parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    parts.into_iter().map(lossy).collect()
}

/// Fields of `IFS=$'\n' read -r -d '' -a ign` on an already NUL-cut text.
fn split_lines(s: &str) -> Vec<String> {
    s.split('\n')
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse args (without argv[0]) including the script's post-parse sanity checks
/// on operations (none / more than one). Filesystem checks are NOT done here.
pub fn parse(args: &[OsString], env: &mut dyn Env) -> Result<Parsed, CliError> {
    let args: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let optret = parseopts(&args, &env.progname()).map_err(CliError::raw)?;

    let mut dryrun = false;
    let mut delete = false;
    let mut mv = false;
    let mut movedir: Vec<u8> = Vec::new();
    let mut scanarch = String::new();
    let mut cachedirs: Vec<PathBuf> = Vec::new();
    let mut force = false;
    let mut blacklist: Vec<String> = Vec::new();
    let mut keep: usize = 3;
    let mut min_atime: i64 = 0;
    let mut min_mtime: i64 = 0;
    let mut nocolor = false;
    let mut null = false;
    let mut quiet = false;
    let mut verbose: u64 = 0;
    // Unread part of stdin (`read -d ''` stops at the first NUL).
    let mut stdin_buf: Option<String> = None;

    let empty: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < optret.len() {
        let w = optret[i].as_slice();
        let arg2 = optret.get(i + 1).unwrap_or(&empty);
        let mut takes_arg = true;
        match w {
            b"-a" | b"--arch" => scanarch = lossy(arg2),
            b"-c" | b"--cachedir" => {
                cachedirs.push(PathBuf::from(OsString::from_vec(arg2.clone())))
            }
            b"-m" | b"--move" => {
                mv = true;
                movedir = arg2.clone();
            }
            b"-i" | b"--ignore" => {
                if arg2.as_slice() == b"-" {
                    if !env.stdin_is_tty() {
                        let buf = stdin_buf.get_or_insert_with(|| env.read_stdin());
                        let (chunk, rest) = match buf.find('\0') {
                            Some(p) => (buf[..p].to_string(), buf[p + 1..].to_string()),
                            None => (std::mem::take(buf), String::new()),
                        };
                        *buf = rest;
                        blacklist.extend(split_lines(&chunk));
                    }
                } else {
                    blacklist.extend(split_commas(arg2));
                }
            }
            b"-k" | b"--keep" => {
                // DIVERGENCE: the script's `keep` is `declare -i`, so `keep=$2` is
                // evaluated as bash arithmetic before the check (`-k abc` = 0,
                // `-k 010` = 8, `-k 1+1` = 2, `-k 08` prints a bash error and breaks
                // the option loop, overflow wraps modulo 2^64). We implement the
                // intent instead: one or more ASCII digits, base 10 even with leading
                // zeros (`-k 010` = 10, `-k 08` = 8); anything else (empty, blanks,
                // signs, `0x2`, expressions, names, > usize::MAX) is the die.
                keep = parse_keep(arg2).ok_or_else(|| CliError::die(MSG_K, false))?;
            }
            b"--min-atime" | b"--min-mtime" => {
                let text = lossy(arg2);
                let atime = w == b"--min-atime";
                match env.parse_date(&text) {
                    Some(v) if atime => min_atime = v,
                    Some(v) => min_mtime = v,
                    None => {
                        return Err(CliError::die(
                            if atime { MSG_ATIME } else { MSG_MTIME },
                            false,
                        ))
                    }
                }
            }
            _ => {
                takes_arg = false;
                match w {
                    b"-d" | b"--dryrun" => dryrun = true,
                    b"-f" | b"--force" => force = true,
                    b"-h" | b"--help" => return Ok(Parsed::Help(USAGE.to_string())),
                    b"--nocolor" => nocolor = true,
                    b"-z" | b"--null" => null = true,
                    b"-q" | b"--quiet" => quiet = true,
                    b"-r" | b"--remove" => delete = true,
                    b"-u" | b"--uninstalled" => {
                        let pkgs: Vec<String> = env
                            .installed_packages()
                            .unwrap_or_default()
                            .into_iter()
                            .flat_map(|l| split_lines(&l))
                            .collect();
                        if pkgs.is_empty() {
                            return Err(CliError::die(MSG_U, false));
                        }
                        blacklist.extend(pkgs);
                    }
                    b"-v" | b"--verbose" => verbose += 1,
                    b"-V" | b"--version" => return Ok(Parsed::Version(VERSION.to_string())),
                    b"--" => {
                        i += 1;
                        break;
                    }
                    _ => {} // unknown words are skipped (see module docs)
                }
            }
        }
        i += if takes_arg { 2 } else { 1 };
    }
    let whitelist: Vec<String> = optret
        .get(i..)
        .unwrap_or(&[])
        .iter()
        .map(|w| lossy(w))
        .collect();

    // `[[ -z "${cachedirs[0]}" ]]` → pacman-conf replaces the whole list.
    if cachedirs.first().is_some_and(|c| c.as_os_str().is_empty()) {
        cachedirs.clear();
    }
    if cachedirs.is_empty() {
        env.default_cachedirs_needed();
    }

    let op = match (dryrun as u8) + (delete as u8) + (mv as u8) {
        0 => return Err(CliError::die(MSG_NOOP, !nocolor)),
        1 if dryrun => Op::DryRun,
        1 if delete => Op::Remove,
        1 => Op::Move(PathBuf::from(OsString::from_vec(movedir))),
        _ => return Err(CliError::die(MSG_MANYOP, !nocolor)),
    };

    Ok(Parsed::Run(Config {
        op,
        arch: if scanarch.is_empty() {
            None
        } else {
            Some(scanarch)
        },
        cachedirs,
        force,
        blacklist,
        keep,
        min_atime: (min_atime != 0).then_some(min_atime),
        min_mtime: (min_mtime != 0).then_some(min_mtime),
        nocolor,
        null,
        quiet,
        verbose: u8::try_from(verbose).unwrap_or(u8::MAX),
        whitelist,
    }))
}

/// `date -d TEXT +%s`, as the script does.
///
/// Decision: we run GNU `date` instead of using the `parse_datetime` crate.
/// The crate (0.18, uutils) agreed with `date -d` on 94 of 96 expressions tried
/// ("30 days ago", "last week", "yesterday 12:00", "@1700000000", "x" (a military
/// time zone), "", "TZ=\"Europe/Madrid\" 2024-01-01", DST edges, garbage...) and
/// only differed for years >= 9999. But on failure the script's stderr contains
/// `date`'s own diagnostic (`date: invalid date ‘garbage’`, quoted per locale,
/// or `date: time ... is out of range`), which we would have to re-create.
/// Running `date` keeps parsing *and* diagnostics identical, costs one process
/// only when `--min-atime/--min-mtime` is used, and coreutils is already a
/// dependency of the original. `date`'s stderr is inherited, so its message is
/// printed at the same moment as in the script, just before the `die`.
pub fn system_parse_date(text: &str) -> Option<i64> {
    let out = Command::new("date")
        .arg("-d")
        .arg(text)
        .arg("+%s")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv {
        tty: bool,
        stdin: String,
        stdin_reads: usize,
        installed: Option<Vec<String>>,
        installed_calls: usize,
        dates: HashMap<String, i64>,
    }

    impl Default for FakeEnv {
        fn default() -> Self {
            let mut dates = HashMap::new();
            dates.insert("@0".to_string(), 0);
            dates.insert("@1700000000".to_string(), 1_700_000_000);
            dates.insert("@-100".to_string(), -100);
            dates.insert("30 days ago".to_string(), 1_000_000);
            FakeEnv {
                tty: true,
                stdin: String::new(),
                stdin_reads: 0,
                installed: None,
                installed_calls: 0,
                dates,
            }
        }
    }

    impl Env for FakeEnv {
        fn stdin_is_tty(&self) -> bool {
            self.tty
        }
        fn read_stdin(&mut self) -> String {
            self.stdin_reads += 1;
            std::mem::take(&mut self.stdin)
        }
        fn installed_packages(&mut self) -> Option<Vec<String>> {
            self.installed_calls += 1;
            self.installed.clone()
        }
        fn parse_date(&self, text: &str) -> Option<i64> {
            self.dates.get(text).copied()
        }
        fn progname(&self) -> String {
            "paccache".into()
        }
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn run_env(args: &[&str], env: &mut FakeEnv) -> Result<Parsed, CliError> {
        parse(&os(args), env)
    }

    fn cfg_env(args: &[&str], env: &mut FakeEnv) -> Config {
        match run_env(args, env) {
            Ok(Parsed::Run(c)) => c,
            other => panic!("{args:?}: expected Run, got {other:?}"),
        }
    }

    fn cfg(args: &[&str]) -> Config {
        cfg_env(args, &mut FakeEnv::default())
    }

    fn err(args: &[&str]) -> CliError {
        match run_env(args, &mut FakeEnv::default()) {
            Err(e) => e,
            other => panic!("{args:?}: expected error, got {other:?}"),
        }
    }

    fn stderr_of(e: &CliError) -> String {
        let mut v = Vec::new();
        e.emit_to(&mut v, false).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[derive(Debug, Clone, Copy)]
    enum Kind {
        Run,
        Fail,
        Help,
        Version,
    }
    use Kind::*;

    /// Captured from `/usr/bin/paccache` 1.13.1 (pacman 7.x parseopts, bash 5.3,
    /// en_US.UTF-8): (args, exit code, outcome, stderr). `{E}` was an empty temp
    /// dir. `date:` lines come from date(1) itself and are skipped in the
    /// comparison (system_parse_date lets date print them).
    #[rustfmt::skip]
    const ORIGINAL: &[(&[&str], i32, Kind, &str)] = &[
    (&["-d", "-c", "{E}"], 0, Run, ""),
    (&["-c", "{E}"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["-d", "-r", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-d", "-m", "{E}", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-r", "-m", "{E}", "-d", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-dvvk2", "-c", "{E}"], 0, Run, ""),
    (&["-d", "-k"], 1, Fail, "paccache: option requires an argument -- 'k'\n"),
    (&["-d", "-x"], 1, Fail, "paccache: invalid option -- 'x'\n"),
    (&["-d", "--bogus"], 1, Fail, "paccache: invalid option '--bogus'\n"),
    (&["-d", "--n"], 1, Fail, "paccache: option '--n' is ambiguous; possibilities: 'nocolor' 'null'\n"),
    (&["-d", "--min"], 1, Fail, "paccache: option '--min' is ambiguous; possibilities: 'min-atime' 'min-mtime'\n"),
    (&["-d", "--m"], 1, Fail, "paccache: option '--m' is ambiguous; possibilities: 'min-atime' 'min-mtime' 'move'\n"),
    (&["--rem", "-d", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["--dry", "-c", "{E}"], 0, Run, ""),
    (&["-d", "--dryrun=1"], 1, Fail, "paccache: option '--dryrun' does not allow an argument\n"),
    (&["-d", "--dryrun="], 1, Fail, "paccache: option '--dryrun' does not allow an argument\n"),
    (&["-d", "--keep"], 1, Fail, "paccache: option '--keep' requires an argument\n"),
    (&["-d", "-k", "-1", "-c", "{E}"], 1, Fail, "==> ERROR: argument to option -k must be a non-negative integer\n"),
    (&["-d", "-k", "-1", "-h"], 1, Fail, "==> ERROR: argument to option -k must be a non-negative integer\n"),
    (&["-h", "-k", "x"], 0, Help, ""),
    (&["-V", "-h"], 0, Version, ""),
    (&["-h", "-V"], 0, Help, ""),
    (&["-h", "--bogus"], 1, Fail, "paccache: invalid option '--bogus'\n"),
    (&["--bogus", "-h"], 1, Fail, "paccache: invalid option '--bogus'\n"),
    (&["-k", "-1", "-h"], 1, Fail, "==> ERROR: argument to option -k must be a non-negative integer\n"),
    (&["-h", "-k", "-1"], 0, Help, ""),
    (&["foo", "-d", "-c", "{E}"], 0, Run, ""),
    (&["-d", "-c", "{E}", "--", "-r"], 0, Run, ""),
    (&["-s", "-d", "-c", "{E}"], 0, Run, ""),
    (&["-d", "-*", "-r", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-d", "-?", "-r", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-d", "-:", "-r", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-d", "-\\", "-r", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    (&["-d", "-[", "-c", "{E}"], 1, Fail, "paccache: invalid option -- '['\n"),
    (&["-d", "-é", "-c", "{E}"], 1, Fail, "paccache: invalid option -- 'é'\n"),
    (&["-d", "--=x"], 1, Fail, "paccache: option '--' is ambiguous; possibilities: 'arch' 'cachedir' 'dryrun' 'force' 'help' 'ignore' 'keep' 'min-atime' 'min-mtime' 'move' 'nocolor' 'null' 'quiet' 'remove' 'uninstalled' 'verbose' 'version'\n"),
    (&["-d", "--min-atime", "garbage", "-c", "{E}"], 1, Fail, "date: invalid date ‘garbage’\n==> ERROR: argument to option --min-atime must be of the form described by 'info \"Date input formats\" '.\n"),
    (&["-d", "--min-mtime=garbage", "-c", "{E}"], 1, Fail, "date: invalid date ‘garbage’\n==> ERROR: argument to option --min-mtime must be of the form described by 'info \"Date input formats\" '.\n"),
    (&["-d", "--min-a=garbage", "-c", "{E}"], 1, Fail, "date: invalid date ‘garbage’\n==> ERROR: argument to option --min-atime must be of the form described by 'info \"Date input formats\" '.\n"),
    (&["-d", "--min-atime", "garbage", "-h"], 1, Fail, "date: invalid date ‘garbage’\n==> ERROR: argument to option --min-atime must be of the form described by 'info \"Date input formats\" '.\n"),
    (&["-h", "--min-atime", "garbage"], 0, Help, ""),
    (&["-d", "-", "-c", "{E}"], 0, Run, ""),
    (&["-d", "-k9223372036854775808", "-c", "{E}"], 1, Fail, "==> ERROR: argument to option -k must be a non-negative integer\n"),
    (&["-d", "--nocolor=x"], 1, Fail, "paccache: option '--nocolor' does not allow an argument\n"),
    (&["-d", "--version=1"], 1, Fail, "paccache: option '--version' does not allow an argument\n"),
    (&["-d", "-m"], 1, Fail, "paccache: option requires an argument -- 'm'\n"),
    (&["-c", "{E}", "-m"], 1, Fail, "paccache: option requires an argument -- 'm'\n"),
    (&["-d", "-a"], 1, Fail, "paccache: option requires an argument -- 'a'\n"),
    (&["-dk"], 1, Fail, "paccache: option requires an argument -- 'k'\n"),
    (&["-dc"], 1, Fail, "paccache: option requires an argument -- 'c'\n"),
    (&["-d", "-i"], 1, Fail, "paccache: option requires an argument -- 'i'\n"),
    (&["-d", "--ignore"], 1, Fail, "paccache: option '--ignore' requires an argument\n"),
    (&["-d", "--min-atime"], 1, Fail, "paccache: option '--min-atime' requires an argument\n"),
    (&["--arch"], 1, Fail, "paccache: option '--arch' requires an argument\n"),
    (&["-d", "--uninstalled=x"], 1, Fail, "paccache: option '--uninstalled' does not allow an argument\n"),
    (&["-d", "--cachedir"], 1, Fail, "paccache: option '--cachedir' requires an argument\n"),
    (&["--"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&[], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["-q", "-c", "{E}"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["--help"], 0, Help, ""),
    (&["--he"], 0, Help, ""),
    (&["--ver"], 1, Fail, "paccache: option '--ver' is ambiguous; possibilities: 'verbose' 'version'\n"),
    (&["--v"], 1, Fail, "paccache: option '--v' is ambiguous; possibilities: 'verbose' 'version'\n"),
    (&["--vers"], 0, Version, ""),
    (&["-V"], 0, Version, ""),
    (&["-dV"], 0, Version, ""),
    (&["-Vx"], 1, Fail, "paccache: invalid option -- 'x'\n"),
    (&["-xV"], 1, Fail, "paccache: invalid option -- 'x'\n"),
    (&["--", "-d"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["-d", "-k", "1", "-k"], 1, Fail, "paccache: option requires an argument -- 'k'\n"),
    (&["--keep=-1", "-d"], 1, Fail, "==> ERROR: argument to option -k must be a non-negative integer\n"),
    (&["-k-1"], 1, Fail, "==> ERROR: argument to option -k must be a non-negative integer\n"),
    (&["-r", "-r", "-c", "{E}"], 0, Run, ""),
    (&["-m", "{E}", "-m", "{E}", "-c", "{E}"], 0, Run, ""),
    (&["--quiet", "--q"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["--nocolor", "-c", "{E}"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["--foo=bar"], 1, Fail, "paccache: invalid option '--foo'\n"),
    (&["--for", "-d", "-c", "{E}"], 0, Run, ""),
    (&["--a", "x", "-d", "-c", "{E}"], 0, Run, ""),
    (&["--c"], 1, Fail, "paccache: option '--cachedir' requires an argument\n"),
    (&["--i"], 1, Fail, "paccache: option '--ignore' requires an argument\n"),
    (&["--k"], 1, Fail, "paccache: option '--keep' requires an argument\n"),
    (&["--r", "-c", "{E}"], 0, Run, ""),
    (&["--u"], 1, Fail, "==> ERROR: no operation specified (use -h for help)\n"),
    (&["--mo", "{E}", "--dr", "-c", "{E}"], 1, Fail, "==> ERROR: only one operation may be used at a time\n"),
    ];

    #[test]
    fn table_matches_original() {
        for (args, code, kind, stderr) in ORIGINAL {
            let args: Vec<&str> = args
                .iter()
                .map(|a| if *a == "{E}" { "/tmp/empty" } else { a })
                .collect();
            let mut env = FakeEnv {
                installed: Some(vec!["bash".into()]),
                ..FakeEnv::default()
            };
            let got = run_env(&args, &mut env);
            let expected_err: String = stderr
                .split_inclusive('\n')
                .filter(|l| !l.starts_with("date: "))
                .collect();
            match (kind, &got) {
                (Run, Ok(Parsed::Run(_))) => assert_eq!(*code, 0),
                (Help, Ok(Parsed::Help(h))) => assert_eq!(h, USAGE),
                (Version, Ok(Parsed::Version(v))) => assert_eq!(v, VERSION),
                (Fail, Err(e)) => {
                    assert_eq!(e.code, *code, "{args:?}");
                    assert_eq!(stderr_of(e), expected_err, "{args:?}");
                }
                _ => panic!("{args:?}: expected {kind:?}, got {got:?}"),
            }
        }
    }

    #[test]
    fn usage_is_byte_identical_to_original() {
        let Ok(o) = Command::new("/usr/bin/paccache").arg("-h").output() else {
            return; // original not installed
        };
        assert_eq!(String::from_utf8(o.stdout).unwrap(), USAGE);
    }

    #[test]
    fn grouped_and_separate_args() {
        let c = cfg(&["-dvvk2", "-c", "/x"]);
        assert_eq!((c.op.clone(), c.verbose, c.keep), (Op::DryRun, 2, 2));
        assert_eq!(c.cachedirs, vec![PathBuf::from("/x")]);
        assert_eq!(cfg(&["-d", "-k", "2"]).keep, 2);
        assert_eq!(cfg(&["-d", "-k2"]).keep, 2);
        assert_eq!(cfg(&["-d", "--keep=2"]).keep, 2);
        assert_eq!(cfg(&["-d", "--keep", "2"]).keep, 2);
        assert_eq!(cfg(&["-d", "--ke", "1"]).keep, 1);
        assert_eq!(cfg(&["-d"]).keep, 3);
        assert_eq!(cfg(&["-dvc/y"]).cachedirs, vec![PathBuf::from("/y")]);
        // option with argument in the middle of a group takes the rest
        assert_eq!(cfg(&["-dak1"]).arch.as_deref(), Some("k1"));
        assert_eq!(cfg(&["-ax86_64", "-d"]).arch.as_deref(), Some("x86_64"));
        assert_eq!(cfg(&["--arch=any", "-d"]).arch.as_deref(), Some("any"));
        assert_eq!(cfg(&["-a", "", "-d"]).arch, None);
        assert_eq!(cfg(&["-d", "-k", "1", "-k", "4"]).keep, 4);
    }

    #[test]
    fn long_option_equals_splitting() {
        // IFS='=' read: one trailing '=' is dropped, more are kept
        assert_eq!(cfg(&["-d", "--arch=x="]).arch.as_deref(), Some("x"));
        assert_eq!(cfg(&["-d", "--arch=x=="]).arch.as_deref(), Some("x=="));
        assert_eq!(cfg(&["-d", "--arch=a=b"]).arch.as_deref(), Some("a=b"));
        assert_eq!(cfg(&["-d", "--arch=a\nb"]).arch.as_deref(), Some("a"));
        assert_eq!(
            cfg(&["-d", "--cachedir= /s "]).cachedirs,
            vec![PathBuf::from(" /s ")]
        );
    }

    #[test]
    fn keep_policy() {
        // DIVERGENCE: base 10, digits only
        assert_eq!(cfg(&["-d", "-k", "010"]).keep, 10);
        assert_eq!(cfg(&["-d", "-k", "08"]).keep, 8);
        assert_eq!(cfg(&["-d", "-k", "007"]).keep, 7);
        assert_eq!(cfg(&["-d", "-k", "0"]).keep, 0);
        assert_eq!(
            cfg(&["-d", "-k", "9223372036854775807"]).keep,
            9223372036854775807
        );
        for bad in [
            "",
            " 2",
            "2 ",
            "+1",
            "-1",
            "0x2",
            "1+1",
            "abc",
            "2#11",
            "9z",
            "1.5",
            "99999999999999999999",
            "18446744073709551617",
            "9223372036854775808",
        ] {
            let e = err(&["-d", "-k", bad]);
            assert_eq!(e.message, MSG_K, "{bad:?}");
            assert!(e.is_die && !e.colorable && e.code == 1);
        }
        assert_eq!(err(&["-d", "--keep="]).message, MSG_K);
        // the die happens in the loop, before a later -h and before the op check
        assert_eq!(err(&["-k", "x", "-h"]).message, MSG_K);
        assert_eq!(err(&["-k", "x"]).message, MSG_K);
        assert!(matches!(
            run_env(&["-h", "-k", "x"], &mut FakeEnv::default()),
            Ok(Parsed::Help(_))
        ));
    }

    #[test]
    fn operations() {
        assert_eq!(cfg(&["-r"]).op, Op::Remove);
        assert_eq!(cfg(&["--remove"]).op, Op::Remove);
        assert_eq!(cfg(&["-m", "dest"]).op, Op::Move("dest".into()));
        assert_eq!(cfg(&["--move=/a b"]).op, Op::Move("/a b".into()));
        assert_eq!(cfg(&["-m", "a", "-m", "b"]).op, Op::Move("b".into()));
        assert_eq!(cfg(&["-d", "-d"]).op, Op::DryRun);
        let e = err(&[]);
        assert_eq!((e.message.as_str(), e.colorable), (MSG_NOOP, true));
        let e = err(&["-d", "-r", "-m", "x"]);
        assert_eq!((e.message.as_str(), e.colorable), (MSG_MANYOP, true));
    }

    #[test]
    fn positionals_and_double_dash() {
        let c = cfg(&["foo", "-d", "bar", "--", "-r", "baz"]);
        assert_eq!(c.op, Op::DryRun);
        assert_eq!(c.whitelist, vec!["foo", "bar", "-r", "baz"]);
        assert_eq!(cfg(&["-d", "-"]).whitelist, vec!["-"]);
        assert_eq!(cfg(&["-d", ""]).whitelist, vec![""]);
        assert_eq!(cfg(&["-d", "my pkg"]).whitelist, vec!["my pkg"]);
        assert!(cfg(&["-d"]).whitelist.is_empty());
        // an option's argument can be "--"
        assert_eq!(
            cfg(&["-d", "-c", "--", "x"]).cachedirs,
            vec![PathBuf::from("--")]
        );
        assert_eq!(cfg(&["-d", "-c", "--", "x"]).whitelist, vec!["x"]);
    }

    #[test]
    fn glob_quirks_of_short_options() {
        // `-\` (like `-*`, `-?`) takes an argument which the loop then re-reads
        // "-v" is swallowed as its argument, but the loop does not know that and re-reads it
        assert_eq!(cfg(&["-d", "-\\", "-v"]).verbose, 1);
        assert_eq!(cfg(&["-d", "-\\", "x", "-v"]).verbose, 1);
        // `-\ -k 2`: "-k" is swallowed, then re-read by the loop with "--" as value
        assert_eq!(err(&["-d", "-\\", "-k", "2"]).message, MSG_K);
        assert_eq!(cfg(&["-d", "-*", "-v"]).verbose, 1);
        assert_eq!(cfg(&["-d", "-*v"]).verbose, 0); // "v" is just a word
        assert_eq!(cfg(&["-d", "-:"]).op, Op::DryRun);
        assert_eq!(
            err(&["-d", "-?"]).message,
            "paccache: option requires an argument -- '?'"
        );
        // invalid UTF-8 bytes are single characters
        let args = vec![OsString::from("-d"), OsString::from_vec(vec![b'-', 0xff])];
        let e = parse(&args, &mut FakeEnv::default()).unwrap_err();
        assert_eq!(e.message, "paccache: invalid option -- '\u{fffd}'");
    }

    #[test]
    fn flags() {
        let c = cfg(&["-dfzqs", "--nocolor", "-v", "--verbose", "-vv"]);
        assert!(c.force && c.null && c.quiet && c.nocolor);
        assert_eq!(c.verbose, 4);
        let c = cfg(&["-d", "--force", "--null", "--quiet"]);
        assert!(c.force && c.null && c.quiet && !c.nocolor);
        let c = cfg(&["-d"]);
        assert!(!c.force && !c.null && !c.quiet && !c.nocolor && c.verbose == 0);
    }

    #[test]
    fn cachedirs_first_empty_means_pacman_conf() {
        assert!(cfg(&["-d", "-c", "", "-c", "/x"]).cachedirs.is_empty());
        assert_eq!(
            cfg(&["-d", "-c", "/x", "-c", ""]).cachedirs,
            vec![PathBuf::from("/x"), PathBuf::new()]
        );
        let args = vec![
            OsString::from("-dc"),
            OsString::from_vec(b"/p\xff".to_vec()),
        ];
        let Ok(Parsed::Run(c)) = parse(&args, &mut FakeEnv::default()) else {
            panic!()
        };
        assert_eq!(c.cachedirs[0].as_os_str().as_bytes(), b"/p\xff");
    }

    #[test]
    fn ignore_commas() {
        assert_eq!(
            cfg(&["-d", "-i", "linux,vim"]).blacklist,
            vec!["linux", "vim"]
        );
        assert_eq!(
            cfg(&["-d", "-i", ",linux,,"]).blacklist,
            vec!["", "linux", ""]
        );
        assert_eq!(cfg(&["-d", "-i", "a,"]).blacklist, vec!["a"]);
        assert_eq!(cfg(&["-d", "-i", ","]).blacklist, vec![""]);
        assert!(cfg(&["-d", "-i", ""]).blacklist.is_empty());
        assert_eq!(cfg(&["-d", "-i", "a\nb,c"]).blacklist, vec!["a"]);
        assert_eq!(
            cfg(&["-d", "-i", " my pkg ,x"]).blacklist,
            vec![" my pkg ", "x"]
        );
        assert_eq!(
            cfg(&["-d", "-i", "a", "--ignore=b,c", "--ig", "d"]).blacklist,
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn ignore_stdin() {
        let mut env = FakeEnv {
            tty: false,
            stdin: "linux\n\nvim,x\n my pkg \n".into(),
            ..FakeEnv::default()
        };
        assert_eq!(
            cfg_env(&["-d", "-i", "-"], &mut env).blacklist,
            vec!["linux", "vim,x", " my pkg "]
        );
        // tty: nothing is read
        let mut env = FakeEnv {
            tty: true,
            stdin: "linux\n".into(),
            ..FakeEnv::default()
        };
        assert!(cfg_env(&["-d", "-i", "-"], &mut env).blacklist.is_empty());
        assert_eq!(env.stdin_reads, 0);
        // second `-i -` finds stdin exhausted
        let mut env = FakeEnv {
            tty: false,
            stdin: "a\nb".into(),
            ..FakeEnv::default()
        };
        assert_eq!(
            cfg_env(&["-d", "-i", "-", "-i", "c", "-i", "-"], &mut env).blacklist,
            vec!["a", "b", "c"]
        );
        assert_eq!(env.stdin_reads, 1);
        // `read -d ''` stops at NUL; the next `-i -` continues after it
        let mut env = FakeEnv {
            tty: false,
            stdin: "a\0b\nc\n".into(),
            ..FakeEnv::default()
        };
        assert_eq!(cfg_env(&["-d", "-i", "-"], &mut env).blacklist, vec!["a"]);
        let mut env = FakeEnv {
            tty: false,
            stdin: "a\0b\nc\n".into(),
            ..FakeEnv::default()
        };
        assert_eq!(
            cfg_env(&["-d", "-i", "-", "-i", "-"], &mut env).blacklist,
            vec!["a", "b", "c"]
        );
        let mut env = FakeEnv {
            tty: false,
            ..FakeEnv::default()
        };
        assert!(cfg_env(&["-d", "-i", "-"], &mut env).blacklist.is_empty());
    }

    #[test]
    fn uninstalled() {
        let mut env = FakeEnv {
            installed: Some(vec!["bash".into(), "".into(), "zsh".into()]),
            ..FakeEnv::default()
        };
        assert_eq!(
            cfg_env(&["-d", "-i", "x", "-u", "-i", "y"], &mut env).blacklist,
            vec!["x", "bash", "zsh", "y"]
        );
        for installed in [None, Some(vec![]), Some(vec![String::new()])] {
            let mut env = FakeEnv {
                installed,
                ..FakeEnv::default()
            };
            let e = run_env(&["-d", "-u"], &mut env).unwrap_err();
            assert_eq!(
                (e.message.as_str(), e.is_die, e.colorable),
                (MSG_U, true, false)
            );
            assert_eq!(stderr_of(&e), format!("==> ERROR: {MSG_U}\n"));
        }
        // executed when reached: before -h it dies, after -h it is never run
        let mut env = FakeEnv::default();
        assert!(run_env(&["-u", "-h"], &mut env).is_err());
        let mut env = FakeEnv::default();
        assert!(matches!(
            run_env(&["-h", "-u"], &mut env),
            Ok(Parsed::Help(_))
        ));
        assert_eq!(env.installed_calls, 0);
        // and before the operation check
        let mut env = FakeEnv::default();
        assert_eq!(run_env(&["-u"], &mut env).unwrap_err().message, MSG_U);
    }

    #[test]
    fn min_times() {
        let c = cfg(&[
            "-d",
            "--min-atime",
            "@1700000000",
            "--min-mtime=30 days ago",
        ]);
        assert_eq!(
            (c.min_atime, c.min_mtime),
            (Some(1_700_000_000), Some(1_000_000))
        );
        assert_eq!(cfg(&["-d", "--min-a", "@-100"]).min_atime, Some(-100));
        // 0 means "unset" in the script
        assert_eq!(cfg(&["-d", "--min-atime", "@0"]).min_atime, None);
        assert_eq!(
            cfg(&["-d", "--min-atime", "@1700000000", "--min-atime", "@0"]).min_atime,
            None
        );
        let e = err(&["-d", "--min-atime", "garbage", "--min-mtime", "garbage"]);
        assert_eq!((e.message.as_str(), e.colorable), (MSG_ATIME, false));
        assert_eq!(err(&["-d", "--min-mtime", "nope"]).message, MSG_MTIME);
        assert_eq!(err(&["--min-mtime", "nope", "-h"]).message, MSG_MTIME);
    }

    #[test]
    fn help_and_version_order() {
        let mut e = FakeEnv::default();
        assert_eq!(
            run_env(&["-V", "-h"], &mut e).unwrap(),
            Parsed::Version(VERSION.into())
        );
        assert_eq!(
            run_env(&["-h", "-V"], &mut e).unwrap(),
            Parsed::Help(USAGE.into())
        );
        assert_eq!(
            run_env(&["-dhr", "-k", "x"], &mut e).unwrap(),
            Parsed::Help(USAGE.into())
        );
        // parseopts errors win over everything
        assert!(run_env(&["-h", "--bogus"], &mut e).is_err());
        assert!(VERSION.starts_with(concat!(
            "paccache-rs ",
            env!("CARGO_PKG_VERSION"),
            " (compatible with paccache 1.13.1)\n"
        )));
    }

    #[test]
    fn colored_die() {
        let e = err(&["-r", "-d"]);
        let mut v = Vec::new();
        e.emit_to(&mut v, true).unwrap();
        let s = String::from_utf8(v).unwrap();
        assert!(
            s.starts_with('\x1b') && s.contains("==> ERROR:") && s.contains(MSG_MANYOP),
            "{s:?}"
        );
        // loop dies are printed before colorize: never colored
        let e = err(&["-d", "-k", "x"]);
        let mut v = Vec::new();
        e.emit_to(&mut v, true).unwrap();
        assert_eq!(
            String::from_utf8(v).unwrap(),
            format!("==> ERROR: {MSG_K}\n")
        );
    }

    #[test]
    fn system_date_matches_date_command() {
        if Command::new("date").arg("--version").output().is_err() {
            return;
        }
        assert_eq!(system_parse_date("@1700000000"), Some(1_700_000_000));
        assert_eq!(system_parse_date("@0"), Some(0));
        assert_eq!(
            system_parse_date("2024-01-01 00:00 UTC"),
            Some(1_704_067_200)
        );
        assert!(system_parse_date("-3 days").is_some());
        assert_eq!(system_parse_date("garbage"), None);
    }
}
