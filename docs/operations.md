# Running the server

`dionaea serve [--config file.toml]` reads one TOML file (optional) and
`DIONAEA_*` environment variables, which override the file. Every key below can
be set either way: the environment name is the key in capitals with the
`DIONAEA_` prefix (`spool_dir` is `DIONAEA_SPOOL_DIR`). An unknown key in the
file, a value that does not parse and a value outside its range stop the server
at start with a message naming the key.

There are two kinds of setting. The ones in **Listeners** and **Storage** are
needed before anything can be read, or are secret: they come from the file and
the environment and change with a restart. The ones in **Limits** are
operational and can also be stored in the metadata store with
`dionaea settings`, which a running server follows without a restart (see
[Changing limits while the server runs](#changing-limits-while-the-server-runs)).

## Listeners

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Where SDKs send events (`/v1/collect`, `/healthz`) |
| `ops_listen` | `127.0.0.1:9100` | `/healthz`, `/readyz` and `/metrics`, apart from the public port so they need not be reachable from outside |
| `api_listen` | `127.0.0.1:8090` | The [API](api.md) for members and scripts. Put TLS in front of it before exposing it |
| `secure_cookies` | `true` | Whether the session cookie carries `Secure`; set `false` only for plain HTTP in local development |

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
| `erasure_key`, `erasure_key_file` | none | A random secret of 32 to 512 characters, or a file that holds it. Needed only to erase users (see below); the same on every server that shares the metadata |
| `erasure_previous_keys`, `erasure_previous_keys_file` | none | Earlier erasure keys, kept after the key was replaced so older erasures are still recognised: a comma-separated list in the environment, an array in the file, or a file with one key per line |
| `spool_dir` | `spool` | The on-disk queue between the endpoint and ClickHouse; keep it on a disk that survives restarts |
| `spool_quota_bytes` | 1 GiB | The most disk the spool uses (at least 32 MiB). When it is full the endpoint answers 503 with `Retry-After` |
| `clickhouse_url` | `http://127.0.0.1:8123` | `http://host[:port]` or `https://host[:port]` |
| `clickhouse_user`, `clickhouse_password`, `clickhouse_database` | `default`, empty, `dionaea` | The password is never printed |
| `clickhouse_query_user`, `clickhouse_query_password` | none | The ClickHouse user reports are read with. Give it `SELECT` and nothing else (`GRANT SELECT ON dionaea.* TO ...`); without it the user above reads too |
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
| `erase_grace_secs` | 10 | 0 to 3600 | How long erasure waits for every server to know a user was erased before deleting (never less than twice `refresh_secs`) |
| `erase_finish_after_secs` | 86400 | 0 to 604800 | How long an erased user stays hidden before the rows are physically removed, so a day of requests share one rewrite |
| `erase_max_attempts` | 10 | 1 to 100 | How many times in a row an erasure step may meet trouble that retrying could fix before the request fails |
| `erase_failed_keep_days` | 30 | 1 to 3650 | How long a failed erasure request keeps the user id and devices in the clear so that it can be retried; after that they are forgotten |
| `retention_default_days` | 395 | 1 to 3650 | How long a site keeps its analytics data unless it has its own number |
| `retention_interval_secs` | 86400 | 60 to 604800 | How often old data is removed |

## Changing limits while the server runs

Every key of the **Limits** table is an operational setting. Its value is the
first that exists of: the value stored in the metadata store, the environment,
the file, the default.

```
dionaea settings list                      # name, value in force, source, range, meaning
dionaea settings get key_burst             # the value in force, alone
dionaea settings set key_burst 800         # store a value (takes effect after a short delay)
dionaea settings set key_burst 800 --now   # take effect on each server's next refresh
dionaea settings set key_burst 800 --after 30   # take effect 30 seconds from now
dionaea settings unset key_burst           # back to the environment, file or default
```

`set` checks the value against the same range the file is checked against and
refuses a name that is not an operational setting (listing the ones that are);
it stores the number the way the server shows it (`0800` and `800` are the same
setting). The commands read the same configuration as `serve`
(`--config`, `DIONAEA_*`, notably `metadata_path`), so they act on the same
database.

A change takes effect at a stated moment, the same for every server that reads
the store. The default is one refresh interval plus a second from now, long
enough for every server to have read the change before it is due; `--after N`
chooses another delay and `--now` skips the wait (each server then switches on
its own next refresh, so servers may differ for up to `refresh_secs`). Until
the moment arrives the old value stays in force and `dionaea settings list`
shows what is coming (`; 900 in 3 s`). Servers check the clock every tenth of a
second while a change is waiting, so they switch within a fraction of a second
of each other, as far as their clocks agree; keep them synchronised (NTP).

Once a change is due, a running server applies it in place: the rate limits
and the number of tracked keys, the event id memory, the batch size and the
cross-node check, the connection limit and the header time (a connection
already open keeps the time it was given), the body time, the event time
window, the proxy settings and the flush time of drop counts. Nothing is
dropped or restarted; a request that is being handled finishes under the
settings it started with.

A stored value wins over the environment and the file, so editing the file
will not change a setting that has a stored value. `dionaea settings list`
says so (`stored (overrides file: 555)`), and the server logs a warning for each
such setting. `dionaea settings unset` gives the file or the environment its
say again.

A stored value that is out of range or no longer a known setting (for example
after an upgrade removed it, or a hand edit of the database) is ignored with a
warning in the log; `dionaea_settings_stored` and `dionaea_settings_skipped` in
`/metrics` show how many are stored and how many were ignored. The server never
refuses to start because of a stored value.

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

## What is checked about failures and memory

These run in CI (`cargo test -p dionaea --test serve -- --ignored`, with ClickHouse for the last two):

- **Logs.** A server at `trace` level handles requests with a key, a client address (IPv4 and IPv6, through a trusted proxy hop), a user agent, an origin and hostile bodies. None of those appear in what it logs.
- **Memory.** Forty-eight concurrent senders post 100-event bodies for twenty seconds with ClickHouse unreachable and a 32 MiB spool quota. The spool fills, requests are then refused with 503, and the process stays under a peak resident size of 256 MiB (a ceiling, not a measurement of the typical case: a debug build peaks near 50 MiB there). The queue between the sockets and the spool is bounded by the connection cap, and the spool by its quota.
- **Outage.** ClickHouse is cut while the server runs: every request is still acknowledged, `/readyz` stays 200, the batcher reports failing, nothing reaches the tables, and after the connection is restored every acknowledged event is stored exactly once.
- **Kills.** The server is killed six times at different moments while a client sends without pause. Every event acknowledged before a kill is in the tables at the end; events sent but not acknowledged may or may not be.

Not covered: a disk that fails or fills under the spool, a ClickHouse that answers slowly rather than not at all, and load heavier than one machine's loopback can produce.

## Identity

`POST /v1/identify` (see `docs/wire-format.md`) writes two ClickHouse tables, created by the start-up migrations like the others:

- `identity_alias`: one row per anonymous id and user pair, with the time of the latest link. The ingest user can insert into it and cannot read it.
- `user_traits`: the user's traits as of the latest identify that carried a declared trait, as two maps (text and numeric).

Both are `ReplacingMergeTree` tables, so a repeated identify is harmless; read them with `FINAL` (or `argMax` over the link time) until background merges have run. To list the events of a user through the link:

```sql
SELECT count() FROM events WHERE (site_id, anon_id) IN
  (SELECT site_id, anon_id FROM identity_alias FINAL WHERE user_id = 'u_42')
```

Metrics: `dionaea_identifies_total{result="linked"|"bot"}`, `dionaea_identify_traits_stripped_total`, and `dionaea_batcher_rows_total{table="identity"}`.

Retention for both tables is not done yet; erasure covers them (see below).

## Erasing a user

```
dionaea erase request --site 1 --user u_42 --by ops
dionaea erase request --site 1 --anon <device id>
dionaea erase list [--state requested|hidden|complete|failed]
dionaea erase show 3
dionaea erase retry 3
```

A request is recorded in the metadata store and from that moment every server drops what belongs to the user: an identify for the user is acknowledged and ignored, and so is anything from a device that was ever linked to them. The server's erasure job then:

1. finds every anonymous id linked to the user in `identity_alias` and keeps a permanent hash of each (and of the user id) so the device stays blocked when it keeps sending;
2. waits for every server to have loaded the hashes, then deletes the user's rows and the rows of those devices from `events`, `identity_alias` and `user_traits` with a lightweight delete. Queries stop returning them at once; the request is `hidden`;
3. after `erase_finish_after_secs`, deletes once more (a slow server may have inserted some), rewrites the data without the deleted rows (`APPLY DELETED MASK`, one rewrite for all the requests due together, and only of the monthly partitions of `events` that still hold a deleted row of theirs; the small tables of links and traits are rewritten whole) and counts directly, marked rows included, that none is left. Then the request is `complete`; if any remains it is `failed` with the counts.

Rows that were already in a server's spool are left out when they are inserted, so a replay cannot bring the user back.

**One server does the work.** Servers that share a metadata store compete for a lease named `erasure` (30 seconds, renewed every 10 while work runs). The holder hides and finishes requests; the others stand by, and take over within the lease time if the holder dies, or at once if it stops cleanly. A server that finds the lease taken while it works (it stalled for longer than the lease) finishes the statement it is in and starts nothing more, and a request is also held by whoever works on it, so two servers never work on one request. `dionaea_erasure_leader` is 1 on the server that leads.

**Trouble is bounded.** A failure that retrying could fix (ClickHouse unreachable, a statement that times out) puts the request off for 30 seconds, then 60, 120 and so on up to an hour, and counts an attempt; after `erase_max_attempts` in a row the request fails with the last message (`gave up after N attempts`). Any progress starts the count again. A request ClickHouse rejects fails at once with the reason. A failed request can be put back with `erase retry`; it keeps the user id and the devices in the clear for that, but only for `erase_failed_keep_days`. After that they are forgotten (the hashes keep blocking the user and the devices, and the audit line stays), `erase retry` refuses with a message, and asking for the erasure again with `erase request` starts a new request. `dionaea erase list` shows the attempts of each request. The job needs the ClickHouse user `serve` runs migrations with (it uses `ALTER` and `DELETE`).

Guarantee, with the defaults: invisible to queries within a few seconds of the request reaching a server that has reloaded the metadata, and gone from disk within a day and the time one rewrite takes. If a device is shared, erasing one user removes the rows of every device that was ever linked to them, including another user's on the same device.

**Erasing a device instead of a user.** Events can only be found through a user if the device was linked to one. For a person who gives you their device id, or whose device never identified itself (or was reset before it did), `--anon <device id>` erases that device alone: its events and every link to it, whoever the links were to, and it is blocked from then on. The traits of the users it was linked to stay, since they belong to the user. The request goes through the same states, the same two steps and the same verification as a user's, and `dionaea erase list` shows `device` or `user` in its last column. What cannot be done is to find events of a device nobody names and that never identified: nothing connects them to a person.

Tombstones are never removed, so a server learns of new ones by asking only for what came after the last it saw, in batches, and keeps 128 bits of each hash (a few dozen bytes with the set's overhead: a million erased devices are tens of megabytes). A metadata change that adds none reads none.

**The erasure key.** The hashes kept for erased users and devices are HMAC-SHA-256 under a secret you configure, so someone who has only the metadata file cannot test whether a guessable id such as `u_42` was erased. Make one with `openssl rand -hex 32` and give every server and every `dionaea` command the same value (`DIONAEA_ERASURE_KEY`, or `DIONAEA_ERASURE_KEY_FILE` for a file such as a container secret). Nothing needs it until the first erasure is requested; asking without one is refused with that instruction. The key itself is never stored, only a value derived from it next to each tombstone, and a server or command that lacks a key some tombstone was made with **refuses to start** (or to read or add erasures), naming the first characters of that value, instead of running as if nobody had been erased.

**Replacing the key.** Put the new secret in `erasure_key` and move the old one to `erasure_previous_keys`. New erasures use the new key; every configured key is tried when an id is looked up, so older erasures keep blocking. Every key some tombstone was made with must stay configured: the identifiers are gone, so the hashes made with an old key cannot be recomputed under a new one. A key that leaks therefore stops protecting future erasures once replaced, but the tombstones already made with it stay as testable as unkeyed ones would have been; nothing else is exposed. Each previous key costs one more HMAC (about a microsecond) per event, and only while erasures exist.

What the metadata store keeps: the user id and the devices in the clear until the request is complete, then only the keyed hashes and an audit line (who asked, when, counts). `event_ids` holds only random event ids and expires after a day; the rollups hold no identifier.

Metrics: `dionaea_identifies_total{result="erased"}`, `dionaea_batcher_erased_rows_total`, and drops with reason `erased` in `dionaea_events_dropped_total`.

The job's own metrics: `dionaea_erasure_requests{state}` (requests by state), `dionaea_erasure_retrying` (waiting for their next attempt), `dionaea_erasure_oldest_open_seconds` (how long the oldest unfinished request has waited; alert on it), `dionaea_erasure_leader`, and the counters `dionaea_erasure_hidden_total`, `_completed_total`, `_failed_total`, `_trouble_total` and `_gave_up_total` and `_purged_total` of this server.

Requests are for one subject: `--user` or `--anon`, not both.

Tested on ClickHouse 26.8 and on 25.8 (the previous long-term-support release). Not covered: erasing rows that are still only in a server that is down, until it starts again and reloads the metadata; and `sessions`, `errors` and `measurements`, which do not exist yet and will join the list of identifier tables a test enforces.

## Sessions

The SDK generates the session id and sends it with every event. A materialized view (`sessions_mv`) summarizes the events that carry one into the `sessions` table as they are inserted: first and last event time, how many events, the entry and exit screen, how many distinct screens, and the platform, app version and country of the first event. Rows of one session that arrive in separate inserts are merged in the background and, until then, read with `GROUP BY`:

```sql
SELECT session_id, anon_id, min(first_ts), max(last_ts), sum(events),
       argMinMerge(entry_screen), argMaxMerge(exit_screen), uniqMerge(screens)
FROM sessions GROUP BY site_id, session_id, anon_id
```

A session with one event is a bounce. Events sent without a session id (plain HTTP integrations) are not in the table; the view `derived_sessions` groups them per device at query time with the 30 minute inactivity rule (a gap of more than 30 minutes starts a new session), with `session_number` counting a device's sessions in order.

The view runs with the rights of the user that migrated, so the ingest user, which may only insert into `events`, needs nothing else. `sessions` holds the device id and is one of the tables erasure covers.

Not covered: events stored before the sessions migration ran are not summarized (the migration does not backfill, since a backfill would count events twice when events arrive meanwhile), and the worst error kind of a session arrives with the error events.

## Retention

Every site keeps its analytics data (events and the sessions summarized from them) for `retention_default_days`, 395 by default (13 months), unless it has a number of its own:

```
dionaea retention set --site 1 --days 90
dionaea retention unset --site 1
dionaea retention list
dionaea retention preview
```

`list` prints, for each site, the family (`analytics`), the days and whether the number is the site's own or the default. `preview` counts, per site and table, the rows the next run would remove at the retention each site has now, and deletes nothing: use it before lowering a number, because a run cannot be undone. Setting a number deletes nothing by itself: the retention job applies it at its next run, once per `retention_interval_secs` (a day). One server takes each run, whichever finds it unclaimed in the metadata store, so any number of servers can run.

A run works in three steps. Each site's rows older than its cutoff are deleted with a lightweight delete, which hides them from queries at once. A month of `events` or `sessions` that is entirely older than the cutoff of the site that keeps data the longest is dropped whole, which costs nothing. The months older than the newest cutoff that still hold deleted rows are rewritten, so the data leaves the disk. A run that fails (ClickHouse down, a timeout) is tried again 15 minutes later and not after a whole interval; `dionaea_retention_failing` is 1 until one succeeds.

Metrics: `dionaea_retention_runs_total`, `_failures_total`, `_rows_expired_total`, `_partitions_dropped_total`, `_partitions_rewritten_total`, `dionaea_retention_failing` and `dionaea_retention_last_success_timestamp_seconds` (alert when it is older than a few intervals).

The `events` table used to carry a rule of its own, 13 months for every site. A migration replaces it with one that cannot fire in the life of the product (60 years), so a site can keep its data longer than 13 months; it applies to databases made before as well. The links and traits of identified users (`identity_alias`, `user_traits`) have no age: they are removed by erasure. Raw errors and measurements will have a family of their own when they exist.

Not covered: a site that is deleted from the metadata store keeps its rows until a month holding only them falls out of every remaining site's cutoff; and the data of two sites that share a month is never dropped whole before the site that keeps longer is done with it, only deleted row by row.

## Members and read tokens

People log in to the [API](api.md) with an address and a password, and scripts read reports with a token. Nobody can log in until the first member exists, and it comes from the command line:

```
echo 'a long password' | dionaea member create ana@example.com --name Ana --organization Acme --role owner
```

The password is read from standard input and must have 12 to 256 characters. Roles are `viewer`, `admin` and `owner`, per organization; an organization always keeps at least one owner. `dionaea member list|role|disable|enable|password` manage the rest, and the same can be done through the API by an owner.

`dionaea token issue --site 1` prints a read token once; it can read that site's reports and nothing else, and `dionaea token revoke` ends it. Only a SHA-256 of the token is stored, so a lost token is replaced, not recovered.

Failed logins are counted per address and per client in memory, ten in fifteen minutes. With several servers the count is per server. Sessions last seven days, end when the password changes or the member is disabled, and are not extended by use.

Reports are read with `clickhouse_query_user` when it is set. Each read has a time limit of 30 seconds and a row limit of 20,000, and a node runs at most eight at once; the [API document](api.md) lists the rest.
