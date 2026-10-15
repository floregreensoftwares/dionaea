# dionaea

Dionaea is an open-source product analytics engine written in Rust. Like the flytrap it is named after, it stays open until something worth catching touches it: every event must match a schema you declare, and anything else is dropped at the door, so user content is never collected by construction. It runs as a single binary on top of ClickHouse, treats screens, taps and app lifecycle as first-class events, and can erase a user from every table on request.

## Status

Early development. There is no usable release yet, and no crate is published.

## Schema files

A site declares its events and identity traits in a TOML file (see
[`examples/schema.toml`](examples/schema.toml)). Two commands work on local
files:

```sh
dionaea schema check examples/schema.toml          # problems print as file:line:column
dionaea schema diff old.toml new.toml              # exits 1 when a change is breaking
```

`check` exits 0 for a valid file, 1 for an invalid one and 2 when the file
cannot be read. `diff` lists each change as allowed, deprecated or breaking and
exits 1 only for breaking ones.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
