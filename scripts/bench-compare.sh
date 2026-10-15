#!/bin/sh
# Runs the Criterion benchmarks of the given crates on the base revision and
# on the working tree, one after the other on the same machine, and prints the
# change per benchmark. Timings only mean something relative to each other, so
# this never fails on a slowdown; it fails only if a benchmark cannot run.
#
# Usage: scripts/bench-compare.sh <base-revision> [crate...]   (default: dionaea-core)
# Crates live in crates/<name>/. BENCH_WARM_UP and BENCH_MEASURE are seconds.
set -eu

base=${1:?usage: bench-compare.sh <base-revision> [crate...]}
shift
crates=${*:-dionaea-core}
warm_up=${BENCH_WARM_UP:-1}
measure=${BENCH_MEASURE:-3}

repo_root=$(git rev-parse --show-toplevel)
work=$(mktemp -d)
base_tree=$work/base
trap 'git -C "$repo_root" worktree remove --force "$base_tree" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

# One target directory for both trees, so Criterion finds the saved baseline.
export CARGO_TARGET_DIR=$work/target

# `--bench <name>` for every file in a crate's benches folder. Naming the
# targets keeps the library's own test harness, which rejects Criterion's
# options, out of the run (also for revisions that predate `bench = false`).
bench_targets() {
    for file in "$1"/*.rs; do
        printf -- '--bench %s ' "$(basename "$file" .rs)"
    done
}

# $1 tree, $2 crate, then the Criterion options.
run_benches() {
    tree=$1
    crate=$2
    shift 2
    targets=$(bench_targets "$tree/crates/$crate/benches")
    # shellcheck disable=SC2086
    (cd "$tree" && cargo bench -p "$crate" $targets --locked -- "$@")
}

git -C "$repo_root" worktree add --detach --quiet "$base_tree" "$base"

for crate in $crates; do
    if [ -d "$base_tree/crates/$crate/benches" ]; then
        echo "== $crate: base revision ($base)"
        run_benches "$base_tree" "$crate" --save-baseline base \
            --warm-up-time "$warm_up" --measurement-time "$measure" >/dev/null
    else
        echo "== $crate: the base revision has no benchmarks; nothing to compare to"
    fi
done

for crate in $crates; do
    echo "== $crate: working tree"
    if [ -d "$base_tree/crates/$crate/benches" ]; then
        run_benches "$repo_root" "$crate" --baseline-lenient base \
            --warm-up-time "$warm_up" --measurement-time "$measure"
    else
        run_benches "$repo_root" "$crate" \
            --warm-up-time "$warm_up" --measurement-time "$measure"
    fi
done
