//! Runs the real binary against a metadata file, the way an operator does.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("meta-cli")
        .join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn run(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dionaea"))
        .args(args)
        .env("DIONAEA_METADATA_PATH", db)
        .output()
        .expect("run the binary")
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const V1: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 10\nrequired = true\n";
const V2: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 10\nrequired = true\n\n[events.checkout.fields.coupon]\nkind = \"text\"\nmax = 8\n";
const BREAKING: &str =
    "[events.checkout.fields.items]\nkind = \"count\"\nmax = 5\nrequired = true\n";

#[test]
fn an_operator_can_bootstrap_a_site_a_key_and_a_schema() {
    let dir = dir("bootstrap");
    let db = dir.join("meta.db");
    let v1 = dir.join("v1.toml");
    fs::write(&v1, V1).expect("write");

    let created = run(&db, &["site", "create", "shop"]);
    assert_eq!(created.status.code(), Some(0), "{}", err(&created));
    assert_eq!(out(&created), "1\tshop\n");

    let key = run(
        &db,
        &[
            "key",
            "issue",
            "--site",
            "1",
            "--label",
            "web",
            "--origin",
            "https://shop.example",
        ],
    );
    assert_eq!(key.status.code(), Some(0), "{}", err(&key));
    let key = out(&key).trim().to_owned();
    assert!(dionaea_meta::is_write_key_shape(&key), "{key}");

    let pushed = run(
        &db,
        &["schema", "push", "--site", "1", v1.to_str().expect("utf-8")],
    );
    assert_eq!(pushed.status.code(), Some(0), "{}", err(&pushed));
    assert_eq!(out(&pushed), "published version 1: 0 changes, 0 breaking\n");

    let listed = run(&db, &["key", "list", "--site", "1"]);
    assert_eq!(
        out(&listed),
        format!("{key}\tactive\tweb\thttps://shop.example\n")
    );
    let sites = run(&db, &["site", "list"]);
    assert_eq!(out(&sites), "1\tshop\tdefault\n");

    // It is all in the file: a new process sees it.
    let versions = run(&db, &["schema", "versions", "--site", "1"]);
    assert_eq!(out(&versions), "1\tactive\n");
    let revoked = run(&db, &["key", "revoke", &key]);
    assert_eq!(revoked.status.code(), Some(0));
    let listed = run(&db, &["key", "list", "--site", "1"]);
    assert!(out(&listed).contains("\trevoked\t"), "{}", out(&listed));
}

#[test]
fn a_refused_push_exits_1_names_the_problem_and_stores_nothing() {
    let dir = dir("refused");
    let db = dir.join("meta.db");
    let (v1, v2, breaking) = (dir.join("v1.toml"), dir.join("v2.toml"), dir.join("b.toml"));
    fs::write(&v1, V1).expect("write");
    fs::write(&v2, V2).expect("write");
    fs::write(&breaking, BREAKING).expect("write");
    run(&db, &["site", "create", "shop"]);
    let push = |file: &Path, extra: &[&str]| {
        let mut args = vec!["schema", "push", "--site", "1"];
        args.extend_from_slice(extra);
        args.push(file.to_str().expect("utf-8"));
        run(&db, &args)
    };
    assert_eq!(push(&v1, &[]).status.code(), Some(0));

    let refused = push(&breaking, &[]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        err(&refused).contains("breaking: events.checkout.fields.items"),
        "{}",
        err(&refused)
    );

    let dry = push(&v2, &["--dry-run"]);
    assert_eq!(dry.status.code(), Some(0));
    assert!(
        out(&dry).contains("dry run: 1 changes, 0 breaking"),
        "{}",
        out(&dry)
    );
    assert_eq!(
        out(&run(&db, &["schema", "versions", "--site", "1"])),
        "1\tactive\n"
    );

    let again = push(&v1, &[]);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        err(&again).contains("same as the latest"),
        "{}",
        err(&again)
    );

    let missing = run(&db, &["schema", "push", "--site", "1", "no-such-file.toml"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(err(&missing).contains("cannot read"), "{}", err(&missing));
}

#[test]
fn an_unusable_store_exits_2() {
    let dir = dir("unusable");
    // A directory is not a database file.
    let output = run(&dir, &["site", "list"]);
    assert_eq!(output.status.code(), Some(2), "{}", err(&output));
    assert!(err(&output).contains("cannot open"), "{}", err(&output));
}
