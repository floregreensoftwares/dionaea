#!/bin/sh
# Runs the Criterion benchmarks of the given crates on the base revision and
# on the working tree, one after the other on the same machine, and prints the
# change per benchmark. Timings only mean something relative to each other, so
# this never fails on a slowdown; it fails only if a benchmark cannot run.
#
# Usage: scripts/bench-compare.sh <base-revision> [crate...]   (default: dionaea-core)
# Crates live in crates/<name>/. Every file directly in a crate's benches/
# folder must be a bench target; helper modules go in a subfolder.
#
# BENCH_WARM_UP and BENCH_MEASURE are seconds. With BENCH_NOISE_FLOOR=1 the
# base is benchmarked a second time against the first, which shows how much
# unchanged code moves on this machine; read the real comparison against it.
set -eu

base=${1:?usage: bench-compare.sh <base-revision> [crate...]}
shift
crates=${*:-dionaea-core}
warm_up=${BENCH_WARM_UP:-1}
measure=${BENCH_MEASURE:-3}
noise_floor=${BENCH_NOISE_FLOOR:-0}

repo_root=$(git rev-parse --show-toplevel)
work=$(mktemp -d)
base_tree=$work/base
trap 'git -C "$repo_root" worktree remove --force "$base_tree" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

# One target directory for both trees, so the dependencies (the bulk of the
# build) compile once. Cargo names a path package's artifacts from its path
# inside the workspace, so the two trees would otherwise reuse the base's
# library for the branch and measure the same code twice; the workspace crates
# are therefore cleaned before each tree is built. Criterion's baselines live in
# one shared place. BENCH_TARGET_DIR lets CI keep the directory in its cache.
export CARGO_TARGET_DIR=${BENCH_TARGET_DIR:-$work/target}
export CRITERION_HOME=$work/criterion

# Drops the release-profile artifacts (benchmarks build there) of every
# workspace crate of a tree; `cargo clean -p` alone only cleans the dev profile.
clean_workspace_crates() {
    for dir in "$1"/crates/*/; do
        (cd "$1" && cargo clean --quiet --release -p "$(basename "$dir")")
    done
}

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
    (
        cd "$tree"
        cargo bench -p "$crate" $targets --locked -- "$@"
    )
}

timing() {
    printf -- '--warm-up-time %s --measurement-time %s' "$warm_up" "$measure"
}

git -C "$repo_root" worktree add --detach --quiet "$base_tree" "$base"

for crate in $crates; do
    if [ ! -d "$repo_root/crates/$crate/benches" ]; then
        echo "== $crate: no benchmarks in the working tree, skipped"
        continue
    fi
    if [ ! -d "$base_tree/crates/$crate/benches" ]; then
        echo "== $crate: the base revision has no benchmarks; nothing to compare to"
        # shellcheck disable=SC2046
        clean_workspace_crates "$repo_root"
        run_benches "$repo_root" "$crate" $(timing)
        continue
    fi

    echo "== $crate: base revision ($base)"
    # shellcheck disable=SC2046
    clean_workspace_crates "$base_tree"
    run_benches "$base_tree" "$crate" --save-baseline base $(timing) >/dev/null
    if [ "$noise_floor" = 1 ]; then
        echo "== $crate: noise floor (the base revision against itself)"
        # shellcheck disable=SC2046
        run_benches "$base_tree" "$crate" --baseline base $(timing)
    fi
    echo "== $crate: working tree against the base revision"
    # shellcheck disable=SC2046
    clean_workspace_crates "$repo_root"
    run_benches "$repo_root" "$crate" --baseline-lenient base $(timing)
done
