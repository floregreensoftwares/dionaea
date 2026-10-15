# dionaea

Dionaea is an open-source product analytics engine written in Rust. Like the flytrap it is named after, it stays open until something worth catching touches it: every event must match a schema you declare, and anything else is dropped at the door, so user content is never collected by construction. It runs as a single binary on top of ClickHouse, treats screens, taps and app lifecycle as first-class events, and can erase a user from every table on request.

## Status

Early development. There is no usable release yet, and no crate is published.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
