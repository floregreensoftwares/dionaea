# Conformance vectors

Data files that every implementation of the schema guard and of the request
decoder must pass: the Rust core runs them in
[`crates/dionaea-core/tests/vectors.rs`](../crates/dionaea-core/tests/vectors.rs),
and each SDK runs the same files in its own test suite. A rule is only a rule
here if a vector pins it, so a change to the guard or to the wire format starts
by changing the vectors.

The files are plain JSON, so any language can read them. Numbers are compared
numerically (`1` and `1.0` are the same), objects without regard to key order.

## Guard vectors (`guard/*.json`)

```json
{
  "description": "What the file covers",
  "schema": "the schema file, as TOML text",
  "cases": [ { "name": "...", "event": "probe", "props": {}, "expect": {} } ]
}
```

The `schema` is parsed with the schema file rules (`docs/wire-format.md` and
`examples/schema.toml` describe the format). A case is either an event
(`event` and `props`) or identity traits (`traits`), and `expect` is one of:

- `{"accepted": {"fields": {...}, "undeclared": N, "invalid": [...]}}`: the
  fields that passed with their cleaned values, how many undeclared properties
  were stripped, and the declared optional fields stripped for an invalid
  value (by name, in alphabetical order).
- `{"dropped": {"reason": "...", "field": "..."}}`: the drop reason, and for a
  missing or invalid required field the declared field (the first by name when
  several are missing). `field` is absent for the other reasons.

| File | Covers |
|---|---|
| `kinds.json` | each field kind: accepted values, cleaning, clamping and rejection |
| `drops.json` | every drop reason, the order of the checks, the property limit, built-in events |
| `text.json` | the characters `text` replaces with a space, trimming and the cut |
| `traits.json` | identity traits |

## Wire vectors (`wire/*.json`)

```json
{
  "description": "...",
  "endpoint": "collect",
  "cases": [ { "name": "...", "body": "{...}", "expect": {} } ]
}
```

`endpoint` is `collect` or `identify`. `body` is the request body as text;
`body_parts` (`prefix`, `repeat`, `times`, `suffix`) builds a large body
without storing it. `expect` is one of:

- `{"request_error": "...", "field": "..."}`: the whole request is refused
  (`body_too_large`, `not_json`, `not_an_object`, `invalid_field` with the
  `field`, `too_many_events`).
- `{"batch": {...}}` for `collect`: `sent_at`, `sdk`, `release`, the decoded
  `events` and the `rejected` events as `{index, reason}`.
- `{"identify": {...}}` for `identify`.

## Reasons

The drop reasons are `malformed_event`, `invalid_event_id`,
`invalid_timestamp`, `invalid_anon_id`, `invalid_session_id`,
`invalid_screen`, `invalid_properties`, `invalid_event_name`, `unknown_event`,
`too_many_properties`, `missing_required_field` and
`invalid_required_field`. The Rust test fails unless at least one vector
produces each of them, and each request error.

## Not here yet

Scrubbing of error messages and route templating belong to app health and get
their own vectors with it.
