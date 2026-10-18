# API

The API for people and scripts: logging in, administering organizations,
sites, keys, schemas, retention and erasure, and reading reports. It is JSON
under `/api/v1/`, served on `api_listen` (default `127.0.0.1:8090`), apart from
the ports events arrive on. It is not meant to be exposed without TLS in front
of it; see [Running the server](operations.md).

**Status: draft.** Like the [wire format](wire-format.md), it is declared
stable only when the first release is cut.

## Who is asking

A request carries one of:

- **A session cookie**, from `POST /api/v1/login`. The cookie is `HttpOnly`,
  `SameSite=Lax` and `Secure` (unless `secure_cookies` is turned off for local
  development) and lasts seven days. A member has a role in each organization
  they belong to:

  | Role | May |
  |---|---|
  | `viewer` | read reports and see sites |
  | `admin` | also manage sites, keys, schemas, read tokens, retention, erasure and list members |
  | `owner` | also add members, change their roles and start another organization |

- **A read token**, as `Authorization: Bearer rt_...`. A token is issued for
  one site and can only read that site's reports (`POST .../query`) and see the
  site. It cannot administer anything, not even its own site, and a request for
  another site answers `404`.

- **An upload token**, as `Authorization: Bearer ut_...`, issued for one site by
  an administrator. It can upload symbol files (below) to that site and do
  nothing else: not read a report, not list files, not see the site. A read
  token cannot upload and an upload token cannot read.

Neither passwords nor tokens are stored: a password is kept as an argon2id
hash (19 MiB, two passes), a session or token as the SHA-256 of a random
256-bit secret. A token is shown once, when it is issued.

### Refusals

- `401` — no valid session or token.
- `404` — the thing does not exist, **or** the caller has no standing in the
  organization that owns it. Both answer the same, so ids cannot be probed.
- `403` — the caller can see it but lacks the role.
- `429` — too many failed logins from an address (an IPv6 client counts as its
  /64) or for an account (ten in fifteen minutes), too many passwords being
  checked at once (four per node), or too many reports being read at once;
  `Retry-After` says when to try again.

