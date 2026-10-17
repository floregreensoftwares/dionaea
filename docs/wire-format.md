# Wire format

The request bodies of `POST /v1/collect` and `POST /v1/identify`. Both are
JSON (`Content-Type: application/json`). This document describes what the
decoder in `dionaea-core` (`decode_batch`, `decode_identify`) accepts; the
tests in that module are its executable form.

**Status: draft.** The format is declared stable only when the first release is
cut, and any change before that is made in this document and in the decoder
together. After that release a breaking change needs a new version prefix.

## Conventions

- **Timestamps** are integers: milliseconds since the Unix epoch, from 0 to
  2^53 - 1 (the largest integer every JSON implementation keeps exactly).
  Floats, negative numbers and strings are refused.
- **Ids** (`anon_id`, `session_id`, `user_id`) match `[A-Za-z0-9_.:-]`, 1 to 64
  characters. An email address is therefore not a valid `user_id`; send an
  opaque id.
- **Labels** (`sdk`, `release`) match `[A-Za-z0-9_.+/-]`, 1 to 64 characters,
  for example `dionaea-rn/0.1.0` and `1.4.2+57`.
- **A repeated key** inside the envelope or an event refuses the request (the
  reader treats it as invalid JSON); SDKs never write one.
- **Unknown fields are ignored**, at every level, so a newer SDK can add fields
  without breaking an older server. Optional fields may be absent or `null`.
- **Nothing is trusted.** The body size is checked before anything is parsed,
  and the body is read without building a general tree: arrays and objects
  inside `props` are read past and count as invalid values, an event stops
  being stored after 32 properties, and a batch after 100 events, so a body of
  tiny elements costs about as much as a normal one.

## Limits

| Limit | Value |
|---|---|
| Request body | 256 KiB (262144 bytes) |
| Events in a batch | 100 |
| Properties in one event | 32 (checked by the schema guard) |

## `POST /v1/collect`

```json
{
  "sent_at": 1760000001000,
  "sdk": "dionaea-rn/0.1.0",
  "release": "1.4.2+57",
  "events": [
    {
      "event_id": "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e",
      "name": "checkout_started",
      "ts": 1760000000000,
      "anon_id": "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e",
      "session_id": "0190f7e2-7c3e-7a52-9a0e-aaaaaaaaaaaa",
      "screen": "/orders/:id",
      "props": { "items": 3 }
    }
  ]
}
```

Envelope:

| Field | Required | Meaning |
|---|---|---|
| `sent_at` | yes | Client time when the batch was sent; the server uses it to correct clock skew |
| `sdk` | yes | The SDK name and version |
| `release` | no | The app version |
| `events` | yes | An array of up to 100 events (an empty array is valid) |

Event:

| Field | Required | Meaning |
|---|---|---|
| `event_id` | yes | A canonical UUID version 7 (36 characters, either case; variant `10xx`). The same id sent again is deduplicated, so an SDK can retry after a lost response |
| `name` | yes | A lowercase identifier, `[a-z][a-z0-9_]*`, up to 64 characters. Whether it is declared is decided by the schema guard |
| `ts` | yes | Client time of the event |
| `anon_id` | yes | The device's anonymous id |
| `session_id` | no | The SDK's session id. Without it the server derives sessions at query time |
| `screen` | no | The current screen or route pattern: printable ASCII without spaces, up to 128 characters |
| `props` | no | A JSON object checked against the schema; undeclared properties are stripped |

`anon_id` and `session_id` are per event, not per batch, because both change
inside a batch (on `reset` and when a session rotates).

### Refusing and dropping

A request with a bad envelope is refused as a whole, with one of these causes:
body over the limit, not JSON, not a JSON object, `sent_at` or `sdk` missing
or invalid, `release` invalid, `events` missing or not an array, more than 100
events.

A bad event is dropped alone and the rest of the batch goes on. Each drop is
counted under one reason, never with the payload. The checks run in this
order and the first failure names the reason:

