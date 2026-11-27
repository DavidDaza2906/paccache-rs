//! Candidate selection, the awk `pkgfilter` END block (owner: agent C).
//!
//! This mirrors the gawk program in paccache 1.13.1, including its quirks:
//!
//! * The blacklist is only consulted when it is non-empty, but then it always also
//!   contains the empty name (the BEGIN loop reads one element past `ARGV`), so a file
//!   whose awk-parsed name is `""` is skipped whenever any `-i`/`-u` was given.
//! * `-a ARCH` is passed with `awk -v`, so it goes through awk escape processing
//!   (`\t`, `\101`, `\x41`, `\u00e9`, ...; unknown escapes warn on stderr like gawk:
//!   `awk: warning: escape sequence `\_' treated as plain `_'`).
//! * The arch test is `!scanarch || scanarch == arch`, with awk "strnum" semantics:
//!   a numeric-looking zero (`0`, `00`, ` 0`, `0.0`, `0e5`, ...) disables the filter,
//!   and when both sides look numeric they compare numerically (`1` matches `01`, `1e0`).
//! * Groups are emitted in gawk's `for (k in packages)` order (its string-array hash
//!   table, honouring `AWK_HASH` and `STR_CHAIN_MAX` like gawk 5.x), files within a
//!   group in input order. This order reaches `rm -v`/`mv -v` output.
//! * Times: a minimum of 0 means "not set" (`0 + min_atime == 0`), so `Some(0)` is
//!   treated exactly like `None`; callers may pass either.
//!
//! Not replicated: when `min_atime == -min_mtime != 0` the script forgets to strip
//! the time columns from the listed path (`0 + min_atime + min_mtime != 0`), producing
//! bogus candidate paths; and names containing awk's SUBSEP (`\x1c`).
use crate::scan::Entry;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub keep: usize,
    /// Raw `-a` argument (awk escape processing is applied here). None/"" = all.
    pub arch: Option<String>,
    /// Epoch seconds; None = not set (the script's 0). `Some(0)` is also "not set",
    /// exactly like the script (an epoch-exact date cannot be used as a minimum).
    pub min_atime: Option<i64>,
    pub min_mtime: Option<i64>,
    /// Non-empty = only these names.
    pub whitelist: HashSet<String>,
    /// Non-empty = skip these names (and, quirk, the empty name).
    pub blacklist: HashSet<String>,
}

/// Entries come sorted (pacsort order). Returns candidates (all but the newest `keep`
/// per (name, arch), subject to arch/time/white/blacklist rules), in the order gawk's
/// `for (pkglist in packages)` would print them (files of a group oldest first).
/// Prints gawk's escape warnings for `-a` on stderr, once per call (= per awk run).
pub fn candidates(entries: Vec<Entry>, f: &Filter) -> Vec<Entry> {
    let (scanarch, warnings) = awk_unescape(f.arch.as_deref().unwrap_or("").as_bytes());
    for w in warnings {
        eprintln!("awk: warning: {w}");
    }
    candidates_with(entries, f, &scanarch, &HashParams::from_env())
}

fn candidates_with(
    entries: Vec<Entry>,
    f: &Filter,
    scanarch: &[u8],
    hp: &HashParams,
) -> Vec<Entry> {
    let min_atime = f.min_atime.filter(|&t| t != 0).map(|t| t as f64);
    let min_mtime = f.min_mtime.filter(|&t| t != 0).map(|t| t as f64);
    let arch_filter = ArchFilter::new(scanarch);

    // Group in input order; remember first-insertion order of keys for the hash sim.
    let mut groups: Vec<Vec<Entry>> = Vec::new();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    for e in entries {
        let name = &e.file.name;
        if !f.whitelist.is_empty() && !f.whitelist.contains(name) {
            continue;
        }
        if !f.blacklist.is_empty() && (name.is_empty() || f.blacklist.contains(name)) {
            continue;
        }
        let mut key = name.as_bytes().to_vec();
        key.push(SUBSEP);
        key.extend_from_slice(e.file.arch.as_bytes());
        match index.get(&key) {
            Some(&i) => groups[i].push(e),
            None => {
                index.insert(key.clone(), groups.len());
                keys.push(key);
                groups.push(vec![e]);
            }
        }
    }

    let order = gawk_for_in_order(&keys, hp);
    let mut out = Vec::new();
    let mut groups: Vec<Option<Vec<Entry>>> = groups.into_iter().map(Some).collect();
    for gi in order {
        let group = groups[gi].take().expect("each group visited once");
        if !arch_filter.matches(group[0].file.arch.as_bytes()) {
            continue;
        }
        let n = group.len().saturating_sub(f.keep);
        for e in group.into_iter().take(n) {
            if min_atime.is_some_and(|m| e.atime >= m) || min_mtime.is_some_and(|m| e.mtime >= m) {
                continue;
            }
            out.push(e);
        }
    }
    out
}

