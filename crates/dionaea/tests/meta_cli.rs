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
        .env("DIONAEA_ERASURE_KEY", "0123456789abcdef0123456789abcdef")
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

#[test]
fn an_operator_can_ask_for_an_erasure_list_it_and_retry_a_failed_one() {
    let dir = dir("erase");
    let db = dir.join("meta.db");
    assert_eq!(run(&db, &["site", "create", "shop"]).status.code(), Some(0));

    let asked = run(
        &db,
        &[
            "erase", "request", "--site", "1", "--user", "u_42", "--by", "ops",
        ],
    );
    assert_eq!(asked.status.code(), Some(0), "{}", err(&asked));
    let line = out(&asked);
    let fields: Vec<&str> = line.trim_end().split('\t').collect();
    assert_eq!(&fields[..4], &["1", "1", "requested", "ops"], "{line}");
    assert_eq!(&fields[5..8], &["-", "-", "0"]);
    assert_eq!(fields[8].len(), 64, "the hash of the user id");
    assert_eq!(fields[9], "0", "no trouble yet");
    assert!(!line.contains("u_42"), "the user id is never printed");

    // The same user again is the same request.
    let again = run(&db, &["erase", "request", "--site", "1", "--user", "u_42"]);
    assert_eq!(out(&again), line);

    let listed = run(&db, &["erase", "list"]);
    assert_eq!(out(&listed), line);
    let none = run(&db, &["erase", "list", "--state", "complete"]);
    assert_eq!((none.status.code(), out(&none).as_str()), (Some(0), ""));
    let shown = run(&db, &["erase", "show", "1"]);
    assert_eq!(out(&shown), line);

    let retry = run(&db, &["erase", "retry", "1"]);
    assert_eq!(retry.status.code(), Some(1), "only a failed one");
    assert!(
        err(&retry).contains("only a failed request"),
        "{}",
        err(&retry)
    );
}

#[test]
fn erase_refuses_what_it_cannot_do_with_a_clear_message() {
    let dir = dir("erase-refused");
    let db = dir.join("meta.db");
    assert_eq!(run(&db, &["site", "create", "shop"]).status.code(), Some(0));

    let no_site = run(&db, &["erase", "request", "--site", "9", "--user", "u_1"]);
    assert_eq!(no_site.status.code(), Some(1));
    assert!(
        err(&no_site).contains("site not found"),
        "{}",
        err(&no_site)
    );

    let email = run(&db, &["erase", "request", "--site", "1", "--user", "a@b.c"]);
    assert_eq!(email.status.code(), Some(1));
    assert!(err(&email).contains("user id"), "{}", err(&email));

    let state = run(&db, &["erase", "list", "--state", "later"]);
    assert_eq!(state.status.code(), Some(1));
    assert!(err(&state).contains("not a state"), "{}", err(&state));

    let missing = run(&db, &["erase", "show", "7"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        err(&missing).contains("erasure request not found"),
        "{}",
        err(&missing)
    );
    assert_eq!(out(&run(&db, &["erase", "list"])), "");
}

fn run_keyed(db: &Path, args: &[&str], key: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    command
        .args(args)
        .env("DIONAEA_METADATA_PATH", db)
        .env_remove("DIONAEA_ERASURE_KEY")
        .env_remove("DIONAEA_ERASURE_KEY_FILE");
    if let Some(key) = key {
        command.env("DIONAEA_ERASURE_KEY", key);
    }
    command.output().expect("run the binary")
}

const OTHER_KEY: &str = "fedcba9876543210fedcba9876543210";

#[test]
fn erasure_needs_a_key_and_says_how_to_set_one() {
    let dir = dir("erase-nokey");
    let db = dir.join("meta.db");
    assert_eq!(
        run_keyed(&db, &["site", "create", "shop"], None)
            .status
            .code(),
        Some(0),
        "the rest of the product needs no key"
    );
    let asked = run_keyed(
        &db,
        &["erase", "request", "--site", "1", "--user", "u_1"],
        None,
    );
    assert_eq!(asked.status.code(), Some(1));
    let message = err(&asked);
    assert!(
        message.contains("erasure_key") && message.contains("same on every server"),
        "{message}"
    );
    let short = run_keyed(
        &db,
        &["erase", "request", "--site", "1", "--user", "u_1"],
        Some("too short"),
    );
    assert_eq!(short.status.code(), Some(2), "{}", err(&short));
    assert!(err(&short).contains("32"), "{}", err(&short));
}

#[test]
fn another_key_cannot_read_or_add_erasures_made_with_the_first() {
    let dir = dir("erase-otherkey");
    let db = dir.join("meta.db");
    let key = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        run_keyed(&db, &["site", "create", "shop"], None)
            .status
            .code(),
        Some(0)
    );
    let first = run_keyed(
        &db,
        &["erase", "request", "--site", "1", "--user", "u_1"],
        Some(key),
    );
    assert_eq!(first.status.code(), Some(0), "{}", err(&first));

    for other in [Some(OTHER_KEY), None] {
        let again = run_keyed(
            &db,
            &["erase", "request", "--site", "1", "--user", "u_2"],
            other,
        );
        assert_eq!(again.status.code(), Some(1), "{other:?}: {}", err(&again));
    }
    // Listing needs no key: it shows hashes of requests already made.
    let listed = run_keyed(&db, &["erase", "list"], Some(OTHER_KEY));
    assert_eq!(listed.status.code(), Some(0));
    assert_eq!(out(&listed).lines().count(), 1);
}

