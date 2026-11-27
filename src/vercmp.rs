//! Pure-Rust port of libalpm's `alpm_pkg_vercmp` (owner: agent A).
//!
//! Faithful byte-level port of `lib/libalpm/version.c` (pacman 7.1.0, unchanged
//! on master as of 2026-01): `parseEVR` + `rpmvercmp`. Notes on semantics:
//!
//! * Character classes are those of the C locale (`vercmp` and `pacsort` never
//!   call `setlocale`), i.e. ASCII only; every byte >= 0x80 is a separator.
//! * There is no special handling of `~` (unlike RPM): it is an ordinary
//!   separator, like `.`, `_`, `+`, etc.
//! * Separators only matter through their *count* between two segments; a
//!   string with a longer separator run at the first point of difference wins.
//! * The release (`-pkgrel`) is compared only when both sides have one.
//! * A C string stops at the first NUL, so anything after an embedded `\0` is
//!   ignored here too.
use std::cmp::Ordering;

/// Compare two full versions (`[epoch:]pkgver[-pkgrel]`) exactly like `vercmp(8)`.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    vercmp_bytes(a.as_bytes(), b.as_bytes())
}

/// Same as [`vercmp`], on raw bytes (file names need not be UTF-8).
pub fn vercmp_bytes(a: &[u8], b: &[u8]) -> Ordering {
    let a = cstr(a);
    let b = cstr(b);
    if a == b {
        return Ordering::Equal;
    }
    let (e1, v1, r1) = parse_evr(a);
    let (e2, v2, r2) = parse_evr(b);
    rpmvercmp(e1, e2).then_with(|| {
        rpmvercmp(v1, v2).then_with(|| match (r1, r2) {
            (Some(r1), Some(r2)) => rpmvercmp(r1, r2),
            _ => Ordering::Equal,
        })
    })
}