| Order | Reason | When |
|---|---|---|
| 1 | `malformed_event` | the event is not a JSON object |
| 2 | `invalid_event_id` | missing, not a string, or not a canonical UUID v7 |
| 3 | `invalid_event_name` | missing, not a string, or not a valid name |
| 4 | `invalid_timestamp` | `ts` missing or not an integer in range |
| 5 | `invalid_anon_id` | missing or not id-shaped |
| 6 | `invalid_session_id` | present but not id-shaped |
| 7 | `invalid_screen` | present but not a valid screen |
| 8 | `invalid_properties` | present but not an object |
| 9 | `too_many_properties` | more than 32 entries (counted as written, duplicates included) |

The schema guard then adds `unknown_event`, `too_many_properties` (for traits),
`missing_required_field` and `invalid_required_field`.

### Sending the request

- **The write key** goes in the header `X-Dionaea-Key`, or in the query
  parameter `key` for clients that cannot set headers (`navigator.sendBeacon`).
  If both are present they must be equal, or the request is refused with 400.
  The key is a public identifier: it ships inside apps and pages, and what
  protects the endpoint is the schema, the limits and the origin list.
- **The body** is JSON, sent as `application/json` or as `text/plain` (the type
  `sendBeacon` uses); any other type is refused with 415.
- **Origins.** A key that lists web origins is accepted only with an `Origin`
  header that matches one of them. A key without origins (a native app) is
  accepted with or without one. The answer to an admitted request carries
  `Access-Control-Allow-Origin` with that origin and `Vary: Origin`. A
  preflight `OPTIONS` request is answered with 204 only for a valid key and an
  allowed origin.

| Status | Body | When |
|---|---|---|
| 202 | `{"accepted":n,"dropped":m}` | The events are durable. `n` were kept and `m` were dropped (invalid, undeclared, out of the time window, sent again, or sent by a known automated client) |
| 400 | `{"error":"<reason>"}` | The envelope is invalid (`not_json`, `not_an_object`, `invalid_field`, `too_many_events`) or the key was given twice with different values (`conflicting_keys`) |
| 401 | `{"error":"unknown_key"}` | No key, a key of the wrong shape, an unknown key or a revoked one |
| 403 | `{"error":"origin_not_allowed"}` | The key lists origins and the request's is not one |
| 405 | `{"error":"method_not_allowed"}` | Not `POST` or `OPTIONS` |
| 408 | `{"error":"body_timeout"}` | The body did not arrive in time |
| 413 | `{"error":"body_too_large"}` | The body is over 256 KiB, by `Content-Length` or by what arrived |
| 415 | `{"error":"unsupported_media_type"}` | The body type is not accepted |
| 429 | `{"error":"rate_limited"}` and `Retry-After` | A limit per key or per client address was reached |
| 503 | `{"error":"unavailable"}` and `Retry-After` | The server cannot make the events durable now (its queue is full or failing); keep the batch and retry |

An SDK keeps a batch it could not deliver (network error, 408, 429, 503) and
sends it again with the same `event_id`s; it drops a batch refused with 400,
401, 403, 413 or 415, which retrying cannot fix.

A kept event's time is corrected by the difference between the arrival time and
`sent_at`; an event whose corrected time is more than 30 days in the past or
one hour in the future is dropped.

## `POST /v1/identify`

```json
{
  "sent_at": 1760000001000,
  "sdk": "dionaea-web/0.1.0",
  "anon_id": "0190f7e2-7c3e-7a52-9a0e-5f3c2a1b4d6e",
  "user_id": "u_42",
  "traits": { "plan": "pro" }
}
```

All of `sent_at`, `sdk`, `anon_id` and `user_id` are required; an invalid one
refuses the request. `traits` is an optional object checked against the
schema's declared traits (undeclared ones are stripped). There is no endpoint
for `reset`: signing out is a new `anon_id` on the client.

The key, the origin rules, the rate limits, the body limits and the status
codes are those of `/v1/collect`. A request that is accepted is answered `202`
once it is durable:

```json
{ "accepted": 1, "stripped": 1 }
```

`accepted` is 1 when the link was recorded and 0 when the sender is an
automated client (counted, not stored). `stripped` is how many traits the
schema did not declare or whose value broke its rules. A request with more
than 32 traits is refused with `400`.

What is stored:

