//! Event schema model and privacy guard shared by the Dionaea server and SDKs.
//!
//! This crate has no I/O, no async runtime and no storage dependency, so the
//! same validation runs on the server and inside clients.
