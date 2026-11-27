//! Fake pacman cache generator shared by the differential tests.
//!
//! Every fixture is a plain list of [`FileSpec`]s produced deterministically
//! from a seed, so two calls with the same arguments materialize byte-for-byte
//! identical trees (names, sizes, atime and mtime). The compat battery
//! materializes one fresh copy per program run.
#![allow(dead_code)]

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;

use filetime::{set_file_times, FileTime};

/// 2021-01-01T00:00:00Z: lower bound of the "old" timestamps.
pub const T_OLD_LO: i64 = 1_609_459_200;
/// 2023-01-01T00:00:00Z: upper bound of the "old" timestamps.
pub const T_OLD_HI: i64 = 1_672_531_200;
/// A threshold in the middle of the old range, used as `--min-*time @T_MID`.
pub const T_MID: i64 = 1_640_995_200; // 2022-01-01
/// 2100-01-01: "always newer than any relative date" timestamps.
pub const T_FUTURE: i64 = 4_102_444_800;

/// One file of a fake cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpec {
    /// Path relative to the cache root (may contain `/` for subdirectories).
    pub rel: String,
    /// Logical size in bytes. Sizes above [`SPARSE_THRESHOLD`] are created
    /// sparse so huge caches cost no disk space.
    pub size: u64,
    pub atime: i64,
    pub mtime: i64,
}

/// Files bigger than this are created with `set_len` (sparse) instead of
/// writing real bytes.
pub const SPARSE_THRESHOLD: u64 = 64 * 1024;

/// A whole fake cache.
#[derive(Clone, Debug, Default)]
pub struct CacheSpec {
    pub files: Vec<FileSpec>,
}

impl CacheSpec {
    /// Create every file under `root` (which must exist) with its size and
    /// times.
    pub fn materialize(&self, root: &Path) -> io::Result<()> {
        for f in &self.files {
            let path = root.join(&f.rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = File::create(&path)?;
            if f.size > SPARSE_THRESHOLD {
                file.set_len(f.size)?;
            } else if f.size > 0 {
                // Deterministic, non-zero content.
                let byte = (f.rel.len() % 251) as u8;
                io::Write::write_all(&mut &file, &vec![byte; f.size as usize])?;
            }
            drop(file);
            set_file_times(
                &path,
                FileTime::from_unix_time(f.atime, 0),
                FileTime::from_unix_time(f.mtime, 0),
            )?;
        }
        Ok(())
    }

    fn push(&mut self, rng: &mut Rng, rel: impl Into<String>) {
        let rel = rel.into();
        let size = rng.size();
        let (atime, mtime) = rng.times();
        self.files.push(FileSpec {
            rel,
            size,
            atime,
            mtime,
        });
    }

    fn push_exact(&mut self, rel: impl Into<String>, size: u64, atime: i64, mtime: i64) {
        self.files.push(FileSpec {
            rel: rel.into(),
            size,
            atime,
            mtime,
        });
    }

    /// Add a package file and, with probability `sig_p`, its detached `.sig`.
    fn pkg(&mut self, rng: &mut Rng, rel: impl Into<String>, sig_p: u32) {
        let rel = rel.into();
        self.push(rng, rel.clone());
        if rng.below(100) < sig_p {
            let (atime, mtime) = rng.times();
            self.files.push(FileSpec {
                rel: format!("{rel}.sig"),
                size: 566,
                atime,
                mtime,
            });
        }
    }
}

/// splitmix64: tiny, deterministic, no dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u32) as usize]
    }
    /// Sizes spread over several orders of magnitude, including 0.
    fn size(&mut self) -> u64 {
        match self.below(10) {
            0 => 0,
            1..=3 => u64::from(self.below(1000)),
            4..=7 => u64::from(self.below(60_000)),
            _ => 1_000_000 + self.next_u64() % 40_000_000, // sparse
        }
    }
    /// Old timestamps (both below and above [`T_MID`]) with whole seconds,
    /// plus some files far in the future.
    fn times(&mut self) -> (i64, i64) {
        let one = |r: &mut Rng| {
            if r.below(8) == 0 {
                T_FUTURE + i64::from(r.below(1000))
            } else {
                T_OLD_LO + (r.next_u64() % (T_OLD_HI - T_OLD_LO) as u64) as i64
            }
        };
        (one(self), one(self))
    }
}

