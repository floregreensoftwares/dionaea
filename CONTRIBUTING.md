# Contributing

## Setup

Install the Rust toolchain with [rustup](https://rustup.rs); the version is
pinned in `rust-toolchain.toml` and installed on first use. Then enable the
repository hooks and commit template once per clone:

```sh
sh scripts/install-hooks.sh
```

On Windows PowerShell:

```powershell
./scripts/install-hooks.ps1
```

Build output goes to `.cargo-target/` (set in `.cargo/config.toml`).

## Branches

- `trigger` is the integration branch. Branch from it and open pull requests
  against it: `feat/<slug>`, `fix/<slug>`, `chore/<slug>`, one concern per
  branch.
- `snap` holds releases and only moves forward from `trigger`.

## Commits

The `commit-msg` hook enforces the format:

```
type(scope): imperative summary

- Why: what problem this solves
- Change: what was changed
- Validation: how it was checked
- Risk: what could break
```

- Types: `feat fix refactor perf test docs build ci chore revert`.
- Scope is optional, lowercase, one only.
- Subject at most 72 characters, starting lowercase, no trailing period.
- The body is optional; when present it uses `- ` bullets or indented
  continuation lines.
- No attribution trailers. Describe what the change does, in English.

Never bypass the hooks with `--no-verify`; fix the cause instead.

## Checks

The hooks run these, and so does CI:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Every observable behavior needs a focused test, and every bug fix a
regression test where practical. New dependencies and any `unsafe` code
(forbidden by default) need a stated purpose and cost in the pull request.

## Benchmarks

Code on the ingest path has
[Criterion](https://github.com/bheisler/criterion.rs) benchmarks next to it, in
the crate's `benches/` folder (today the guard, the schema parser and the schema diff in
`dionaea-core`; new hot-path code adds its own). Run one crate's benchmarks, not the whole
workspace:

```sh
cargo bench -p dionaea-core
cargo bench -p dionaea-core --bench guard -- --save-baseline before
# change the code, then compare:
cargo bench -p dionaea-core --bench guard -- --baseline before
```

Benchmarks are compiled by clippy (`--all-targets`). Pull requests also run
`scripts/bench-compare.sh`, which benchmarks the base revision and the branch
one after the other on the same runner and prints the change in the job
summary; it never fails on a slowdown, since
shared runners are too noisy for a threshold. A change that touches a hot
path should state the before and after numbers, with the machine, in its pull
request. Numbers are only comparable on the same machine.