/// Truncate at the first NUL, as C would see the string.
fn cstr(s: &[u8]) -> &[u8] {
    match s.iter().position(|&c| c == 0) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// `parseEVR`: split into (epoch, version, release).
fn parse_evr(evr: &[u8]) -> (&[u8], &[u8], Option<&[u8]>) {
    // `s` = index of the first non-digit (potential epoch terminator).
    let s = evr
        .iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(evr.len());
    // Last '-' at or after `s` (strrchr(s, '-')).
    let se = evr[s..].iter().rposition(|&c| c == b'-').map(|i| s + i);
    let (epoch, vstart): (&[u8], usize) = if evr.get(s) == Some(&b':') {
        let e = &evr[..s];
        (if e.is_empty() { b"0" } else { e }, s + 1)
    } else {
        (b"0", 0)
    };
    match se {
        // When there is an epoch, `se` is always > s, hence >= vstart.
        Some(se) => (epoch, &evr[vstart..se], Some(&evr[se + 1..])),
        None => (epoch, &evr[vstart..], None),
    }
}

/// `rpmvercmp` from libalpm, on NUL-free byte strings.
fn rpmvercmp(a: &[u8], b: &[u8]) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    // ptr1/ptr2: end of the previous segment; one/two: scan positions.
    let (mut ptr1, mut ptr2) = (0usize, 0usize);
    let (mut one, mut two) = (0usize, 0usize);

    while one < a.len() && two < b.len() {
        while one < a.len() && !a[one].is_ascii_alphanumeric() {
            one += 1;
        }
        while two < b.len() && !b[two].is_ascii_alphanumeric() {
            two += 1;
        }
        if !(one < a.len() && two < b.len()) {
            break;
        }
        // Different separator lengths: the longer one is newer.
        let (sep1, sep2) = (one - ptr1, two - ptr2);
        if sep1 != sep2 {
            return sep1.cmp(&sep2);
        }
        ptr1 = one;
        ptr2 = two;

        let isnum = a[ptr1].is_ascii_digit();
        if isnum {
            while ptr1 < a.len() && a[ptr1].is_ascii_digit() {
                ptr1 += 1;
            }
            while ptr2 < b.len() && b[ptr2].is_ascii_digit() {
                ptr2 += 1;
            }
        } else {
            while ptr1 < a.len() && a[ptr1].is_ascii_alphabetic() {
                ptr1 += 1;
            }
            while ptr2 < b.len() && b[ptr2].is_ascii_alphabetic() {
                ptr2 += 1;
            }
        }

        // Segment types differ (b's segment is empty): numeric beats alpha.
        if two == ptr2 {
            return if isnum {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }

        let mut s1 = &a[one..ptr1];
        let mut s2 = &b[two..ptr2];
        if isnum {
            // Strip leading zeros; more digits wins.
            while s1.first() == Some(&b'0') {
                s1 = &s1[1..];
            }
            while s2.first() == Some(&b'0') {
                s2 = &s2[1..];
            }
            match s1.len().cmp(&s2.len()) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        // strcmp on the segments (unsigned bytes, prefix is smaller).
        match s1.cmp(s2) {
            Ordering::Equal => {}
            o => return o,
        }
        one = ptr1;
        two = ptr2;
    }

    let c1 = at(a, one);
    let c2 = at(b, two);
    if c1 == 0 && c2 == 0 {
        return Ordering::Equal;
    }
    // "We never want a remaining alpha string to beat an empty string."
    if (c1 == 0 && !c2.is_ascii_alphabetic()) || c1.is_ascii_alphabetic() {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use Ordering::*;

    #[test]
    fn known_cases() {
        // Selection of pacman's test/util/vercmptest.sh.
        let cases: &[(&str, &str, Ordering)] = &[
            ("1.5.0", "1.5.0", Equal),
            ("1.5.1", "1.5.0", Greater),
            ("1.5.1", "1.5", Greater),
            ("1.5.0-1", "1.5.0-2", Less),
            ("1.5.0-1", "1.5.1-1", Less),
            ("1.5.0-2", "1.5.1-1", Less),
            ("1.5-1", "1.5", Equal),
            ("1.5b-1", "1.5-1", Less),
            ("1.5b", "1.5", Less),
            ("1.5b-1", "1.5", Less),
            ("1.5b", "1.5.1", Less),
            ("1.0a", "1.0alpha", Less),
            ("1.0alpha", "1.0b", Less),
            ("1.0b", "1.0beta", Less),
            ("1.0beta", "1.0rc", Less),
            ("1.0rc", "1.0", Less),
            ("1.5.a", "1.5", Greater),
            ("1.5.b", "1.5.a", Greater),
            ("1.5.1", "1.5.b", Greater),
            ("1.5.b-1", "1.5.b", Equal),
            ("1.5-1", "1.5.b", Less),
            ("2.0", "2_0", Equal),
            ("2.0_a", "2_0.a", Equal),
            ("2.0a", "2.0.a", Less),
            ("2___a", "2_a", Greater),
            ("0:1.0", "0:1.0", Equal),
            ("0:1.0", "0:1.1", Less),
            ("1:1.0", "0:1.0", Greater),
            ("1:1.0", "0:1.1", Greater),
            ("1:1.0", "2:1.1", Less),
            ("0:1.0", "1.0", Equal),
            ("0:1.0", "1.1", Less),
            ("0:1.1", "1.0", Greater),
            ("1:1.0", "1.0", Greater),
            ("1:1.0", "1.1", Greater),
            ("1:1.1", "1.1", Greater),
            ("1.0~rc1", "1.0", Greater),
            ("", "", Equal),
            ("", "1", Less),
            ("1", "", Greater),
            ("1.0", "1.0.", Less), // trailing separator wins
            ("1..0", "1.0", Greater),
            ("01", "1", Equal),
            ("r123.gabc", "r124.g000", Less),
        ];
        for &(a, b, want) in cases {
            assert_eq!(vercmp(a, b), want, "vercmp({a:?}, {b:?})");
            assert_eq!(vercmp(b, a), want.reverse(), "vercmp({b:?}, {a:?})");
        }
    }

    /// Edge cases checked against `/usr/bin/vercmp` in [`differential_vs_vercmp`].
    pub(crate) const EDGE: &[&str] = &[
        "",
        ":",
        "-",
        ":-",
        "-:",
        "0",
        "1",
        "01",
        "001",
        "00",
        "1:",
        ":1",
        "1:1",
        "01:1",
        "0:1",
        "a:1",
        "1a:1",
        "1:1:1",
        "1-",
        "-1",
        "1-1",
        "1-1-1",
        "1:1-1",
        "1:-1",
        "1.0",
        "1.0-1",
        "1.0-2",
        "1.0a",
        "1.0.a",
        "1.0a1",
        "1.0.0",
        "1..0",
        "1.0.",
        ".1.0",
        "1.0..",
        "1_0",
        "1+0",
        "1~0",
        "1.0~rc1",
        "1.0rc1",
        "1.0+r1",
        "a",
        "b",
        "aa",
        "ab",
        "a1",
        "1a",
        "alpha",
        "beta",
        "rc",
        "pre",
        "1.0alpha",
        "1.0beta",
        "1.0rc",
        "r123.gabc1234",
        "r124.gabc1234",
        "r123.g0bc1234",
        "0.r123.gabc",
        "1.2.3.r4.g5678abc-1",
        "2:1.2.3.r4.g5678abc-1",
        "99999999999999999999999",
        "100000000000000000000000",
        "0000000000000001",
        "1.000001",
        "1.01",
        "1.1",
        "1.10",
        "20240101",
        "2024.01.01",
        "1.0-1.1",
        "1.0-1a",
        "1.0-a",
        "1.0-.",
        "1.0-",
        "-",
        "--",
        "1--1",
        "1-1-",
        "1.0 1",
        " 1",
        "1 ",
        "1\u{e9}",
        "\u{e9}1",
        "1.\u{e9}.1",
        "1:1:",
        "::",
        "1::1",
        "Z",
        "z",
        "A1",
        "a1",
        "1A",
        "_",
        "__",
        "._",
        "1._a",
        "1_.a",
    ];

    /// Simple xorshift64* PRNG (no dependencies).
    pub(crate) struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        pub fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[self.below(xs.len() as u64) as usize]
        }
    }

    /// Random version string, biased toward realistic and tricky shapes.
    pub(crate) fn random_version(rng: &mut Rng) -> String {
        const ALPHA: &[&str] = &["a", "b", "rc", "alpha", "beta", "pre", "r", "g", "z", "A"];
        const SEP: &[&str] = &[".", ".", ".", "_", "+", "~", "..", "-", ":", ""];
        let mut s = String::new();
        if rng.below(6) == 0 {
            let e = rng.below(4);
            if e > 0 || rng.below(3) == 0 {
                s.push_str(&e.to_string());
            }
            s.push(':');
        }
        let segs = rng.below(6);
        for i in 0..segs {
            if i > 0 {
                s.push_str(rng.pick(SEP));
            }
            match rng.below(8) {
                0 | 1 => s.push_str(rng.pick(ALPHA)),
                2 => s.push_str(&format!("{:0w$}", rng.below(3), w = rng.below(4) as usize)),
                3 => s.push_str(&format!("r{}.g{:x}", rng.below(300), rng.next() >> 40)),
                _ => s.push_str(&rng.below(13).to_string()),
            }
        }
        if rng.below(5) == 0 {
            s.push_str(rng.pick(SEP));
        }
        if rng.below(2) == 0 {
            s.push('-');
            if rng.below(8) != 0 {
                s.push_str(&rng.below(4).to_string());
                if rng.below(6) == 0 {
                    s.push_str(rng.pick(SEP));
                    s.push_str(&rng.below(3).to_string());
                }
            }
        }
        s
    }

    /// Run `/usr/bin/vercmp` on every pair through one bash process.
    fn reference(pairs: &[(String, String)]) -> Vec<i32> {
        let mut child = Command::new("bash")
            .arg("-c")
            // NUL-separated input: a\0b\0a\0b\0...
            .arg(
                "while IFS= read -r -d '' a && IFS= read -r -d '' b; do vercmp \"$a\" \"$b\"; done",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("bash");
        let mut stdin = child.stdin.take().unwrap();
        let input: Vec<u8> = pairs
            .iter()
            .flat_map(|(a, b)| [a.as_bytes(), b"\0", b.as_bytes(), b"\0"].concat())
            .collect();
        let writer = std::thread::spawn(move || stdin.write_all(&input).unwrap());
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect()
    }

    fn check(pairs: &[(String, String)]) -> usize {
        let want = reference(pairs);
        assert_eq!(want.len(), pairs.len(), "vercmp output count");
        let mut bad = 0;
        for ((a, b), w) in pairs.iter().zip(want) {
            let got = vercmp(a, b) as i32;
            if got != w.signum() {
                bad += 1;
                if bad < 20 {
                    eprintln!("MISMATCH vercmp({a:?}, {b:?}): ours {got}, vercmp {w}");
                }
            }
        }
        bad
    }

    #[test]
    #[ignore = "slow: spawns /usr/bin/vercmp ~100k times"]
    fn differential_vs_vercmp() {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for a in EDGE {
            for b in EDGE {
                pairs.push((a.to_string(), b.to_string()));
            }
        }
        let edge = pairs.len();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let n: usize = std::env::var("VERCMP_PAIRS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);
        for _ in 0..n {
            let a = random_version(&mut rng);
            // Half the time derive b from a to get close calls.
            let b = if rng.below(2) == 0 {
                let mut b = a.clone();
                let r = random_version(&mut rng);
                match rng.below(3) {
                    0 => b.push_str(&r[..r.len().min(3)]),
                    1 => b.truncate(rng.below(b.len() as u64 + 1) as usize),
                    _ => b.insert_str(
                        rng.below(b.len() as u64 + 1) as usize,
                        rng.pick(&[".", "0", "a", "-", "1", "_"]),
                    ),
                }
                b
            } else {
                random_version(&mut rng)
            };
            pairs.push((a, b));
        }
        // Run in parallel chunks.
        let chunks: Vec<_> = pairs
            .chunks(pairs.len().div_ceil(16))
            .map(|c| c.to_vec())
            .collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|c| std::thread::spawn(move || check(&c)))
            .collect();
        let bad: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        eprintln!(
            "vercmp differential: {} pairs ({edge} edge + {n} random), {bad} mismatches",
            pairs.len()
        );
        assert_eq!(bad, 0);
    }
}
