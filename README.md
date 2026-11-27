# paccache-rs

A drop-in Rust rewrite of `paccache`, the pacman package cache cleaner from
[pacman-contrib](https://gitlab.archlinux.org/pacman/pacman-contrib) (version 1.13.1).

`paccache` is a 431-line bash script that pipes the cache listing through `pacsort`,
an awk program, `stat`, `xargs`, `rm` and `mv`. `paccache-rs` does the listing,
sorting, version comparison, candidate selection and size accounting in-process,
and produces **the same output, exit codes and effects on disk** as the original.
It was checked against the original in differential test batteries (see
[How it was tested](#how-it-was-tested)).

It takes the same options:

```
paccache-rs <operation> [options] [target ...]

Operations:
  -d, --dryrun      perform a dry run, only finding candidate packages
  -m, --move <dir>  move candidate packages to "dir"
  -r, --remove      remove candidate packages

Options:
  -a, --arch <arch>       scan for "arch" (default: all architectures)
  -c, --cachedir <dir>    scan "dir" for packages. can be used more than once
  -f, --force             apply force to mv(1) and rm(1) operations
  -i, --ignore <pkgs>     ignore "pkgs", comma-separated, or "-" for stdin
  -k, --keep <num>        keep "num" of each package in the cache (default: 3)
      --min-atime <time>
      --min-mtime <time>  keep packages newer than the given date
      --nocolor           do not colorize output
  -z, --null              use null delimiters for candidate names
  -q, --quiet             minimize output
  -u, --uninstalled       target uninstalled packages
  -v, --verbose           increase verbosity. specify up to 3 times
  -h, --help / -V, --version
```

## Installation

### With cargo

```sh
cargo install --locked --git https://github.com/OWNER/paccache-rs
```

or from a checkout:

```sh
cargo build --release
install -Dm755 target/release/paccache-rs ~/.local/bin/paccache-rs
```

### Arch Linux (AUR)

A `PKGBUILD` is in [`packaging/PKGBUILD`](packaging/PKGBUILD) (the AUR package is
not published yet):

```sh
cd packaging && makepkg -si
```

The binary is installed as `/usr/bin/paccache-rs`, so it does **not** conflict with
`pacman-contrib`. Both can be installed side by side.

### Runtime requirements

The same tools the original needs, minus `pacsort` and `awk`: `pacman`
(`pacman-conf CacheDir` and `pacman -Qq` for `-u`), GNU coreutils (`date` for
`--min-atime/--min-mtime`, `rm`/`mv` for `-r`/`-m`), findutils (`xargs`), and `sudo`
when a non-root user removes or moves files from a root-owned cache.

## Using it as a drop-in replacement

Arguments, messages and exit codes are those of `paccache`, so scripts, pacman
hooks and systemd units can call it unchanged.

```sh
# shell alias
alias paccache=paccache-rs

# or a symlink earlier in PATH
ln -s /usr/bin/paccache-rs ~/.local/bin/paccache
```

For the `paccache.service` unit shipped with pacman-contrib, override its
`ExecStart=` (`systemctl edit paccache.service`) to point to `/usr/bin/paccache-rs`.

## Benchmark

Dry run (`-d`) wall time in ms, mean over 10 runs after 2 warmup runs (min–max in
parentheses), measured with [`bench/run.sh`](bench/run.sh). Machine: AMD Ryzen 7
5700X, btrfs on NVMe, Arch Linux (bash 5.3, gawk 5.4, glibc 2.44). `hyperfine` was
not installed, so the script's fallback loop was used. That loop times
`bash -c "<command>"`, so each figure includes a few ms of bash startup.

**Real cache** (`/var/cache/pacman/pkg`, 10 518 entries / 10 492 files, 35 GB),
`-d` only:

| command | paccache 1.13.1 | paccache-rs | speedup |
|:---|---:|---:|---:|
| `-d` (keep 3) | 128.6 (126.1–131.7) | 22.3 (20.8–25.1) | 5.8× |
| `-d -k 1` | 208.9 (205.1–215.4) | 27.3 (26.1–29.0) | 7.7× |
| `-d --min-atime '30 days ago'` | 136.0 (130.0–148.2) | 32.9 (31.1–34.0) | 4.1× |

**Synthetic cache** (2 000 packages × 5 versions = 10 000 empty files):

| command | paccache 1.13.1 | paccache-rs | speedup |
|:---|---:|---:|---:|
| `-d` (keep 3) | 504.3 (501.4–509.7) | 40.8 (39.8–42.2) | 12.4× |
| `-d -k 1` | 796.9 (775.8–841.8) | 55.4 (53.1–57.2) | 14.4× |
| `-d --min-atime @…` | 334.4 (328.6–337.3) | 38.4 (36.8–40.2) | 8.7× |

`-r`/`-m` still run `xargs rm`/`xargs mv` like the original (see below), so their
cost is dominated by the file operations themselves.

## Differences from paccache

Four intentional differences. Each one is covered by a `divergence_*` test that
asserts the behaviour of paccache-rs:

1. **`-k/--keep` accepts only decimal digits** (`[0-9]+`, base 10, up to
   `i64::MAX`). Anything else is rejected with the usual
   `argument to option -k must be a non-negative integer`. The original stores the
   value in a `declare -i` variable, so bash evaluates it as arithmetic before
   validating it. As a result `-k 010` means 8, `-k 0x2`, `-k 2#11`, `-k 1+1` and
   `-k abc` (= 0) are accepted, overflow wraps around, and `-k 08` or `-k 9z` print a
   bash error, abort option parsing and exit with status 0.
2. **A `%` in the `-m` directory is printed literally** in the summary line. The
   original passes that line to `printf` as a format string, so `%s` and other
   directives in the directory name are expanded.
3. **No `pacsort` crash.** When a cache directory path contains a `-` and a file
   in it matches `*.pkg.tar*` but has no `-` in its own name, `pacsort` 1.13.1
   segfaults and the original silently finds no candidates in that directory.
   paccache-rs sorts such a file like any other non-package name (plain version
   comparison of the path).
4. **`-V/--version`** prints paccache-rs' own version text.

Not handled, as in v0.1: translations (messages are always English, as with
`LANG=C`), and file names containing a newline, which the original splits into two
entries.

## How it was tested

Every behaviour was ported by reading the script and the tools it calls, and then
checked differentially against the real programs:

| battery | result |
|---|---|
| `tests/compat.rs`: original vs. paccache-rs on freshly generated fake caches, comparing exit code, stdout, stderr and the final file tree | **151 scenarios, 0 differences** |
| `divergence_*` tests asserting our behaviour for the 4 differences above (explicitly or against the original run with equivalent arguments) | **22 tests, all passing** |
| `vercmp` vs. `/usr/bin/vercmp` (`alpm_pkg_vercmp`) | **109 409 pairs** (9 409 edge cases + 100 000 random), 0 mismatches |
| `pacsort --files` ordering vs. `/usr/bin/pacsort` | **4 000 random lists / 152 163 paths**, 0 mismatches; real cache, 10 518 paths in 3 input orders, identical |
| awk `parse_filename` vs. gawk | 50 000 random paths, identical |
| option parsing vs. the script's `parseopts` + option loop | 85 option combinations, identical (help text byte-identical) |
| real cache, `-d` with `-v`, `-vv`, `-vvv`, `-z`, `-k0`, `-k1`, `-a`, `-i`, `-u`, `--min-atime`, `--min-mtime`, `-q`, in `C` and `en_US.UTF-8` locales, plus colour on a tty | byte-identical stdout, stderr and exit code |

The compat scenarios cover dry run, remove and move, `--keep`, verbosity levels,
NUL delimiters, `--arch` (including gawk escape and numeric comparison quirks),
whitelists, `--ignore` from arguments and stdin, `--uninstalled`, `--min-atime` /
`--min-mtime` (recursive `find` listing, unreadable subdirectories), `--quiet`,
multiple, relative, missing and default (`pacman-conf`) cache directories, relative
and odd move directories, `parseopts` error messages (ambiguous and unknown options,
missing arguments) and help.

Some quirks are reproduced on purpose. The `-v` output of `rm`/`mv` comes out in
gawk's hash-table order. When a non-root user needs `sudo`, `type -p sudo` prints
its path on stdout. `-c ''` as the first cache directory falls back to `pacman-conf`.
For `-r`/`-m`, paccache-rs runs the same `xargs -0 rm|mv` command lines as the
original, so the messages, quoting and exit statuses of coreutils match exactly.

Run them with:

```sh
cargo test --release                                       # everything except the long differentials
cargo test --release -- --include-ignored --skip dump_fixture   # also vercmp/pacsort/real-cache
```

The compat battery needs `/usr/bin/paccache` (pacman-contrib 1.13.1). It never
touches `/var/cache/pacman`: a `pacman-conf` shim points the default cache at a
temporary directory.

## License

GPL-2.0-or-later, the same as pacman-contrib. See [LICENSE](LICENSE).

paccache-rs is a port of `paccache` from pacman-contrib, Copyright (C) 2011 Dave
Reisner &lt;dreisner@archlinux.org&gt;. It reproduces its logic, messages and help
text, and parts of libmakepkg (`parseopts`, `message.sh`), `pacsort` and libalpm's
`alpm_pkg_vercmp` (Copyright (C) the pacman development team). All credit for the
original design goes to its authors.

## Roadmap

The other pacman-contrib scripts, in the same repository and with the same approach
(differential tests against the original):

- `checkupdates`
- `checkrebuild`
- `pacdiff`