- **The link.** One row says `anon_id` belongs to `user_id` as of the arrival
  time. A device that identified as two users keeps both links; queries use
  the latest one. Sending the same identify again changes nothing but the
  time of the link.
- **The traits.** When at least one trait survives the schema, the user's
  traits become exactly that set (an earlier trait that is not sent again is
  gone). A request with no trait, or none the schema declares, leaves the
  earlier traits alone.
- Time is the server's arrival time, not `sent_at`, so a delayed retry of an
  old identify replaces a newer one.

## Health events

Errors and performance travel through `POST /v1/collect` as events with fixed
names and shapes that a site cannot declare or change. Everything is a scalar
property like any other event, so no wire change is involved. Durations are
whole milliseconds, up to ten minutes; sizes are bytes. Enum values are
lowercase.

| Event | Property | Required | Meaning |
|---|---|---|---|
| `app_error` | `kind` | yes | `handled`, `unhandled`, `boundary` or `crash` |
| | `type` | yes | The exception type, an id (`TypeError`, `java.io.IOException`) |
| | `message` | no | Text up to 512 characters; masked by the server (below) |
| | `frames` | no | The stack, up to 4096 characters (below) |
| `app_start` | `start` | yes | `cold` or `warm` |
| | `first_frame` | yes | Time to the first frame |
| | `interactive` | no | Time until the app is usable, when the app marks it |
| `screen_load` | `render` | yes | Time to first render of the event's `screen` |
| | `interactive` | no | Time to interactive |
| `http_request` | `method` | yes | `get`, `post`, `put`, `patch`, `delete`, `head`, `options` or `other` |
| | `host` | yes | The host and port, text up to 128 characters (an IPv6 address in brackets is fine) |
| | `route` | yes | The route; a template is best, the server templates what it gets |
| | `status` | yes | The status code, up to 599 |
| | `duration` | yes | Time to the response |
| | `request_size`, `response_size` | no | Bytes |
| | `connection` | no | `wifi`, `cellular`, `ethernet`, `other` or `none` |
| `web_vital` | `name` | yes | `lcp`, `inp`, `cls`, `ttfb` or `fcp` |
| | `value` | yes | Milliseconds; a layout shift score (`cls`) in thousandths |
| | `rating` | no | `good`, `needs_improvement` or `poor` |
| | `route` | no | The route pattern |

`frame_stats`, `anr` and `app_crash` are reserved and have no shape yet; events
with those names are dropped as unknown.

There is no property for a URL query string, a request or response body, a
header, or a local variable, so there is nowhere for one to travel: such
properties are stripped as undeclared.

### Frames

A stack is code locations only, written in one text: frames joined by `;`, each
`module|function|line|column|flag`. `module` is a file or class, `line` and
`column` may be empty, and `flag` is `1` for the app's own code and `0` for a
library or the platform. The first frame is the top of the stack. A `%`, `|` or
`;` inside a name is written `%25`, `%7C` or `%3B`. The server keeps at most 64
well-formed frames, cuts a module to 256 characters and a function to 128, and
removes the query string and fragment of a module; a frame that does not parse
(one cut in half by the length limit, for example) is dropped.

### What the server does to the text

- **Messages** are masked before storage: e-mail addresses, the user name,
  password, query string and fragment of URLs, UUIDs, JWT-like and other long
  tokens, and long digit runs are replaced by `[email]`, `[userinfo]`,
  `?[query]`, `#[fragment]`, `[uuid]`, `[token]` and `[number]`.
- **A message the guard cut** at 512 characters loses its last word, which may
  be the front of an address or a number the masks no longer recognise.
- **The `screen` of every event** loses a query string or fragment, and a
  screen that is a path (starts with `/`) has identifier-like segments turned
  into `:id`; a screen name such as `Home` is kept.
- **Routes** lose the scheme, host, query string, fragment and matrix
  parameters, and a segment that looks like an identifier becomes `:id`. A
  segment already written `:name`, `{name}` or `[name]` is kept.

Both rules are pinned by the vectors in `vectors/health/`, which an SDK that
applies them on the device passes too. A site cannot turn the built-in masks
off.

## What is not here yet

- Request headers, authentication with the write key, and the HTTP status
  codes come with the ingest handlers.