#[test]
fn the_key_may_come_from_a_file_but_not_from_both() {
    let dir = dir("erase-keyfile");
    let db = dir.join("meta.db");
    assert_eq!(
        run_keyed(&db, &["site", "create", "shop"], None)
            .status
            .code(),
        Some(0)
    );
    let file = dir.join("erasure.key");
    fs::write(&file, "0123456789abcdef0123456789abcdef\n").expect("write the key");
    let run_file = |extra: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
        command
            .args(["erase", "request", "--site", "1", "--user", "u_1"])
            .env("DIONAEA_METADATA_PATH", &db)
            .env_remove("DIONAEA_ERASURE_KEY")
            .env("DIONAEA_ERASURE_KEY_FILE", &file);
        if let Some(extra) = extra {
            command.env("DIONAEA_ERASURE_KEY", extra);
        }
        command.output().expect("run the binary")
    };
    let ok = run_file(None);
    assert_eq!(ok.status.code(), Some(0), "{}", err(&ok));
    let both = run_file(Some("0123456789abcdef0123456789abcdef"));
    assert_eq!(both.status.code(), Some(2));
    assert!(err(&both).contains("not both"), "{}", err(&both));
    // The same key from the file and from the variable are the same key.
    let again = run_keyed(
        &db,
        &["erase", "request", "--site", "1", "--user", "u_2"],
        Some("0123456789abcdef0123456789abcdef"),
    );
    assert_eq!(again.status.code(), Some(0), "{}", err(&again));
}

#[test]
fn the_key_can_be_replaced_when_the_old_one_is_kept_as_a_previous_key() {
    let dir = dir("erase-rotate");
    let db = dir.join("meta.db");
    let old = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        run_keyed(&db, &["site", "create", "shop"], None)
            .status
            .code(),
        Some(0)
    );
    let first = run_keyed(
        &db,
        &["erase", "request", "--site", "1", "--user", "u_1"],
        Some(old),
    );
    assert_eq!(first.status.code(), Some(0), "{}", err(&first));

    let run_rotated = |previous: Option<&str>, user: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
        command
            .args(["erase", "request", "--site", "1", "--user", user])
            .env("DIONAEA_METADATA_PATH", &db)
            .env("DIONAEA_ERASURE_KEY", OTHER_KEY)
            .env_remove("DIONAEA_ERASURE_KEY_FILE")
            .env_remove("DIONAEA_ERASURE_PREVIOUS_KEYS");
        if let Some(previous) = previous {
            command.env("DIONAEA_ERASURE_PREVIOUS_KEYS", previous);
        }
        command.output().expect("run the binary")
    };
    let alone = run_rotated(None, "u_2");
    assert_eq!(alone.status.code(), Some(1), "{}", err(&alone));
    let message = err(&alone);
    assert!(
        message.contains("not configured") && message.contains("erasure_previous_keys"),
        "{message}"
    );
    assert!(!message.contains(old) && !message.contains(OTHER_KEY));

    let rotated = run_rotated(Some(old), "u_2");
    assert_eq!(rotated.status.code(), Some(0), "{}", err(&rotated));
    assert_eq!(
        out(&run_keyed(&db, &["erase", "list"], None))
            .lines()
            .count(),
        2
    );

    // A previous key with no current key is an error that says so.
    let mut orphan = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    orphan
        .args(["erase", "list"])
        .env("DIONAEA_METADATA_PATH", &db)
        .env_remove("DIONAEA_ERASURE_KEY")
        .env("DIONAEA_ERASURE_PREVIOUS_KEYS", old);
    let orphan = orphan.output().expect("run the binary");
    assert_eq!(orphan.status.code(), Some(2));
    assert!(
        err(&orphan).contains("set the current key"),
        "{}",
        err(&orphan)
    );
}
