#!/bin/sh
# Clones the repository into a temporary directory, installs the hooks the way
# a contributor would, and checks that real `git commit` calls are accepted or
# rejected as the commit-msg hook promises.
set -eu

repo_root=$(git rev-parse --show-toplevel)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

git clone --quiet --local "$repo_root" "$work/clone"
cd "$work/clone"
git config user.name "Hook Check"
git config user.email "hook-check@example.invalid"
git config commit.gpgsign false
sh scripts/install-hooks.sh >/dev/null

failures=0

expect_accept() {
    if git commit --quiet --allow-empty -m "$1" >/dev/null 2>&1; then
        echo "ok    accepted: $1"
    else
        echo "FAIL  should accept: $1"
        failures=$((failures + 1))
    fi
}

expect_reject() {
    if git commit --quiet --allow-empty -m "$1" >/dev/null 2>&1; then
        echo "FAIL  should reject: $1"
        failures=$((failures + 1))
    else
        echo "ok    rejected: $1"
    fi
}

expect_accept "chore(hooks): accept a well formed message"
expect_accept "fix(core): handle empty input

- Why: an empty body used to reach the parser.
- Change: return a typed error instead."

expect_reject "update stuff"
expect_reject "feat: no scope is fine but this ends with a period."
expect_reject "Feat(core): capitalized type"
expect_reject "feat(Core): capitalized scope"
expect_reject "feat(core,store): two scopes"
expect_reject "feat(core): this subject is far too long to be accepted because it keeps going past the limit"
expect_reject "feat(core): add the mvp"
expect_reject "feat(core): cover the next phase"
expect_reject "feat(core): add parser

Co-Authored-By: Someone <someone@example.invalid>"
expect_reject "feat(core): add parser

- Change: written with Claude"
expect_reject "feat(core): add parser

free prose instead of bullets"

if [ "$failures" -ne 0 ]; then
    echo "$failures hook check(s) failed" >&2
    exit 1
fi
echo "all hook checks passed"