/// awk's default SUBSEP.
const SUBSEP: u8 = 0x1c;

// ---------------------------------------------------------------------------
// awk value semantics

/// gawk's "looks numeric" test for user input (strnum), non-POSIX mode, decimal data:
/// surrounding whitespace allowed, no hex, `inf`/`nan` only as exactly `[+-]inf`/`[+-]nan`.
fn awk_strnum(s: &[u8]) -> Option<f64> {
    let is_space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    let start = s.iter().position(|c| !is_space(c))?;
    let end = s.len() - s.iter().rev().position(|c| !is_space(c)).unwrap_or(0);
    let t = &s[start..end];
    if t[0].is_ascii_alphabetic() {
        return None;
    }
    if t.len() >= 4 && (t[0] == b'+' || t[0] == b'-') {
        let w = t[1..4].to_ascii_lowercase();
        if w == b"inf" || w == b"nan" {
            if t.len() != 4 {
                return None;
            }
            let v = if w == b"inf" { f64::INFINITY } else { f64::NAN };
            return Some(if t[0] == b'-' { -v } else { v });
        }
    }
    // Plain decimal float, the whole (trimmed) string: [+-]? (D+ .? D* | . D+) ([eE][+-]?D+)?
    let mut i = 0;
    if matches!(t.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while i < t.len() && t[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - int_start;
    if i < t.len() && t[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < t.len() && t[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - frac_start;
    }
    if digits == 0 {
        return None;
    }
    if i < t.len() && (t[i] == b'e' || t[i] == b'E') {
        let mut j = i + 1;
        if j < t.len() && (t[j] == b'+' || t[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < t.len() && t[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j; // strtod only consumes a complete exponent
        }
    }
    if i != t.len() {
        return None;
    }
    std::str::from_utf8(t).ok()?.parse::<f64>().ok()
}

/// `!scanarch || scanarch == idx[2]` with awk comparison rules.
struct ArchFilter<'a> {
    raw: &'a [u8],
    num: Option<f64>,
    disabled: bool,
}

impl<'a> ArchFilter<'a> {
    fn new(raw: &'a [u8]) -> Self {
        let num = awk_strnum(raw);
        let disabled = match num {
            Some(n) => n == 0.0,
            None => raw.is_empty(),
        };
        ArchFilter { raw, num, disabled }
    }
    fn matches(&self, arch: &[u8]) -> bool {
        if self.disabled {
            return true;
        }
        match (self.num, awk_strnum(arch)) {
            (Some(a), Some(b)) => a == b,
            _ => self.raw == arch,
        }
    }
}

/// Escape processing that gawk applies to `-v var=value`. Returns the bytes and the
/// warning texts gawk would print (each prefixed with `awk: warning: ` by the caller).
/// Assumes a UTF-8 locale for `\u` (gawk uses the current locale's c32rtomb).
fn awk_unescape(s: &[u8]) -> (Vec<u8>, Vec<String>) {
    let mut out = Vec::with_capacity(s.len());
    let mut warnings = Vec::new();
    let mut warned = [false; 256];
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let Some(&e) = s.get(i) else {
            out.push(b'\\'); // terminal backslash is preserved
            break;
        };
        i += 1;
        match e {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\n' => {} // line continuation
            b'\\' | b'"' => out.push(e),
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                let mut n = 1;
                while n < 3 && i < s.len() && (b'0'..=b'7').contains(&s[i]) {
                    v = v * 8 + u32::from(s[i] - b'0');
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            b'x' | b'u' => {
                let max = if e == b'x' { 2 } else { 8 };
                let start = i;
                while i < s.len() && i - start < max && s[i].is_ascii_hexdigit() {
                    i += 1;
                }
                if i == start {
                    warnings.push(format!(
                        "no hex digits in `\\{}' escape sequence",
                        e as char
                    ));
                    out.push(e);
                    continue;
                }
                let v =
                    u32::from_str_radix(std::str::from_utf8(&s[start..i]).unwrap(), 16).unwrap();
                if e == b'x' {
                    out.push(v as u8);
                } else if let Some(ch) = char::from_u32(v) {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                } else {
                    warnings.push("invalid `\\u' escape sequence".to_string());
                    out.push(b'?');
                }
            }
            _ => {
                if !warned[e as usize] {
                    warned[e as usize] = true;
                    let ch = String::from_utf8_lossy(&[e]).into_owned();
                    warnings.push(format!("escape sequence `\\{ch}' treated as plain `{ch}'"));
                }
                out.push(e);
            }
        }
    }
    (out, warnings)
}

// ---------------------------------------------------------------------------
// gawk str_array traversal order (str_array.c, gawk 5.x)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashFn {
    Awk,
    Gst,
    Fnv1a,
}

#[derive(Debug, Clone, Copy)]
struct HashParams {
    func: HashFn,
    chain_max: u64,
}

impl Default for HashParams {
    fn default() -> Self {
        HashParams {
            func: HashFn::Awk,
            chain_max: 10,
        }
    }
}

impl HashParams {
    fn from_env() -> Self {
        let mut hp = HashParams::default();
        if let Some(v) = std::env::var_os("STR_CHAIN_MAX") {
            // gawk getenv_long: leading digits only, used when > 0.
            let mut n: u64 = 0;
            let b = v.as_encoded_bytes();
            if b.first().is_some_and(u8::is_ascii_digit) {
                for c in b.iter().take_while(|c| c.is_ascii_digit()) {
                    n = n.wrapping_mul(10).wrapping_add(u64::from(c - b'0'));
                }
                if n > 0 {
                    hp.chain_max = n;
                }
            }
        }
        match std::env::var_os("AWK_HASH")
            .as_deref()
            .map(|v| v.as_encoded_bytes())
        {
            Some(b"gst") => hp.func = HashFn::Gst,
            Some(b"fnv1a") => hp.func = HashFn::Fnv1a,
            _ => {}
        }
        hp
    }
}

/// The full hash code gawk stores in `ahcode` (bucket = code % array_size).
fn gawk_hash_code(s: &[u8], func: HashFn) -> u64 {
    match func {
        HashFn::Awk => {
            // sdbm, forced to 32 bits; `*s` is a signed char on x86_64.
            let mut h: u64 = 0;
            for &c in s {
                let htmp = h << 6;
                h = (c as i8 as i64 as u64)
                    .wrapping_add(htmp)
                    .wrapping_add(htmp << 10)
                    .wrapping_sub(h);
                h &= 0xFFFF_FFFF;
            }
            h
        }
        HashFn::Gst => {
            let mut h: u64 = 1_497_032_417;
            for &c in s {
                h = h.wrapping_add(c as i8 as i64 as u64);
                h = h.wrapping_add(h << 10);
                h ^= h >> 6;
            }
            let mut x = h;
            x ^= (!x) >> 31;
            x = x.wrapping_add((x << 21) | (x >> 11));
            x = x.wrapping_add((x << 5) | (x >> 27));
            x = x.wrapping_add((x << 27) | (x >> 5));
            x = x.wrapping_add(x << 31);
            x
        }
        HashFn::Fnv1a => {
            let mut h: u32 = 2_166_136_261;
            for &c in s {
                h ^= u32::from(c);
                h = h.wrapping_mul(16_777_619);
            }
            u64::from(h)
        }
    }
}

const GAWK_SIZES: [u64; 21] = [
    13, 127, 1021, 8191, 16381, 32749, 65497, 131101, 262147, 524309, 1048583, 2097169, 4194319,
    8388617, 16777259, 33554467, 67108879, 134217757, 268435459, 536870923, 1073741827,
];

/// Order in which gawk's `for (k in arr)` visits `keys`, inserted in the given order
/// (keys are distinct). Returns indices into `keys`.
fn gawk_for_in_order(keys: &[Vec<u8>], hp: &HashParams) -> Vec<usize> {
    if keys.is_empty() {
        return Vec::new();
    }
    let codes: Vec<u64> = keys.iter().map(|k| gawk_hash_code(k, hp.func)).collect();
    // Each bucket is a chain stored tail-first: the last element is the chain head.
    let mut size: u64 = GAWK_SIZES[0];
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); size as usize];
    let mut maxed = false;
    for (idx, &code) in codes.iter().enumerate() {
        let table_size = idx as u64 + 1;
        if !maxed && table_size / size > hp.chain_max {
            match GAWK_SIZES.iter().find(|&&s| s > size) {
                Some(&new_size) => {
                    let mut new: Vec<Vec<usize>> = vec![Vec::new(); new_size as usize];
                    for chain in &buckets {
                        for &e in chain.iter().rev() {
                            new[(codes[e] % new_size) as usize].push(e);
                        }
                    }
                    buckets = new;
                    size = new_size;
                }
                None => maxed = true,
            }
        }
        buckets[(code % size) as usize].push(idx);
    }
    buckets
        .into_iter()
        .flat_map(|c| c.into_iter().rev())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgname::PkgFile;
    use std::path::PathBuf;
    use std::process::Command;

    fn ent(path: &str, name: &str, arch: &str) -> Entry {
        Entry {
            file: PkgFile {
                path: PathBuf::from(path),
                name: name.into(),
                arch: arch.into(),
            },
            atime: 0.0,
            mtime: 0.0,
        }
    }
    fn tent(path: &str, name: &str, arch: &str, atime: f64, mtime: f64) -> Entry {
        Entry {
            atime,
            mtime,
            ..ent(path, name, arch)
        }
    }
    fn paths(v: &[Entry]) -> Vec<String> {
        v.iter()
            .map(|e| e.file.path.to_string_lossy().into_owned())
            .collect()
    }
    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }
    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
    fn run(entries: Vec<Entry>, f: &Filter) -> Vec<String> {
        let (arch, _) = awk_unescape(f.arch.as_deref().unwrap_or("").as_bytes());
        paths(&candidates_with(entries, f, &arch, &HashParams::default()))
    }

    fn foo_bar() -> Vec<Entry> {
        vec![
            ent("/c/bar-1-1-any", "bar", "any"),
            ent("/c/bar-2-1-any", "bar", "any"),
            ent("/c/foo-1-1-x86_64", "foo", "x86_64"),
            ent("/c/foo-1-1-i686", "foo", "i686"),
            ent("/c/foo-2-1-x86_64", "foo", "x86_64"),
            ent("/c/foo-3-1-x86_64", "foo", "x86_64"),
            ent("/c/foo-4-1-x86_64", "foo", "x86_64"),
        ]
    }

    #[test]
    fn keep_per_name_arch() {
        let f = Filter {
            keep: 1,
            ..Default::default()
        };
        assert_eq!(
            sorted(run(foo_bar(), &f)),
            vec![
                "/c/bar-1-1-any",
                "/c/foo-1-1-x86_64",
                "/c/foo-2-1-x86_64",
                "/c/foo-3-1-x86_64"
            ]
        );
        let f = Filter {
            keep: 3,
            ..Default::default()
        };
        assert_eq!(run(foo_bar(), &f), vec!["/c/foo-1-1-x86_64"]);
        let f = Filter {
            keep: 0,
            ..Default::default()
        };
        assert_eq!(run(foo_bar(), &f).len(), 7);
        let f = Filter {
            keep: 100,
            ..Default::default()
        };
        assert!(run(foo_bar(), &f).is_empty());
        assert!(run(Vec::new(), &Filter::default()).is_empty());
    }

    #[test]
    fn group_files_keep_input_order() {
        let f = Filter {
            keep: 1,
            ..Default::default()
        };
        let out = run(foo_bar(), &f);
        let foo: Vec<_> = out.iter().filter(|p| p.contains("foo")).cloned().collect();
        assert_eq!(
            foo,
            vec![
                "/c/foo-1-1-x86_64",
                "/c/foo-2-1-x86_64",
                "/c/foo-3-1-x86_64"
            ]
        );
    }

    #[test]
    fn arch_filter() {
        let f = Filter {
            keep: 0,
            arch: Some("i686".into()),
            ..Default::default()
        };
        assert_eq!(run(foo_bar(), &f), vec!["/c/foo-1-1-i686"]);
        let f = Filter {
            keep: 0,
            arch: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(run(foo_bar(), &f).len(), 7);
        let f = Filter {
            keep: 0,
            arch: Some("nope".into()),
            ..Default::default()
        };
        assert!(run(foo_bar(), &f).is_empty());
        // awk quirks: numeric zero disables, numeric compare
        for zero in ["0", "00", " 0", "0.0", ".0", "0e5", "-0", "+0"] {
            let f = Filter {
                keep: 0,
                arch: Some(zero.into()),
                ..Default::default()
            };
            assert_eq!(run(foo_bar(), &f).len(), 7, "{zero:?}");
        }
        let e = vec![
            ent("/a", "a", "01"),
            ent("/b", "b", "1e0"),
            ent("/c", "c", "1"),
            ent("/d", "d", "x"),
        ];
        let f = Filter {
            keep: 0,
            arch: Some("1".into()),
            ..Default::default()
        };
        assert_eq!(sorted(run(e.clone(), &f)), vec!["/a", "/b", "/c"]);
        let f = Filter {
            keep: 0,
            arch: Some("0x0".into()),
            ..Default::default()
        };
        assert!(run(e, &f).is_empty());
        // escape processing of -v
        let e = vec![ent("/t", "t", "a\tb"), ent("/u", "u", "a_b")];
        let f = Filter {
            keep: 0,
            arch: Some("a\\tb".into()),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f), vec!["/t"]);
        let f = Filter {
            keep: 0,
            arch: Some("a\\_b".into()),
            ..Default::default()
        };
        assert_eq!(run(e, &f), vec!["/u"]);
    }

    #[test]
    fn white_and_blacklist() {
        let f = Filter {
            keep: 0,
            whitelist: set(&["bar"]),
            ..Default::default()
        };
        assert_eq!(
            sorted(run(foo_bar(), &f)),
            vec!["/c/bar-1-1-any", "/c/bar-2-1-any"]
        );
        let f = Filter {
            keep: 0,
            blacklist: set(&["bar"]),
            ..Default::default()
        };
        assert_eq!(run(foo_bar(), &f).len(), 5);
        let f = Filter {
            keep: 0,
            whitelist: set(&["bar", "foo"]),
            blacklist: set(&["foo"]),
            ..Default::default()
        };
        assert_eq!(run(foo_bar(), &f).len(), 2);
        // blacklisting shifts the kept set: entries are filtered before grouping
        let f = Filter {
            keep: 1,
            whitelist: set(&["zzz"]),
            ..Default::default()
        };
        assert!(run(foo_bar(), &f).is_empty());
    }

    #[test]
    fn empty_name_quirk() {
        let e = vec![
            ent("/x1", "", "any"),
            ent("/x2", "", "any"),
            ent("/y", "y", "any"),
        ];
        let f = Filter {
            keep: 0,
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f).len(), 3);
        // any blacklist also drops the empty name
        let f = Filter {
            keep: 0,
            blacklist: set(&["nomatch"]),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f), vec!["/y"]);
        // whitelist containing "" lets it through
        let f = Filter {
            keep: 0,
            whitelist: set(&[""]),
            ..Default::default()
        };
        assert_eq!(sorted(run(e, &f)), vec!["/x1", "/x2"]);
    }

    #[test]
    fn time_filters() {
        let e = vec![
            tent("/p1", "p", "any", 100.0, 100.0),
            tent("/p2", "p", "any", 300.0, 100.0),
            tent("/p3", "p", "any", 100.0, 300.0),
            tent("/p4", "p", "any", 199.999, 199.999),
            tent("/p5", "p", "any", 200.0, 200.0),
            tent("/p6", "p", "any", 1.0, 1.0),
        ];
        let f = Filter {
            keep: 1,
            min_atime: Some(200),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f), vec!["/p1", "/p3", "/p4"]);
        let f = Filter {
            keep: 1,
            min_mtime: Some(200),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f), vec!["/p1", "/p2", "/p4"]);
        let f = Filter {
            keep: 1,
            min_atime: Some(200),
            min_mtime: Some(200),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f), vec!["/p1", "/p4"]);
        // keep is applied first: the newest (last) entry is never considered
        let f = Filter {
            keep: 0,
            min_atime: Some(200),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f), vec!["/p1", "/p3", "/p4", "/p6"]);
        // 0 means unset
        let f = Filter {
            keep: 1,
            min_atime: Some(0),
            min_mtime: Some(0),
            ..Default::default()
        };
        assert_eq!(run(e.clone(), &f).len(), 5);
        // negative minimum works (pre-epoch dates)
        let e2 = vec![
            tent("/n1", "n", "any", -10.5, 0.0),
            tent("/n2", "n", "any", -1.0, 0.0),
            tent("/n3", "n", "any", 0.0, 0.0),
        ];
        let f = Filter {
            keep: 0,
            min_atime: Some(-1),
            ..Default::default()
        };
        assert_eq!(run(e2, &f), vec!["/n1"]);
    }

    #[test]
    fn strnum() {
        for (s, v) in [
            ("0", 0.0),
            (" 1 ", 1.0),
            ("1.", 1.0),
            (".5", 0.5),
            ("1e3", 1000.0),
            ("+2", 2.0),
            ("-0", 0.0),
            ("1e", 1.0),
        ] {
            assert_eq!(
                awk_strnum(s.as_bytes()),
                if s == "1e" { None } else { Some(v) },
                "{s:?}"
            );
        }
        for s in [
            "",
            " ",
            "x",
            "inf",
            "nan",
            "0x1",
            "1_",
            "+infinity",
            ".",
            "+",
            "e5",
            "1 2",
        ] {
            assert_eq!(awk_strnum(s.as_bytes()), None, "{s:?}");
        }
        assert_eq!(awk_strnum(b"-inf"), Some(f64::NEG_INFINITY));
        assert!(awk_strnum(b"+NaN").unwrap().is_nan());
    }

    #[test]
    fn unescape() {
        let (v, w) = awk_unescape(br#"a\_b\tc\101\x41g\\h\/\"\q\_"#);
        assert_eq!(v, b"a_b\tcAAg\\h/\"q_");
        assert_eq!(
            w,
            vec![
                "escape sequence `\\_' treated as plain `_'",
                "escape sequence `\\/' treated as plain `/'",
                "escape sequence `\\q' treated as plain `q'",
            ]
        );
        assert_eq!(awk_unescape(b"x\\").0, b"x\\");
        assert_eq!(awk_unescape(b"\\x").0, b"x");
        assert_eq!(awk_unescape(b"\\x414243").0, b"A4243");
        assert_eq!(awk_unescape(b"\\0101\\8").0, b"\x0818");
        assert_eq!(awk_unescape(b"\\u00e9").0, "é".as_bytes());
    }

    fn gawk_available() -> bool {
        Command::new("gawk")
            .arg("BEGIN{}")
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Compare escape processing and warnings with real gawk.
    #[test]
    fn unescape_matches_gawk() {
        if !gawk_available() {
            eprintln!("gawk not found, skipping");
            return;
        }
        for s in [
            r#"a\_b\tc\101\x41g\\h\/\"\q"#,
            r"x\",
            r"\x",
            r"\x414243",
            r"\0101\8",
            r"\u00e9\u41",
            r"\uD800",
            r"a\
b",
            r"\z\z\y",
        ] {
            let o = Command::new("gawk")
                .arg("-v")
                .arg(format!("s={s}"))
                .arg("BEGIN{printf \"%s\", s}")
                .output()
                .unwrap();
            let (v, w) = awk_unescape(s.as_bytes());
            assert_eq!(o.stdout, v, "{s:?}");
            let expected: String = w.iter().map(|w| format!("gawk: warning: {w}\n")).collect();
            assert_eq!(String::from_utf8_lossy(&o.stderr), expected, "{s:?}");
        }
    }

    /// Compare the arch test with real gawk for many value pairs.
    #[test]
    fn arch_matches_gawk() {
        if !gawk_available() {
            return;
        }
        let vals = [
            "0", "00", " 0", "0 ", "+0", "-0", "0.0", ".0", "0e5", "0x0", "1", "01", "1e0", " 1",
            "+inf", "inf", "-nan", "nan", "1.", "0x1", "1_", "", "a", "x86_64", "INF", "+INF",
            "1e", "-1", "1e-0", "\t2\n",
        ];
        let mut script = String::from("BEGIN{n=split(ARGV[1], v, \"|\"); for(i=1;i<=n;i++){s=v[i]; for(j=1;j<=n;j++){split(\"p\" SUBSEP v[j], idx, SUBSEP); printf \"%s\", ((!s || s == idx[2]) ? \"Y\" : \".\")} print \"\"}}");
        script.push('\n');
        let o = Command::new("gawk")
            .arg(&script)
            .arg(vals.join("|"))
            .output()
            .unwrap();
        let got: Vec<String> = vals
            .iter()
            .map(|s| {
                let af = ArchFilter::new(s.as_bytes());
                vals.iter()
                    .map(|a| if af.matches(a.as_bytes()) { 'Y' } else { '.' })
                    .collect()
            })
            .collect();
        let exp: Vec<String> = String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(got, exp);
    }

    /// Compare the full selection (including gawk's for-in order) with the script's
    /// real awk program.
    #[test]
    fn order_matches_gawk() {
        if !gawk_available() {
            return;
        }
        let Ok(script) = std::fs::read_to_string("/usr/bin/paccache") else {
            return;
        };
        let start = script.find("\tawk -v keep").expect("pkgfilter awk program");
        let body_start = script[start..].find('\'').unwrap() + start + 1;
        let body_end = script[body_start..].find("}' \"${@:5}\"").unwrap() + body_start + 1;
        let prog = &script[body_start..body_end];

        let mut lines = Vec::new();
        let mut entries = Vec::new();
        // ~2000 groups: forces several table grows (13 -> 127 -> 1021 -> 8191)
        for i in 0..2000u32 {
            let name = format!("pkg{}-{}", i * 7919 % 1009, i);
            for v in 1..=3 {
                let arch = if i % 3 == 0 { "any" } else { "x86_64" };
                let p = format!("/c/{name}-{v}-1-{arch}.pkg.tar.zst");
                lines.push(p.clone());
                entries.push(ent(&p, &name, arch));
            }
        }
        for (hash, chain) in [
            (None, None),
            (Some("gst"), None),
            (Some("fnv1a"), Some("2")),
            (None, Some("1")),
        ] {
            let mut cmd = Command::new("gawk");
            cmd.args([
                "-v",
                "keep=1",
                "-v",
                "scanarch=",
                "-v",
                "min_atime=0",
                "-v",
                "min_mtime=0",
                prog,
                "0",
                "0",
            ]);
            cmd.env_remove("AWK_HASH").env_remove("STR_CHAIN_MAX");
            if let Some(h) = hash {
                cmd.env("AWK_HASH", h);
            }
            if let Some(c) = chain {
                cmd.env("STR_CHAIN_MAX", c);
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            {
                use std::io::Write;
                let mut stdin = child.stdin.take().unwrap();
                stdin
                    .write_all((lines.join("\n") + "\n").as_bytes())
                    .unwrap();
            }
            let o = child.wait_with_output().unwrap();
            let exp: Vec<String> = String::from_utf8(o.stdout)
                .unwrap()
                .lines()
                .map(String::from)
                .collect();
            let hp = HashParams {
                func: match hash {
                    Some("gst") => HashFn::Gst,
                    Some("fnv1a") => HashFn::Fnv1a,
                    _ => HashFn::Awk,
                },
                chain_max: chain.map_or(10, |c| c.parse().unwrap()),
            };
            let f = Filter {
                keep: 1,
                ..Default::default()
            };
            let got = paths(&candidates_with(entries.clone(), &f, b"", &hp));
            assert_eq!(got.len(), 4000);
            assert_eq!(got, exp, "hash {hash:?} chain {chain:?}");
        }
    }
}