A state-changing request (`POST`, `PUT`, `DELETE`) that carries an `Origin`
header must name the server's own `Host`; anything else is `403
cross_origin`. This is on top of `SameSite=Lax`. A request without `Origin`,
such as a script's, is not affected.

Every error is `{"error": {"code": "...", "message": "...", "details": ...}}`;
`code` is stable, `message` is for people. Every response is
`Cache-Control: no-store`.

## Routes

Bodies are `application/json` (except a schema file) and unknown fields are
refused.

| Method and path | Needs | Does |
|---|---|---|
| `POST /login` | — | `{email, password}`; sets the cookie; answers the member and their roles |
| `POST /logout` | session | ends the session |
| `GET /me` | any | who the caller is |
| `PUT /me/password` | session | `{current, new}`; ends every session of the member |
| `GET /organizations` | session | the organizations the member belongs to |
| `POST /organizations` | owner of any | `{name}`; the caller becomes its owner |
| `GET /organizations/{org}/sites` | viewer | its sites |
| `POST /organizations/{org}/sites` | admin | `{name}` |
| `GET /organizations/{org}/members` | admin | its members and roles |
| `POST /organizations/{org}/members` | owner | `{email, name, password, role}` |
| `PUT /organizations/{org}/members/{member}` | owner | `{role}`; `null` removes. The last owner stays |
| `GET /sites/{site}` | viewer or its token | the site and its time zone |
| `PUT /sites/{site}/time-zone` | admin | `{time_zone}`, an IANA name ClickHouse knows |
| `GET`, `POST /sites/{site}/keys` | admin | list; issue `{label, origins}` |
| `DELETE /sites/{site}/keys/{key}` | admin | revoke |
| `GET /sites/{site}/schema/versions` | admin | published versions |
| `POST /sites/{site}/schema` | admin | the schema file as the body; `?dry_run=true` only lists the changes |
| `POST /sites/{site}/schema/versions/{n}/retire` | admin | stop accepting a version (never the last) |
| `GET`, `POST /sites/{site}/tokens` | admin | list; issue `{label}`; the answer holds `token`, once |
| `DELETE /sites/{site}/tokens/{id}` | admin | revoke |
| `GET`, `PUT`, `DELETE /sites/{site}/retention` | admin | days kept (`{days}`); `DELETE` goes back to the default |
| `GET /sites/{site}/retention/preview` | admin | rows per table the next run would remove at `?days=` (or the days kept now); deletes nothing |
| `GET`, `POST /sites/{site}/erasures` | admin | list (`?state=`); request `{user_id}` or `{device_id}` |
| `GET /sites/{site}/erasures/{id}` | admin | one request and what the job reported |
| `POST /sites/{site}/erasures/{id}/retry` | admin | put a failed request back |
| `GET`, `POST /sites/{site}/upload-tokens` | admin | list; issue `{label}`; the answer holds `token`, once |
| `DELETE /sites/{site}/upload-tokens/{id}` | admin | revoke |
| `GET /sites/{site}/symbols` | admin | the symbol files, `?release=` for one release, with the bytes kept and the limit |
| `POST /sites/{site}/symbols` | admin or its upload token | `?release=&kind=&name=` and the file as the body (below) |
| `DELETE /sites/{site}/symbols` | admin | `?release=&kind=&name=` for one file, `?release=` alone for all of a release |
| `POST /sites/{site}/query` | viewer or its token | a report |

Adding a member answers `409` when the address already has an account, so an
owner can learn whether an address is registered in another organization. There
is no invitation by e-mail to hide that behind.

Erasure listings (the newest 500 of the site) show the hash of the user or device id and never the id.
Every administrative change is logged with who made it.

## Reports

`POST /sites/{site}/query` takes a typed query, never SQL:

```json
{
  "from_ms": 1760000000000,
  "to_ms": 1760604800000,
  "filters": [{"dimension": "country", "values": ["BR", "PT"]}],
  "report": {"type": "series", "measure": "users", "event": "checkout",
             "granularity": "day",
             "breakdown": {"dimension": "property", "event": "checkout", "name": "coupon"}}
}
```

The interval is `[from_ms, to_ms)` in milliseconds since the Unix epoch. Days,
weeks (from Monday) and months are cut in the site's time zone; buckets with no
events have no row. Users are counted exactly, not estimated.

| `report.type` | Fields | Answer |
|---|---|---|
| `overview` | — | users and events, each with the value for the period of the same length just before; sessions, average screens, average seconds and bounces (a session of one event) with the same comparison |
| `series` | `measure` (`events`, `users`), `event`, `granularity` (`hour`, `day`, `week`, `month`), `breakdown` | `points`: `{bucket, key, value}` |
| `top` | `dimension`, `measure`, `event`, `limit` | `rows`: `{key, value}`, most frequent first |
| `screens` | `limit` | `rows`: `{screen, views, viewers, taps, tappers}` |
| `elements` | `screen`, `limit` | `rows`: `{screen, element, taps, tappers}` |
| `sessions` | `breakdown` (`country`, `platform`, `app_version`, `entry_screen`, `exit_screen`), `limit` | `rows`: `{key, sessions, avg_screens, avg_seconds, bounces}` |

A dimension is `country`, `platform`, `os`, `browser`, `device_class`,
`app_version`, `screen`, `event`, or
`{"dimension": "property", "event": "...", "name": "..."}` for a text-like
property the event declares. Filters take any dimension and keep what has one
of the values; the `overview` leaves its session figures out (`sessions` is
`null`) when a filter has no meaning for sessions, and the `sessions` report
refuses such a filter.

### Limits and typed errors

A query that breaks a rule is refused with `400` and a code, before ClickHouse
is asked:

| Code | When |
|---|---|
| `undeclared_event` | the event is neither built in nor declared by an active schema version |
| `undeclared_property` | the event does not declare the property |
| `not_groupable` | the property is a number, or its kind differs between versions |
| `conflicting_events` | the query names two events, so nothing could match |
| `range_too_large` | longer than 366 days (31 for hourly buckets) |
| `empty_range`, `time_out_of_range` | the interval is empty, before 1970 or after 2100 |
| `breakdown_too_high` | the breakdown has more than 50 distinct values |
| `too_many_filters`, `invalid_filter_values`, `invalid_limit` | more than 8 filters; a filter with no values, more than 100, or one empty or over 256 bytes; a limit outside 1 to 100 |
| `filter_not_available` | a session report filtered by something sessions do not have |

Reads run with a limit of 30 seconds, 20,000 result rows and 16 MiB of answer;
a read that exceeds one fails (`504 too_slow`, `422 too_many_rows`) rather than
returning part of the answer. At most eight reports run at once per node.

Every value, name and time reaches ClickHouse as a query parameter and never as
text in a statement. Sessions are read from the session summaries, so events
sent without a session id are not part of session figures.

## Symbol files

The source maps and mapping files of a release let the server turn a minified or
obfuscated stack back into code locations. `POST /sites/{site}/symbols` takes
the file as the request body (`application/octet-stream`) and three parameters,
percent-encoded in the query string:

| Parameter | Meaning |
|---|---|
| `release` | The app version as the SDK reports it, 1 to 64 characters of letters, digits and `_.+/-` |
| `kind` | `sourcemap` (a JSON object) or `proguard` (UTF-8 text) |
| `name` | The file's name, up to 256 characters of letters, digits and `_.+/-@`, written as a relative path without empty, `.` or `..` parts (`static/js/main.js.map`) |

A file is 1 byte to 64 MiB and a site keeps at most 1 GiB; uploading the same
`release`, `kind` and `name` again replaces the file and counts it once. The
answer holds the size and SHA-256 that were stored. The caller is checked
before the body is read, so only someone allowed to upload can make the server
buffer a large body, and at most four uploads are received at once per node
(`429 busy`). Wrong content is refused with `422 invalid_symbol_file`, a file
past the limits with `413`.

The files are kept in `artifacts_dir`. No part of a release or a name becomes
part of a path on disk: both are hashed.

**Names the server looks for.** A JavaScript frame is translated with the source
map named like its module plus `.map`: for `https://cdn.example.com/static/js/main.3f9a.js`
the file `static/js/main.3f9a.js.map`, then `main.3f9a.js.map`. Uploading from the
build directory with `--root` gives exactly these names. An Android release has
one file named `mapping.txt` (kind `proguard`). A file is looked up by its
release, so upload it for the `release` the app reports.

## Command line

The first member and the first tokens come from the command line, which reads
passwords from standard input (never from an argument):

```
echo 'a long password' | dionaea member create ana@example.com --name Ana --organization Acme --role owner
dionaea member list
dionaea member role 2 Acme viewer
dionaea member disable 2
dionaea token issue --site 1 --label dashboard
dionaea token list --site 1
dionaea token revoke --site 1 3
```

From CI, with an upload token an administrator issued
(`POST /sites/1/upload-tokens`), the token in the environment and never in an
argument:

```
DIONAEA_UPLOAD_TOKEN=ut_... dionaea symbols upload --url https://dionaea.example.com \
    --site 1 --release 1.4.2+57 --kind sourcemap --root build build/static/js/*.map
```

Each stored file is printed as its name, size and SHA-256. The exit status is 0
when every file was stored, 1 when the server refused one and 2 when a file
could not be read or the server could not be reached.