/// The hand-written "kitchen sink" cache: every naming oddity the original
/// script has to deal with. The seed only drives sizes, times and which
/// packages get a `.sig`.
pub fn standard(seed: u64) -> CacheSpec {
    let mut r = Rng::new(seed);
    let mut c = CacheSpec::default();
    let zst = ".pkg.tar.zst";

    // Plain package, versions whose vercmp order differs from lexical order.
    for v in [
        "6.1.1.arch1-1",
        "6.2.arch1-1",
        "6.2.arch1-2",
        "6.10.arch1-1",
        "6.9.9.arch1-1",
    ] {
        c.pkg(&mut r, format!("linux-{v}-x86_64{zst}"), 70);
    }
    // Names with dashes, arch any.
    for v in ["1.0-1", "1.0-2", "1.0.1-1", "2.0-1", "2.0rc1-1"] {
        c.pkg(&mut r, format!("python-foo-bar-{v}-any{zst}"), 50);
    }
    // Same name, two arches -> two groups.
    for v in ["2.38-1", "2.38-2", "2.39-1", "2.39-3"] {
        c.pkg(&mut r, format!("glibc-{v}-x86_64{zst}"), 50);
        c.pkg(&mut r, format!("glibc-{v}-i686{zst}"), 50);
    }
    // Epochs.
    for v in ["9.0.1-1", "1:8.0-1", "2:1.0-1", "9.1.0-1", "1:9.9-1"] {
        c.pkg(&mut r, format!("vim-{v}-x86_64{zst}"), 40);
    }
    // pkgrel with a dot (rebuilds).
    for v in ["120.0-1", "120.0-1.1", "120.0-2", "119.0-1.2"] {
        c.pkg(&mut r, format!("firefox-{v}-x86_64{zst}"), 40);
    }
    // Mixed compression extensions.
    c.pkg(&mut r, "oldpkg-1.0-1-x86_64.pkg.tar.xz", 30);
    c.pkg(&mut r, "oldpkg-1.1-1-x86_64.pkg.tar.gz", 30);
    c.pkg(&mut r, "oldpkg-1.2-1-x86_64.pkg.tar.zst", 30);
    c.pkg(&mut r, "oldpkg-1.3-1-x86_64.pkg.tar", 30);
    c.pkg(&mut r, "oldpkg-1.4-1-x86_64.pkg.tar.xz", 30);
    // Many versions, to tell `-k 010` (octal 8 in the original) from 10.
    for i in 1..=12 {
        c.pkg(&mut r, format!("many-1.{i}-1-x86_64{zst}"), 20);
    }
    // Single version: never a candidate with keep >= 1.
    c.pkg(&mut r, format!("lonely-0.1-1-any{zst}"), 100);
    // Names with spaces.
    for v in ["1.0-1", "1.1-1", "1.2-1", "1.3-1"] {
        c.pkg(&mut r, format!("my pkg-{v}-any{zst}"), 50);
    }
    // Hidden files: ignored by the glob, found by find (--min-*time).
    for v in ["1-1", "2-1", "3-1", "4-1"] {
        c.pkg(&mut r, format!(".hidden-{v}-x86_64{zst}"), 0);
    }
    // Subdirectory: only scanned recursively (--min-*time).
    for v in ["5.0-1", "5.1-1", "5.2-1", "5.3-1"] {
        c.pkg(&mut r, format!("sub/zsh-{v}-x86_64{zst}"), 50);
    }
    c.pkg(
        &mut r,
        format!("sub/deeper/linux-5.15.arch1-1-x86_64{zst}"),
        50,
    );
    // Partial download: matches both the glob and find's pattern.
    c.push(&mut r, format!("linux-6.11.arch1-1-x86_64{zst}.part"));
    // Orphan signature, sig-like non-package, junk.
    c.push_exact(format!("ghost-1.0-1-x86_64{zst}.sig"), 566, T_MID, T_MID);
    c.push(&mut r, "README");
    c.push(&mut r, "core.db");
    // Malformed names.
    c.push(&mut r, format!("weird{zst}"));
    c.push(&mut r, format!("two-parts{zst}"));
    // A really big (sparse) file for GiB in the summary.
    for v in ["1.0-1", "1.1-1"] {
        c.push_exact(
            format!("huge-data-{v}-any{zst}"),
            5 * 1024 * 1024 * 1024 + 123,
            T_OLD_LO,
            T_OLD_LO,
        );
    }
    c.push_exact(format!("huge-data-1.2-1-any{zst}"), 7, T_FUTURE, T_FUTURE);
    c
}

/// A small cache used as the second `-c` in multi-cachedir scenarios. It
/// shares package names with [`standard`] so groups would merge if the
/// implementation (wrongly) pooled cachedirs.
pub fn secondary(seed: u64) -> CacheSpec {
    let mut r = Rng::new(seed.wrapping_add(1000));
    let mut c = CacheSpec::default();
    for v in ["6.0-1", "6.3.arch1-1", "6.4.arch1-1", "6.5.arch1-1"] {
        c.pkg(&mut r, format!("linux-{v}-x86_64.pkg.tar.zst"), 60);
    }
    for v in ["3.0-1", "3.1-1", "3.2-1", "3.3-1"] {
        c.pkg(&mut r, format!("only-here-{v}-any.pkg.tar.zst"), 60);
    }
    c
}

