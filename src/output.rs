//! libmakepkg-style messages and paccache's summary (owner: agent B).
//!
//! Colors: when `Out::color` is set, the escape sequences are obtained once per
//! process the same way libmakepkg's `colorize` does: if `tput setaf 0` succeeds
//! (it depends on `$TERM`) we use `tput sgr0`, `tput bold`, `tput setaf 2/1`
//! (for `xterm-256color` that is `\e(B\e[m`, `\e[1m`, `\e[32m`, `\e[31m`);
//! otherwise the fallback `\e[0m`, `\e[1m`, `\e[32m`, `\e[31m`. Nothing is
//! spawned when color is off.
//!
//! `msg`/`error` in message.sh use the message as a printf *format*. Every call in
//! paccache passes a constant format (dynamic parts are `%s` arguments), so
//! [`Out::msg`]/[`Out::error`] take the final text and write it literally. The one
//! exception is the summary line, whose format embeds the `-m` directory; see the
//! DIVERGENCE note in [`summarize_to`].
use crate::actions::Op;
use std::cmp::Ordering;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub struct Out {
    /// Colors on: stderr is a tty and no --nocolor (applies to stdout msg too).
    pub color: bool,
    pub quiet: bool,
}

/// The `ALL_OFF`/`BOLD`/`GREEN`/`RED` variables of message.sh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Palette {
    pub all_off: Vec<u8>,
    pub bold: Vec<u8>,
    /// `${BOLD}$(tput setaf 2)`
    pub green: Vec<u8>,
    /// `${BOLD}$(tput setaf 1)`
    pub red: Vec<u8>,
}

fn tput(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("tput")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut v = out.stdout;
    // command substitution strips trailing newlines
    while v.last() == Some(&b'\n') {
        v.pop();
    }
    Some(v)
}

impl Palette {
    /// Colors unset (`unset ALL_OFF BOLD ...`).
    pub fn none() -> &'static Palette {
        static NONE: Palette = Palette {
            all_off: Vec::new(),
            bold: Vec::new(),
            green: Vec::new(),
            red: Vec::new(),
        };
        &NONE
    }

    /// colorize's fallback when tput is unusable.
    pub fn ansi() -> Palette {
        Palette {
            all_off: b"\x1b[0m".to_vec(),
            bold: b"\x1b[1m".to_vec(),
            green: b"\x1b[1m\x1b[32m".to_vec(),
            red: b"\x1b[1m\x1b[31m".to_vec(),
        }
    }

    /// colorize's tput branch, `None` if `tput setaf 0` fails or tput is missing.
    pub fn from_tput() -> Option<Palette> {
        tput(&["setaf", "0"])?;
        let all_off = tput(&["sgr0"]).unwrap_or_default();
        let bold = tput(&["bold"]).unwrap_or_default();
        let mut green = bold.clone();
        green.extend(tput(&["setaf", "2"]).unwrap_or_default());
        let mut red = bold.clone();
        red.extend(tput(&["setaf", "1"]).unwrap_or_default());
        Some(Palette {
            all_off,
            bold,
            green,
            red,
        })
    }

    /// `colorize`, evaluated once per process.
    pub fn detect() -> &'static Palette {
        static P: OnceLock<Palette> = OnceLock::new();
        P.get_or_init(|| Palette::from_tput().unwrap_or_else(Palette::ansi))
    }
}

/// `msg` with a literal text: `${GREEN}==>${ALL_OFF}${BOLD} text${ALL_OFF}\n`.
pub fn write_msg(w: &mut dyn Write, pal: &Palette, text: &str) -> io::Result<()> {
    write_msg_bytes(w, pal, text.as_bytes())
}

fn write_msg_bytes(w: &mut dyn Write, pal: &Palette, text: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(text.len() + 32);
    b.extend_from_slice(&pal.green);
    b.extend_from_slice(b"==>");
    b.extend_from_slice(&pal.all_off);
    b.extend_from_slice(&pal.bold);
    b.push(b' ');
    b.extend_from_slice(text);
    b.extend_from_slice(&pal.all_off);
    b.push(b'\n');
    w.write_all(&b)
}

