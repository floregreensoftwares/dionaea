#!/bin/sh
# Brings the development ClickHouse up with compose and checks that the built
# binary reaches it, prepares it and serves. Usage: scripts/verify-compose.sh [path-to-dionaea-binary]
set -eu

repo_root=$(git rev-parse --show-toplevel)
bin=${1:-$repo_root/.cargo-target/debug/dionaea}
log=$(mktemp)
work=$(mktemp -d)
pid=

cleanup() {
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    docker compose -f "$repo_root/compose.yaml" down -v >/dev/null 2>&1 || true
    rm -rf "$log" "$work"
}
trap cleanup EXIT

docker compose -f "$repo_root/compose.yaml" up -d --wait

DIONAEA_LISTEN=127.0.0.1:18090 \
    DIONAEA_METADATA_PATH="$work/meta.db" DIONAEA_SPOOL_DIR="$work/spool" \
    DIONAEA_CLICKHOUSE_USER=dionaea DIONAEA_CLICKHOUSE_PASSWORD=dionaea-dev \
    DIONAEA_CLICKHOUSE_DATABASE=dionaea "$bin" serve >"$log" 2>&1 &
pid=$!

for _ in $(seq 1 30); do
    if grep -q '"message":"clickhouse is ready"' "$log"; then
        break
    fi
    sleep 1
done

if ! grep -q '"message":"clickhouse is ready"' "$log"; then
    echo "FAIL  the binary did not report ClickHouse as ready" >&2
    cat "$log" >&2
    exit 1
fi
if [ "$(curl -fsS http://127.0.0.1:18090/healthz)" != "ok" ]; then
    echo "FAIL  /healthz did not answer ok" >&2
    exit 1
fi
echo "ok    ClickHouse is up, the binary prepared it and /healthz answers"