/// A cache of randomly named packages (deterministic by seed).
pub fn random(seed: u64, packages: usize) -> CacheSpec {
    let mut r = Rng::new(seed);
    let mut c = CacheSpec::default();
    const SYL: &[&str] = &[
        "lib", "py", "qt", "gtk", "x", "core", "net", "font", "kde", "go", "rs",
    ];
    const ARCH: &[&str] = &["x86_64", "x86_64", "x86_64", "any", "any", "i686"];
    const EXT: &[&str] = &[
        ".pkg.tar.zst",
        ".pkg.tar.zst",
        ".pkg.tar.zst",
        ".pkg.tar.xz",
        ".pkg.tar.gz",
    ];
    for i in 0..packages {
        let parts = 1 + r.below(3);
        let mut name = String::new();
        for p in 0..parts {
            if p > 0 {
                name.push('-');
            }
            name.push_str(r.pick(SYL));
        }
        name.push_str(&i.to_string());
        let arch = *r.pick(ARCH);
        let versions = 1 + r.below(6);
        for _ in 0..versions {
            let epoch = if r.below(10) == 0 {
                format!("{}:", 1 + r.below(2))
            } else {
                String::new()
            };
            let ver = format!("{}.{}.{}", r.below(4), r.below(12), r.below(30));
            let rel = if r.below(6) == 0 {
                format!("{}.{}", 1 + r.below(3), 1 + r.below(2))
            } else {
                (1 + r.below(4)).to_string()
            };
            let ext = *r.pick(EXT);
            let file = format!("{name}-{epoch}{ver}-{rel}-{arch}{ext}");
            if c.files.iter().any(|f| f.rel == file) {
                continue;
            }
            c.pkg(&mut r, file, 50);
        }
    }
    c
}

/// Installed package names from the real `pacman -Qq` (sorted), or an empty
/// list when pacman is unavailable.
pub fn installed_packages() -> Vec<String> {
    let out = match Command::new("pacman").arg("-Qq").output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let mut v: Vec<String> = String::from_utf8_lossy(&out)
        .lines()
        .map(str::to_owned)
        .collect();
    v.sort();
    v
}

/// A cache mixing names of really installed packages (picked deterministically
/// from `pacman -Qq`) with invented ones, for `-u`.
pub fn installed_mix(seed: u64) -> CacheSpec {
    let mut r = Rng::new(seed);
    let mut c = CacheSpec::default();
    let installed = installed_packages();
    let mut chosen: Vec<String> = Vec::new();
    for prefer in ["pacman", "bash", "coreutils", "glibc", "linux"] {
        if installed.iter().any(|p| p == prefer) {
            chosen.push(prefer.to_owned());
        }
    }
    if !installed.is_empty() {
        for _ in 0..4 {
            chosen.push(r.pick(&installed).clone());
        }
    }
    chosen.sort();
    chosen.dedup();
    for name in chosen
        .iter()
        .map(String::as_str)
        .chain(["not-installed-xyz", "zz-invented-pkg"])
    {
        for v in ["1.0-1", "1.1-1", "1.2-1", "1.3-1"] {
            c.pkg(&mut r, format!("{name}-{v}-x86_64.pkg.tar.zst"), 50);
        }
    }
    c
}

/// Cache with only uninteresting files (no package matches the glob).
pub fn no_packages() -> CacheSpec {
    let mut c = CacheSpec::default();
    c.push_exact("README", 10, T_MID, T_MID);
    c.push_exact("only-a-sig-1-1-any.pkg.tar.zst.sig", 566, T_MID, T_MID);
    c.push_exact("sub/nested-1-1-any.pkg.tar.zst", 3, T_OLD_LO, T_OLD_LO);
    c
}

/// Cache for exercising `--min-atime` / `--min-mtime` with exact boundaries:
/// every version of `agepkg` gets atime/mtime on both sides of [`T_MID`],
/// including exactly `T_MID` (the comparison is strict `<`).
pub fn ages() -> CacheSpec {
    let mut c = CacheSpec::default();
    let rows: &[(&str, i64, i64)] = &[
        ("1.0-1", T_MID - 100, T_MID - 100),
        ("1.1-1", T_MID - 1, T_MID + 1),
        ("1.2-1", T_MID + 1, T_MID - 1),
        ("1.3-1", T_MID, T_MID),
        ("1.4-1", T_MID - 50, T_MID - 50),
        ("1.5-1", T_FUTURE, T_FUTURE),
    ];
    for (v, a, m) in rows {
        c.push_exact(format!("agepkg-{v}-x86_64.pkg.tar.zst"), 1000, *a, *m);
        c.push_exact(format!("agepkg-{v}-x86_64.pkg.tar.zst.sig"), 10, *a, *m);
        c.push_exact(format!("other-{v}-any.pkg.tar.xz"), 2000, *a, *m);
    }
    c
}