/// `error` with a literal text: `${RED}==> ERROR:${ALL_OFF}${BOLD} text${ALL_OFF}\n`.
pub fn write_error(w: &mut dyn Write, pal: &Palette, text: &str) -> io::Result<()> {
    let mut b = Vec::with_capacity(text.len() + 40);
    b.extend_from_slice(&pal.red);
    b.extend_from_slice(b"==> ERROR:");
    b.extend_from_slice(&pal.all_off);
    b.extend_from_slice(&pal.bold);
    b.push(b' ');
    b.extend_from_slice(text.as_bytes());
    b.extend_from_slice(&pal.all_off);
    b.push(b'\n');
    w.write_all(&b)
}

impl Out {
    /// Palette in effect (detected lazily, only when color is on).
    pub fn palette(&self) -> &'static Palette {
        if self.color {
            Palette::detect()
        } else {
            Palette::none()
        }
    }

    /// `==> text` on stdout (suppressed by quiet).
    pub fn msg(&self, text: &str) {
        if self.quiet {
            return;
        }
        let mut o = io::stdout().lock();
        let _ = write_msg(&mut o, self.palette(), text);
        let _ = o.flush();
    }
    /// `==> ERROR: text` on stderr (never suppressed).
    pub fn error(&self, text: &str) {
        let mut e = io::stderr().lock();
        let _ = write_error(&mut e, self.palette(), text);
    }
}

/// awk size_to_human.
pub fn size_to_human(bytes: u64) -> String {
    const SUFFIX: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut size = bytes as f64;
    let mut count = 0;
    while size > 1024.0 {
        size /= 1024.0;
        count += 1;
    }
    // sprintf("%.2f") then sub(/\.?0+$/, ""): there is always a '.', so this
    // only strips trailing zeros of the fraction (and the '.' if all were zero).
    let mut s = format!("{size:.2}");
    let t = s.trim_end_matches('0').len();
    s.truncate(t);
    if s.ends_with('.') {
        s.pop();
    }
    format!("{} {}", s, SUFFIX.get(count).copied().unwrap_or(""))
}

/// `[[ $pkg =~ (.+)-[^-]+-[0-9]+-([^.]+)\.pkg.* ]]` → (BASH_REMATCH[1], [2]).
/// POSIX leftmost-longest: the match always spans the whole string and group 1
/// is as long as possible.
fn pkg_re(s: &[u8]) -> Option<(&[u8], &[u8])> {
    let tail = |i: usize| -> Option<&[u8]> {
        // s[i] == '-'; then [^-]+ '-' [0-9]+ '-' ([^.]+) '\.pkg'
        let j = i + 1 + s[i + 1..].iter().position(|&b| b == b'-')?;
        if j == i + 1 {
            return None;
        }
        let k = j + 1 + s[j + 1..].iter().position(|&b| !b.is_ascii_digit())?;
        if k == j + 1 || s[k] != b'-' {
            return None;
        }
        let m = k + 1 + s[k + 1..].iter().position(|&b| b == b'.')?;
        if m == k + 1 || !s[m..].starts_with(b".pkg") {
            return None;
        }
        Some(&s[k + 1..m])
    };
    (1..s.len())
        .rev()
        .filter(|&i| s[i] == b'-')
        .find_map(|i| tail(i).map(|arch| (&s[..i], arch)))
}

fn after_last_slash(s: &[u8]) -> &[u8] {
    match s.iter().rposition(|&b| b == b'/') {
        Some(p) => &s[p + 1..],
        None => s,
    }
}

