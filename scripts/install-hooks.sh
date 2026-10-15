#!/bin/sh
set -eu

repo_root=$(git rev-parse --show-toplevel)
git -C "$repo_root" config core.hooksPath .githooks
git -C "$repo_root" config commit.template .gitmessage
chmod +x "$repo_root/.githooks/commit-msg" "$repo_root/.githooks/pre-commit" "$repo_root/.githooks/pre-push" "$repo_root/.githooks/check-message"

echo "Configured dionaea hooks path as .githooks and commit template as .gitmessage"
