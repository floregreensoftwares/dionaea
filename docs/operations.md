# Running the server

`dionaea serve [--config file.toml]` reads one TOML file (optional) and
`DIONAEA_*` environment variables, which override the file. Every key below can
be set either way: the environment name is the key in capitals with the
`DIONAEA_` prefix (`spool_dir` is `DIONAEA_SPOOL_DIR`). An unknown key in the
file, a value that does not parse and a value outside its range stop the server
at start with a message naming the key.

## Listeners

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Where SDKs send events (`/v1/collect`, `/healthz`) |
| `ops_listen` | `127.0.0.1:9100` | `/healthz`, `/readyz` and `/metrics`, apart from the public port so they need not be reachable from outside |

`/readyz` answers 503 when the spool is failing or nearly full (95% of its
quota) or when keys and schemas cannot be read; a ClickHouse outage alone keeps
the node ready, because the spool absorbs it. `/metrics` is Prometheus text:
requests by outcome, events kept and dropped (by reason), connections, spool
size and age, batch inserts, retries and failures, and whether ClickHouse has
been prepared. Labels come from fixed sets, never from request content.

## Storage

| Key | Default | Meaning |
|---|---|---|
| `metadata_path` | `dionaea.db` | The SQLite file with sites, keys and schemas; set it to an absolute path |
| `spool_dir` | `spool` | The on-disk queue between the endpoint and ClickHouse; keep it on a disk that survives restarts |
| `spool_quota_bytes` | 1 GiB | The most disk the spool uses (at least 32 MiB). When it is full the endpoint answers 503 with `Retry-After` |
| `clickhouse_url` | `http://127.0.0.1:8123` | `http://host[:port]` or `https://host[:port]` |
| `clickhouse_user`, `clickhouse_password`, `clickhouse_database` | `default`, empty, `dionaea` | The password is never printed |
| `clickhouse_ca_file` | none | PEM file with the certificates to trust for `https://`; without it the public roots are trusted |
| `clickhouse_server_name` | the host | The name the ClickHouse certificate must carry |
| `geoip_path` | none | A MaxMind country or city database (`.mmdb`); without it events have no country |

The server creates its ClickHouse tables at start and keeps trying while
ClickHouse is down; events are accepted into the spool meanwhile.

## Limits

Every limit has a default and a range. Requests over a limit are answered with
the status listed in `wire-format.md`.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `key_rate_per_sec`, `key_burst` | 200, 400 | > 0, ≥ 1 | Requests per second and burst allowed per write key |
| `address_rate_per_sec`, `address_burst` | 50, 100 | > 0, ≥ 1 | The same per client address (an IPv6 client counts by its /64) |
| `tracked_keys` | 100000 | 1 to 10000000 | Keys and addresses each limiter remembers |
| `body_timeout_secs` | 10 | 1 to 300 | Time a client has to send the whole body |
| `header_timeout_secs` | 10 | 1 to 300 | Time a connection has to send the headers of a request; it also ends an idle keep-alive connection |
| `max_connections` | 10000 | 1 to 1000000 | Open connections at once; further ones are closed as soon as they open |
| `retry_after_secs` | 5 | 1 to 3600 | The `Retry-After` of a 503 |
| `max_event_age_days` | 30 | 1 to 3650 | Oldest accepted event, after correcting for the device clock |
| `max_event_future_secs` | 3600 | 0 to 86400 | Furthest ahead an event may be |
| `refresh_secs` | 2 | 1 to 3600 | How often keys and schemas are checked for changes; a revoked key stops working within this time |
| `batch_max_records`, `batch_max_bytes`, `batch_min_interval_ms` | 1000, 8 MiB, 200 | 1 to 1000, 64 KiB to 64 MiB, 0 to 60000 | How events are grouped into ClickHouse inserts |
| `dedup_capacity`, `dedup_window_secs` | 1000000, 600 (about 80 MB at the default capacity) | 0 to 100000000 (0 is off), 1 to 86400 | Event ids remembered to drop an event sent again |
| `shared_dedup` | true | true or false | Ask ClickHouse which events another node stored, and record this node's ids |
| `drop_flush_secs` | 5 | 1 to 300 | How often the drop counts of requests that kept no event are written |

## Behind a reverse proxy

By default the client address is the socket peer, so behind a proxy every
request would share the proxy's address. Set `trusted_proxy_hops` to the number
of proxies in front of the server (0 to 8) and `client_address_header` to the
header they append the client address to (default `x-forwarded-for`). The
client is then the entry that many places from the right; entries to its left
were written by the client and are ignored. Leave it at 0 when the server is
reachable directly: a client could otherwise choose its own address.

## What is added to events

The `User-Agent` header gives the platform, operating system, browser and
device class. The rules cover the common browsers and systems and use the
woothee dataset for the long tail; an agent nobody recognizes gets empty
fields, never a guess. Requests from known automated clients (crawlers,
headless browsers, command-line tools) are answered normally, but their events
are counted as dropped with the reason `bot`. With `geoip_path` set, the
country of the client address is stored. The address itself is used for the
lookup and the rate limit and is never stored or logged.

A request that kept no event (a crawler, a stale schema, a batch sent again)
costs no spool write: its drop counts are merged in memory and written every
`drop_flush_secs`. A crash loses the counts of that last interval, never an
event.

## Events sent twice

An SDK that loses a response sends the batch again with the same event ids.
Three layers keep the copy out of the tables:

1. Each node remembers the ids it stored in the last `dedup_window_secs`
   (up to `dedup_capacity`), in memory and in a journal next to the spool, so a
   restart does not forget them. The id is remembered only after the request
   is durable, so a request that failed with 503 can be sent again.
2. Before each insert the batcher asks ClickHouse which of the batch's ids
   another node already stored (table `event_ids`, one day of ids, no user or
   device identifier), leaves those out, and records the ids of the events it
   stores. This is what closes the gap between replicas. It costs one query
   and one small insert per batch; a single node can turn it off with
   `shared_dedup = false`.
3. The insert itself carries a token made from the batch's position in the
   spool, so a batch repeated after a failure is stored once.

What remains is a race: the same event arriving at two replicas at the same
moment, before either has recorded it, is stored twice. An SDK retry follows a
timeout, so the usual gap between the copies is far longer than a batch
interval. The ingest database user needs `SELECT` on `event_ids`; `dionaea`
grants it when it creates the users.