/// The script's `summarize` writing to `w`. `cmp` orders the candidate list
/// like `pacsort --files`; `pal` is the palette in effect.
#[allow(clippy::too_many_arguments)]
pub fn summarize_to(
    w: &mut dyn Write,
    pal: &Palette,
    quiet: bool,
    op: &Op,
    verbose: u8,
    null: bool,
    pkgcount: usize,
    pkgs: &[PathBuf],
    totalsaved: u64,
    cmp: &dyn Fn(&Path, &Path) -> Ordering,
) -> io::Result<()> {
    if quiet {
        return Ok(());
    }
    let mut output: Vec<u8> = Vec::new();
    match op {
        Op::Remove => output.extend(format!("finished: {pkgcount} packages removed").bytes()),
        Op::Move(dir) => {
            output.extend(format!("finished: {pkgcount} packages moved to '").bytes());
            output.extend_from_slice(dir.as_os_str().as_bytes());
            output.push(b'\'');
        }
        Op::DryRun => {
            if verbose > 0 {
                write_msg(w, pal, "Candidate packages:")?;
                let mut sorted: Vec<&PathBuf> = pkgs.iter().collect();
                sorted.sort_by(|a, b| cmp(a, b));
                let delim: &[u8] = if null { b"\0" } else { b"\n" };
                let (mut seen, mut seenarch, mut name, mut arch): (
                    Vec<u8>,
                    Vec<u8>,
                    Vec<u8>,
                    Vec<u8>,
                ) = Default::default();
                for p in sorted {
                    // `read -r pkg` strips leading/trailing IFS whitespace
                    let pkg = p.as_os_str().as_bytes();
                    let pkg = trim_blanks(pkg);
                    if verbose >= 3 {
                        if let Some((n, a)) = pkg_re(pkg) {
                            name = n.to_vec();
                            arch = a.to_vec();
                        }
                        if seen.is_empty() || seenarch != arch || seen != name {
                            seen.clone_from(&name);
                            seenarch.clone_from(&arch);
                            let mut l = after_last_slash(&name).to_vec();
                            l.extend_from_slice(b" (");
                            l.extend_from_slice(&arch);
                            l.extend_from_slice(b"):\n");
                            w.write_all(&l)?;
                        }
                        let mut l = b"  ".to_vec();
                        l.extend_from_slice(after_last_slash(pkg));
                        l.push(b'\n');
                        w.write_all(&l)?;
                    } else if verbose >= 2 {
                        w.write_all(pkg)?;
                        w.write_all(delim)?;
                    } else {
                        w.write_all(after_last_slash(pkg))?;
                        w.write_all(delim)?;
                    }
                }
            }
            output.extend(format!("finished dry run: {pkgcount} candidates").bytes());
        }
    }
    w.write_all(b"\n")?;
    // msg "$output (disk space saved: %s)" "$(size_to_human "$totalsaved")"
    // DIVERGENCE: msg uses "$output" (which embeds the -m directory) as a printf
    // format, so a `%` or `\` in the directory gets mangled in the original. We
    // print the directory literally.
    output.extend_from_slice(b" (disk space saved: ");
    output.extend_from_slice(size_to_human(totalsaved).as_bytes());
    output.push(b')');
    write_msg_bytes(w, pal, &output)?;
    w.flush()
}

