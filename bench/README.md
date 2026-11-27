# Benchmarks

`bench/run.sh` times `paccache` (pacman-contrib 1.13.1, `/usr/bin/paccache`)
against `paccache-rs`, always in **dry-run mode (`-d`)**. It never runs `-r`
or `-m` and never uses sudo.

```sh
bench/run.sh            # synthetic cache: 2 000 packages x 5 versions = 10 000 files
bench/run.sh --real     # the real /var/cache/pacman/pkg, read-only (-d only)
```

Three command lines are measured for each binary:

| label | arguments |
|---|---|
| `-d (-k 3)` | `-d -c DIR` (default keep = 3) |
| `-d -k 1` | `-d -k 1 -c DIR` |
| `-d --min-atime` | `-d --min-atime T -c DIR` (recursive `find` path in the original) |

`T` is `@<epoch>` (between the 3rd and 4th version) for the synthetic cache and
`30 days ago` for the real one.

The synthetic cache is created in a temporary directory with zero-byte files
named `pkg-N-nameM-<ver>-<arch>.pkg.tar.zst` (mixing `-1`, `-2`, `.1` pkgrels,
an epoch and `rc` versions, arches `x86_64`/`any`); version *v* gets
atime = mtime = 2022-01-01 + *v* days. It is deleted on exit unless `KEEP=1`.

## Knobs (environment variables)

| variable | default | meaning |
|---|---|---|
| `PKGS`, `VERSIONS` | 2000, 5 | size of the synthetic cache |
| `SIG_EVERY` | 0 | also create a `.sig` for every Nth package (0 = none) |
| `RUNS`, `WARMUP` | 10, 2 | iterations per command |
| `ORIGINAL` | `/usr/bin/paccache` | reference script |
| `PACCACHE_RS` | `target/release/paccache-rs` | our binary (built with `cargo build --release` if missing) |
| `OUT_DIR` | temp dir (printed) | Markdown reports, one per command line |
| `KEEP` | 0 | keep the synthetic cache |

## hyperfine

If `hyperfine` is in `PATH` it is used (`--warmup`, `--runs`,
`--export-markdown`). Otherwise the script falls back to a loop timing each
run with `date +%s%N` and reports mean/min/max in milliseconds (no standard
deviation, no outlier detection; process start-up of the `bash -c` wrapper is
included in both columns equally). Install it with `pacman -S hyperfine` for
publishable numbers.

If `paccache-rs` exits non-zero on `-d` (e.g. not implemented yet) only the
original is measured and a warning is printed.

## Reference numbers for the original (fallback loop, before integration)

Measured 2026-11-26 on the development machine (`RUNS=5`, synthetic cache on
tmpfs; real cache 10 492 files):

| command | synthetic (10 000 files) | real (10 492 files) |
|---|---:|---:|
| `paccache -d` | 292 ms | 157 ms |
| `paccache -d -k 1` | 546 ms | 269 ms |
| `paccache -d --min-atime …` | 337 ms | 230 ms |
