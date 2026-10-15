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

## What is not here yet

- Request headers, authentication with the write key, and the HTTP status
  codes come with the ingest handlers.
- The health event shapes (errors, performance) are added with the app health
  work; their names are already reserved.
