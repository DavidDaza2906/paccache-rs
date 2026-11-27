#!/usr/bin/env bash
# Benchmark paccache (pacman-contrib 1.13.1) against paccache-rs.
#
#   bench/run.sh            synthetic cache (PKGS packages x VERSIONS versions)
#   bench/run.sh --real     the real /var/cache/pacman/pkg, dry run (-d) ONLY
#
# Environment:
#   PKGS=2000 VERSIONS=5    size of the synthetic cache (default 10 000 files)
#   SIG_EVERY=0             also create a .sig for every Nth package file (0 = none)
#   RUNS=10 WARMUP=2        iterations per command
#   ORIGINAL=/usr/bin/paccache
#   PACCACHE_RS=<path>      default: target/release/paccache-rs (built if missing)
#   OUT_DIR=<dir>           where the reports go (default: a temp dir, printed)
#   KEEP=1                  keep the synthetic cache instead of deleting it
#
# Uses hyperfine when available; otherwise falls back to a `date +%s%N` loop
# (mean/min/max over RUNS, no statistics beyond that). Never uses sudo and
# never runs -r or -m.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/.." && pwd)

real=0
case ${1-} in
	--real) real=1 ;;
	'') ;;
	-h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
esac

PKGS=${PKGS:-2000}
VERSIONS=${VERSIONS:-5}
SIG_EVERY=${SIG_EVERY:-0}
RUNS=${RUNS:-10}
WARMUP=${WARMUP:-2}
ORIGINAL=${ORIGINAL:-/usr/bin/paccache}
PACCACHE_RS=${PACCACHE_RS:-$repo/target/release/paccache-rs}
OUT_DIR=${OUT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/paccache_bench.XXXXXX")}
mkdir -p "$OUT_DIR"

if [[ ! -x $PACCACHE_RS && $PACCACHE_RS == "$repo/target/release/paccache-rs" ]]; then
	echo ">> building release binary" >&2
	cargo build --release --manifest-path "$repo/Cargo.toml" >&2
fi

# ---------------------------------------------------------------------------
# cache
# ---------------------------------------------------------------------------
# Timestamps: version v of every package gets atime = mtime = BASE + v days,
# so "--min-atime @MIN_ATIME" keeps the newest versions regardless of -k.
BASE=1640995200            # 2022-01-01
MIN_ATIME=$((BASE + 3 * 86400))

if (( real )); then
	cachedir=/var/cache/pacman/pkg
	MIN_ATIME_ARG='30 days ago'
else
	cachedir=$(mktemp -d "${TMPDIR:-/tmp}/paccache_benchcache.XXXXXX")
	(( ${KEEP:-0} )) || trap 'rm -rf -- "$cachedir"' EXIT
	MIN_ATIME_ARG="@$MIN_ATIME"
	echo ">> generating $((PKGS * VERSIONS)) files in $cachedir" >&2
	arches=(x86_64 x86_64 x86_64 any)
	for (( v = 1; v <= VERSIONS; v++ )); do
		files=()
		for (( p = 0; p < PKGS; p++ )); do
			name="pkg-$((p % 7))-name$p"
			arch=${arches[p % 4]}
			# alternate pkgrel/epoch shapes so vercmp has real work to do
			case $(( (p + v) % 4 )) in
				0) ver="1.$v.0-1" ;;
				1) ver="1.$v.0-2" ;;
				2) ver="2:0.$v-1.1" ;;
				3) ver="1.$v.0rc1-1" ;;
			esac
			f="$cachedir/$name-$ver-$arch.pkg.tar.zst"
			files+=("$f")
			if (( SIG_EVERY && p % SIG_EVERY == 0 )); then
				files+=("$f.sig")
			fi
		done
		# zero-byte files, one touch per version batch keeps this fast
		printf '%s\0' "${files[@]}" | xargs -0 touch -d "@$((BASE + v * 86400))"
	done
fi

count=$(find "$cachedir" -maxdepth 1 -type f | wc -l)
echo ">> cache: $cachedir ($count files)" >&2

# ---------------------------------------------------------------------------
# commands (dry run only)
# ---------------------------------------------------------------------------
declare -a labels=("-d (-k 3)" "-d -k 1" "-d --min-atime")
declare -a argsets=("-d" "-d -k 1" "-d --min-atime")

ours_ok=1
if ! "$PACCACHE_RS" -d -c "$cachedir" >/dev/null 2>&1; then
	echo "!! $PACCACHE_RS does not work yet (non-zero exit); benchmarking the original only" >&2
	ours_ok=0
fi

q() { printf '%q' "$1"; }

cmdline() { # prog argset-index
	local extra=''
	[[ ${argsets[$2]} == *--min-atime* ]] && extra=" $(q "$MIN_ATIME_ARG")"
	printf '%s %s%s -c %s' "$(q "$1")" "${argsets[$2]}" "$extra" "$(q "$cachedir")"
}

fallback_bench() { # report name cmd...
	local report=$1; shift
	{
		echo "| command | mean [ms] | min [ms] | max [ms] |"
		echo "|:---|---:|---:|---:|"
	} > "$report"
	local cmd i t0 t1
	for cmd in "$@"; do
		for (( i = 0; i < WARMUP; i++ )); do bash -c "$cmd" >/dev/null 2>&1 || true; done
		local -a times=()
		for (( i = 0; i < RUNS; i++ )); do
			t0=$(date +%s%N)
			bash -c "$cmd" >/dev/null 2>&1 || true
			t1=$(date +%s%N)
			times+=($(( t1 - t0 )))
		done
		printf '%s\n' "${times[@]}" | CMD=$cmd awk '
			{ s += $1; if (NR == 1 || $1 < mn) mn = $1; if ($1 > mx) mx = $1 }
			END { printf "| `%s` | %.1f | %.1f | %.1f |\n", ENVIRON["CMD"], s / NR / 1e6, mn / 1e6, mx / 1e6 }' >> "$report"
	done
	cat "$report"
}

for i in "${!argsets[@]}"; do
	report="$OUT_DIR/bench-$(( real ? 1 : 0 ))-$i.md"
	cmds=("$(cmdline "$ORIGINAL" "$i")")
	(( ours_ok )) && cmds+=("$(cmdline "$PACCACHE_RS" "$i")")
	echo
	echo "### ${labels[$i]} ($( (( real )) && echo real || echo synthetic) cache, $count files)"
	if command -v hyperfine >/dev/null; then
		hyperfine --warmup "$WARMUP" --runs "$RUNS" --export-markdown "$report" "${cmds[@]}" >&2
		cat "$report"
	else
		fallback_bench "$report" "${cmds[@]}"
	fi
done

echo
echo ">> reports in $OUT_DIR" >&2
command -v hyperfine >/dev/null || echo ">> hyperfine not found: used the date +%s%N fallback loop" >&2