fn trim_blanks(s: &[u8]) -> &[u8] {
    let is = |b: &u8| matches!(b, b' ' | b'\t' | b'\n');
    let start = s.iter().position(|b| !is(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !is(b)).map_or(start, |e| e + 1);
    &s[start..end]
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
    if out.quiet {
        return;
    }
    let mut o = io::stdout().lock();
    let _ = summarize_to(
        &mut o,
        out.palette(),
        out.quiet,
        op,
        verbose,
        null,
        pkgcount,
        pkgs,
        totalsaved,
        &crate::pkgname::pacsort_files_cmp,
    );
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
mod tests {
    use super::*;
    use std::path::Path;

    const SCRIPT: &str = "/usr/bin/paccache";
    const MESSAGE_SH: &str = "/usr/share/makepkg/util/message.sh";

    fn have_original() -> bool {
        Path::new(SCRIPT).exists() && Path::new(MESSAGE_SH).exists()
    }

    fn xterm() -> Palette {
        Palette {
            all_off: b"\x1b(B\x1b[m".to_vec(),
            bold: b"\x1b[1m".to_vec(),
            green: b"\x1b[1m\x1b[32m".to_vec(),
            red: b"\x1b[1m\x1b[31m".to_vec(),
        }
    }

    /// Run bash with message.sh sourced and `body` evaluated; returns
    /// (stdout, stderr). `color`: Some(TERM) runs colorize with that TERM, and
    /// Some("") runs it with a failing tput (fallback branch).
    fn bash(
        body: &str,
        color: Option<&str>,
        args: &[&[u8]],
        env: &[(&str, &[u8])],
    ) -> (Vec<u8>, Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        let colorize = match color {
            None => "",
            Some("") => "tput() { return 127; }; colorize",
            Some(_) => "colorize",
        };
        let script = format!(
            "source {MESSAGE_SH}\neval \"$(/usr/bin/sed -n '/^size_to_human()/,/^}}/p;/^summarize()/,/^}}/p' {SCRIPT})\"\npacsort() {{ /usr/bin/cat; }}\n{colorize}\n{body}"
        );
        let mut cmd = Command::new("/usr/bin/bash");
        cmd.arg("-c").arg(script).arg("bash");
        for a in args {
            cmd.arg(std::ffi::OsStr::from_bytes(a));
        }
        cmd.env(
            "TERM",
            color.filter(|t| !t.is_empty()).unwrap_or("xterm-256color"),
        );
        for (k, v) in env {
            cmd.env(k, std::ffi::OsStr::from_bytes(v));
        }
        let o = cmd.output().unwrap();
        (o.stdout, o.stderr)
    }

    #[test]
    fn msg_and_error_format() {
        let mut v = Vec::new();
        write_msg(&mut v, Palette::none(), "hello").unwrap();
        assert_eq!(v, b"==> hello\n");
        let mut v = Vec::new();
        write_error(&mut v, Palette::none(), "bad '%s' \\n").unwrap();
        assert_eq!(v, b"==> ERROR: bad '%s' \\n\n");
        let mut v = Vec::new();
        write_msg(&mut v, &xterm(), "hi").unwrap();
        assert_eq!(v, b"\x1b[1m\x1b[32m==>\x1b(B\x1b[m\x1b[1m hi\x1b(B\x1b[m\n");
        assert_eq!(Palette::ansi().green, b"\x1b[1m\x1b[32m");
    }

    #[test]
    fn msg_and_error_match_message_sh() {
        if !have_original() {
            return;
        }
        let text = "cachedir '/tmp/x y' does not exist or is not a directory";
        for (color, pal) in [
            (None, Palette::none().clone()),
            (Some("xterm-256color"), xterm()),
            (Some("linux"), {
                let mut p = xterm();
                p.all_off = b"\x1b[m\x0f".to_vec();
                p
            }),
            (Some(""), Palette::ansi()),
        ] {
            let (o, e) = bash(r#"msg "$1"; error "$1""#, color, &[text.as_bytes()], &[]);
            let mut mo = Vec::new();
            write_msg(&mut mo, &pal, text).unwrap();
            let mut me = Vec::new();
            write_error(&mut me, &pal, text).unwrap();
            assert_eq!(o, mo, "{color:?}");
            assert_eq!(e, me, "{color:?}");
        }
    }

    fn values() -> Vec<u64> {
        let mut v: Vec<u64> = vec![
            0,
            1,
            2,
            9,
            10,
            99,
            100,
            512,
            999,
            1000,
            1023,
            1024,
            1025,
            1030,
            1035,
            1100,
            1126,
            1127,
            1152,
            1536,
            2047,
            2048,
            2049,
            10240,
            10_000,
            100_000,
            1_000_000,
            1_048_575,
            1_048_576,
            1_048_577,
            1_073_741_824,
            1_099_511_627_776,
            1 << 50,
            1 << 60,
            1 << 63,
            123_456_789,
            36_787_456_123,
            u64::MAX,
            u64::MAX - 1,
            9_007_199_254_740_993,
        ];
        for k in 0..64 {
            v.push(1u64 << k);
            v.push((1u64 << k).wrapping_sub(1));
            v.push((1u64 << k) + (1u64 << k) / 3);
        }
        for p in 1..8u32 {
            for f in [
                1.005f64, 1.125, 1.375, 9.995, 10.0, 10.005, 99.995, 100.5, 1023.995, 1023.999,
            ] {
                v.push((f * 1024f64.powi(p as i32)) as u64);
            }
        }
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..300 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push(x >> (x % 60));
        }
        v
    }

    #[test]
    fn size_to_human_examples() {
        assert_eq!(size_to_human(0), "0 B");
        assert_eq!(size_to_human(1024), "1024 B");
        assert_eq!(size_to_human(1025), "1 KiB");
        assert_eq!(size_to_human(1536), "1.5 KiB");
        assert_eq!(size_to_human(10240), "10 KiB");
        assert_eq!(size_to_human(1_048_575), "1024 KiB");
        assert_eq!(size_to_human(1152), "1.12 KiB"); // 1.125 exact tie: half-to-even like printf
        assert_eq!(size_to_human(u64::MAX), "16 EiB");
    }

    #[test]
    fn size_to_human_matches_awk() {
        if !have_original() {
            return;
        }
        let vals = values();
        let args: Vec<Vec<u8>> = vals.iter().map(|v| v.to_string().into_bytes()).collect();
        let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        let (o, _) = bash(
            r#"for s in "$@"; do size_to_human "$s"; echo; done"#,
            None,
            &refs,
            &[],
        );
        let got: Vec<String> = vals.iter().map(|&v| size_to_human(v)).collect();
        let want: Vec<String> = String::from_utf8(o)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(want.len(), vals.len());
        for ((v, g), w) in vals.iter().zip(&got).zip(&want) {
            assert_eq!(g, w, "size {v}");
        }
    }

    #[test]
    fn pkg_re_examples() {
        let m = |s: &str| {
            pkg_re(s.as_bytes()).map(|(a, b)| {
                (
                    String::from_utf8_lossy(a).into_owned(),
                    String::from_utf8_lossy(b).into_owned(),
                )
            })
        };
        assert_eq!(
            m("/c/linux-6.1-1-x86_64.pkg.tar.zst"),
            Some(("/c/linux".into(), "x86_64".into()))
        );
        assert_eq!(
            m("a-b-1:2-3-any.pkg.tar.xz.sig"),
            Some(("a-b".into(), "any".into()))
        );
        assert_eq!(
            m("a-1-1-x-1-1-y.pkg.tar"),
            Some(("a-1-1-x".into(), "y".into()))
        );
        assert_eq!(m("foo-1-x-any.pkg.tar"), None);
        assert_eq!(m("foo.pkg.tar"), None);
    }

    fn summarize_case(
        op: &Op,
        verbose: u8,
        null: bool,
        quiet: bool,
        pkgcount: usize,
        pkgs: &[&str],
        total: u64,
        pal: &Palette,
    ) -> Vec<u8> {
        let paths: Vec<PathBuf> = pkgs.iter().map(PathBuf::from).collect();
        let mut v = Vec::new();
        summarize_to(
            &mut v,
            pal,
            quiet,
            op,
            verbose,
            null,
            pkgcount,
            &paths,
            total,
            &|_, _| Ordering::Equal,
        )
        .unwrap();
        v
    }

    fn original_summarize(
        op: &Op,
        verbose: u8,
        null: bool,
        quiet: bool,
        pkgcount: usize,
        pkgs: &[&str],
        total: u64,
        color: Option<&str>,
    ) -> Vec<u8> {
        let (d, r, m, md): (u8, u8, u8, Vec<u8>) = match op {
            Op::DryRun => (1, 0, 0, Vec::new()),
            Op::Remove => (0, 1, 0, Vec::new()),
            Op::Move(p) => (0, 0, 1, p.as_os_str().as_bytes().to_vec()),
        };
        let body = format!(
            "declare -i delete={r} dryrun={d} move={m} verbose={verbose} QUIET={} totalsaved={total}\nmovedir=$MD\n{}\nsummarize \"$@\"",
            quiet as u8,
            if null { "delim='\\0'" } else { "delim=$'\\n'" }
        );
        let count = pkgcount.to_string();
        let mut args: Vec<&[u8]> = vec![count.as_bytes()];
        args.extend(pkgs.iter().map(|p| p.as_bytes()));
        bash(&body, color, &args, &[("MD", &md)]).0
    }

    #[test]
    fn summarize_matches_script() {
        if !have_original() {
            return;
        }
        let pkgs: &[&str] = &[
            "/var/cache/pacman/pkg/firefox-120.0-1-x86_64.pkg.tar.zst",
            "/var/cache/pacman/pkg/firefox-120.0-1-x86_64.pkg.tar.zst.sig",
            "/var/cache/pacman/pkg/firefox-121.0-1-x86_64.pkg.tar.zst",
            "/var/cache/pacman/pkg/lib32-glibc-2.38-1-x86_64.pkg.tar.zst",
            "/var/cache/pacman/pkg/linux-6.1-1-x86_64.pkg.tar.zst",
            "/var/cache/pacman/pkg/linux-6.1-1-i686.pkg.tar.zst",
            "/var/cache/pacman/pkg/linux-6.2-1-x86_64.pkg.tar.zst",
            "/other/dir/my pkg-1-1-any.pkg.tar.zst",
            "/var/cache/pacman/pkg/weird.pkg.tar.zst",
            "/var/cache/pacman/pkg/vim-9.0-1-x86_64.pkg.tar.zst",
        ];
        let weird_first: &[&str] = &[
            "/c/weird.pkg.tar",
            "/c/odd-name.pkg.tar.zst",
            "/c/a-1-1-any.pkg.tar",
        ];
        let ops = [
            Op::DryRun,
            Op::Remove,
            Op::Move("/tmp/dest".into()),
            Op::Move("/tmp/dest dir".into()),
        ];
        for op in &ops {
            for verbose in 0..=4u8 {
                for null in [false, true] {
                    for quiet in [false, true] {
                        for list in [pkgs, weird_first] {
                            for total in [0u64, 1536, 36_787_456_123] {
                                let want = original_summarize(
                                    op, verbose, null, quiet, 7, list, total, None,
                                );
                                let got = summarize_case(
                                    op,
                                    verbose,
                                    null,
                                    quiet,
                                    7,
                                    list,
                                    total,
                                    Palette::none(),
                                );
                                assert_eq!(
                                    String::from_utf8_lossy(&got),
                                    String::from_utf8_lossy(&want),
                                    "{op:?} v={verbose} z={null} q={quiet} {list:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
        // colored
        for op in &ops {
            let want =
                original_summarize(op, 1, false, false, 3, pkgs, 1536, Some("xterm-256color"));
            let got = summarize_case(op, 1, false, false, 3, pkgs, 1536, &xterm());
            assert_eq!(got, want, "{op:?}");
            let want = original_summarize(op, 2, false, false, 3, pkgs, 1536, Some(""));
            let got = summarize_case(op, 2, false, false, 3, pkgs, 1536, &Palette::ansi());
            assert_eq!(got, want, "{op:?}");
        }
    }

    #[test]
    fn summarize_examples() {
        let pkgs = [
            "/c/linux-6.1-1-x86_64.pkg.tar.zst",
            "/c/linux-6.1-1-x86_64.pkg.tar.zst.sig",
            "/c/x.pkg.tar",
        ];
        let got = summarize_case(&Op::DryRun, 3, true, false, 1, &pkgs, 2048, Palette::none());
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "==> Candidate packages:\nlinux (x86_64):\n  linux-6.1-1-x86_64.pkg.tar.zst\n  linux-6.1-1-x86_64.pkg.tar.zst.sig\n  x.pkg.tar\n\n==> finished dry run: 1 candidates (disk space saved: 2 KiB)\n"
        );
        let got = summarize_case(
            &Op::DryRun,
            1,
            true,
            false,
            1,
            &pkgs[..1],
            0,
            Palette::none(),
        );
        assert_eq!(got, b"==> Candidate packages:\nlinux-6.1-1-x86_64.pkg.tar.zst\0\n==> finished dry run: 1 candidates (disk space saved: 0 B)\n");
        let got = summarize_case(
            &Op::DryRun,
            3,
            false,
            false,
            1,
            &["/c/x.pkg.tar"],
            0,
            Palette::none(),
        );
        assert!(String::from_utf8(got)
            .unwrap()
            .starts_with("==> Candidate packages:\n ():\n  x.pkg.tar\n"));
        assert!(
            summarize_case(&Op::Remove, 3, false, true, 1, &pkgs, 0, Palette::none()).is_empty()
        );
        // DIVERGENCE: printf directives in the move dir are printed literally
        let got = summarize_case(
            &Op::Move("/d%s\\n".into()),
            0,
            false,
            false,
            2,
            &pkgs,
            1,
            Palette::none(),
        );
        assert_eq!(
            got,
            b"\n==> finished: 2 packages moved to '/d%s\\n' (disk space saved: 1 B)\n"
        );
    }

    #[test]
    fn sorting_uses_comparator() {
        let paths: Vec<PathBuf> = ["/c/b-1-1-any.pkg.tar", "/c/a-1-1-any.pkg.tar"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let mut v = Vec::new();
        summarize_to(
            &mut v,
            Palette::none(),
            false,
            &Op::DryRun,
            1,
            false,
            2,
            &paths,
            0,
            &|a: &Path, b: &Path| a.cmp(b),
        )
        .unwrap();
        assert!(String::from_utf8(v)
            .unwrap()
            .starts_with("==> Candidate packages:\na-1-1-any.pkg.tar\nb-1-1-any.pkg.tar\n"));
    }
}
