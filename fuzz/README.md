# Fuzzing

libFuzzer targets for the code that reads untrusted input. They live outside
the main workspace because they need a nightly compiler and sanitizer flags.

| Target | Input | What it checks |
|---|---|---|
| `parse_schema` | any text | no panic; an accepted file equals itself under `diff`; diagnostics are bounded and positioned inside the file |
| `decode_batch` | any bytes | no panic; accepted batches stay inside the event and property limits |
| `decode_identify` | any bytes | no panic; accepted traits stay inside the property limit |
| `guard_event` | a JSON object | no panic; every property is accounted for exactly once, for events and for traits |

Each target starts from the seed in `corpus/<target>/`. The corpus libFuzzer
grows is not committed.

## Running

```sh
cargo install cargo-fuzz --locked      # once
cd fuzz
cargo +nightly fuzz run parse_schema -- -max_total_time=90 -rss_limit_mb=2048
```

On Windows or a mounted drive the build is slow; set `CARGO_TARGET_DIR` to a
folder on the Linux filesystem. A crash is saved under `fuzz/artifacts/<target>/`;
`cargo +nightly fuzz run <target> <file>` reproduces it, and the fix should add a
regression test (or a conformance vector) beside it.

## Recorded runs

Command: `cargo fuzz run <target> -- -max_total_time=90 -rss_limit_mb=2048`,
nightly toolchain, Ubuntu 24.04 under WSL2, one worker, from the seed corpus.

| Date | Target | Runs | Result |
|---|---|---|---|
| 2026-10-15 | `parse_schema` | 34376 in 91 s | no crash |
| 2026-10-15 | `decode_batch` | 103218 in 91 s | no crash |
| 2026-10-15 | `decode_identify` | 151418 in 92 s | no crash |
| 2026-10-15 | `guard_event` | 239474 in 91 s | no crash |

These are short runs on a slow filesystem: they show the targets work and the
easy inputs are clean, not that the code is free of crashes. Longer runs
belong on a dedicated machine before a release.
