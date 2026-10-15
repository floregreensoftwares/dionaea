//! Runs the real binary on files, the way a contributor or a CI step does.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("schema-cli")
        .join(test);
    fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

fn write(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write the test file");
    path
}

fn run(args: &[&std::path::Path]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dionaea"));
    command.arg("schema");
    command.args(args);
    command.output().expect("run the binary")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

const VALID: &str = "[events.checkout.fields.items]\nkind = \"count\"\nmax = 10\nrequired = true\n";

#[test]
fn check_accepts_a_valid_file() {
    let dir = dir("check_ok");
    let file = write(&dir, "schema.toml", VALID);
    let output = run(&["check".as_ref(), file.as_path()]);
    assert_eq!(output.status.code(), Some(0));
    assert!(text(&output.stdout).contains("ok (1 events, 0 traits)"));
}

#[test]
fn check_prints_the_position_of_each_problem_and_exits_1() {
    let dir = dir("check_bad");
    let file = write(
        &dir,
        "schema.toml",
        "[events.a.fields.b]\nkind = \"float\"\n",
    );
    let output = run(&["check".as_ref(), file.as_path()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("schema.toml:2:8: unknown kind `float`"),
        "{stderr}"
    );
}

#[test]
fn check_exits_2_for_a_missing_file() {
    let dir = dir("check_missing");
    let output = run(&["check".as_ref(), dir.join("nope.toml").as_path()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("cannot read"));
}

#[test]
fn check_refuses_an_oversized_file_without_reading_it_all() {
    let dir = dir("check_big");
    let file = write(&dir, "big.toml", &" ".repeat(2 * 1024 * 1024));
    let output = run(&["check".as_ref(), file.as_path()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("larger than"));
}

#[test]
fn diff_exits_0_for_allowed_changes_and_lists_them() {
    let dir = dir("diff_allowed");
    let old = write(&dir, "old.toml", VALID);
    let new = write(&dir, "new.toml", &format!("{VALID}[events.extra]\n"));
    let output = run(&["diff".as_ref(), old.as_path(), new.as_path()]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("allowed: events.extra: event added"),
        "{stdout}"
    );
    assert!(stdout.contains("1 changes, 0 breaking"));
}

#[test]
fn diff_exits_1_for_a_breaking_change() {
    let dir = dir("diff_breaking");
    let old = write(&dir, "old.toml", VALID);
    let new = write(
        &dir,
        "new.toml",
        "[events.checkout.fields.items]\nkind = \"count\"\nmax = 5\nrequired = true\n",
    );
    let output = run(&["diff".as_ref(), old.as_path(), new.as_path()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output.stdout)
            .contains("breaking: events.checkout.fields.items: max narrowed from 10 to 5")
    );
}

#[test]
fn diff_exits_2_when_an_input_is_invalid() {
    let dir = dir("diff_invalid");
    let old = write(&dir, "old.toml", VALID);
    let new = write(&dir, "new.toml", "[events.a.fields.b]\nkind = \"nope\"\n");
    let output = run(&["diff".as_ref(), old.as_path(), new.as_path()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("new.toml:2:8"));
}

#[test]
fn the_example_schema_in_the_repository_is_valid() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/schema.toml");
    let output = run(&["check".as_ref(), example.as_path()]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("ok (2 events, 1 traits)"));
}

#[test]
fn check_exits_2_for_a_file_that_is_not_utf_8() {
    let dir = dir("check_not_utf8");
    let path = dir.join("latin1.toml");
    fs::write(&path, b"# caf\xe9\n").expect("write the test file");
    let output = run(&["check".as_ref(), path.as_path()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("not valid UTF-8"));
}

#[test]
fn an_oversized_multi_byte_file_is_reported_as_too_large() {
    let dir = dir("check_big_multibyte");
    let file = write(&dir, "big.toml", &"é".repeat(1024 * 1024));
    let output = run(&["check".as_ref(), file.as_path()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("larger than"));
}

#[test]
fn diff_exits_0_and_prints_deprecations_when_nothing_is_breaking() {
    let dir = dir("diff_deprecated");
    let old = write(&dir, "old.toml", VALID);
    let new = write(&dir, "new.toml", "");
    let output = run(&["diff".as_ref(), old.as_path(), new.as_path()]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("deprecated: events.checkout: event removed"),
        "{stdout}"
    );
    assert!(stdout.contains("1 changes, 0 breaking"));
}

#[test]
fn json_schema_prints_a_json_schema_document() {
    let output = run(&["json-schema".as_ref()]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = text(&output.stdout);
    assert!(stdout.trim_start().starts_with('{'));
    assert!(stdout.contains("\"$schema\": \"https://json-schema.org/draft/2020-12/schema\""));
    assert!(stdout.contains("\"duration\""));
}
