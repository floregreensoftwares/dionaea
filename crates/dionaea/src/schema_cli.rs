//! `dionaea schema check` and `dionaea schema diff`: the schema file rules of
//! `dionaea-core` on local files. Exit status: 0 success, 1 the file is
//! invalid (check) or the change is breaking (diff), 2 a file could not be read
//! or parsed for a comparison.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use dionaea_core::{MAX_SOURCE_LEN, Schema, Severity, diff, is_breaking, parse_schema};

pub(crate) const OK: u8 = 0;
pub(crate) const FAILED: u8 = 1;
pub(crate) const UNUSABLE: u8 = 2;

/// Reads a schema file, never more than one byte past the size limit.
pub(crate) fn read_schema_file(path: &Path) -> Result<String, String> {
    let fail = |e: &dyn std::fmt::Display| format!("cannot read {}: {e}", path.display());
    let file = File::open(path).map_err(|e| fail(&e))?;
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(&e))?;
    if bytes.len() > MAX_SOURCE_LEN {
        // Over the limit: the parser reports the size, so decoding may be lossy.
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    String::from_utf8(bytes).map_err(|_| fail(&"the file is not valid UTF-8"))
}

fn parse(label: &str, source: &str, err: &mut impl Write) -> Option<Schema> {
    match parse_schema(source) {
        Ok(schema) => Some(schema),
        Err(diagnostics) => {
            for d in diagnostics {
                let _ = writeln!(err, "{label}:{}:{}: {}", d.line, d.column, d.message);
            }
            None
        }
    }
}

pub(crate) fn check(label: &str, source: &str, out: &mut impl Write, err: &mut impl Write) -> u8 {
    match parse(label, source, err) {
        Some(schema) => {
            let _ = writeln!(
                out,
                "{label}: ok ({} events, {} traits)",
                schema.events().len(),
                schema.traits().len()
            );
            OK
        }
        None => FAILED,
    }
}

pub(crate) fn compare(
    (old_label, old_source): (&str, &str),
    (new_label, new_source): (&str, &str),
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    let old = parse(old_label, old_source, err);
    let new = parse(new_label, new_source, err);
    let (Some(old), Some(new)) = (old, new) else {
        return UNUSABLE;
    };
    let changes = diff(&old, &new);
    for change in &changes {
        let _ = writeln!(out, "{change}");
    }
    let breaking = changes
        .iter()
        .filter(|c| c.severity() == Severity::Breaking)
        .count();
    let _ = writeln!(out, "{} changes, {breaking} breaking", changes.len());
    if is_breaking(&changes) { FAILED } else { OK }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELD: &str = "[events.e.fields.f]\nkind = \"id\"\n";

    fn run_check(source: &str) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = check("s.toml", source, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn run_compare(old: &str, new: &str) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = compare(("old.toml", old), ("new.toml", new), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn check_reports_a_valid_file() {
        let (code, out, err) = run_check(FIELD);
        assert_eq!(code, OK);
        assert_eq!(out, "s.toml: ok (1 events, 0 traits)\n");
        assert!(err.is_empty());
    }

    #[test]
    fn check_prints_every_diagnostic_with_file_line_and_column() {
        let source =
            "[events.a.fields.b]\nkind = \"float\"\n[events.a.fields.c]\nkind = \"text\"\n";
        let (code, out, err) = run_check(source);
        assert_eq!(code, FAILED);
        assert!(out.is_empty());
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("s.toml:2:8: unknown kind `float`"),
            "{}",
            lines[0]
        );
        assert!(lines[1].starts_with("s.toml:4:"), "{}", lines[1]);
    }

    #[test]
    fn compare_without_changes_succeeds() {
        let (code, out, _) = run_compare(FIELD, FIELD);
        assert_eq!(code, OK);
        assert_eq!(out, "0 changes, 0 breaking\n");
    }

    #[test]
    fn compare_with_only_allowed_changes_succeeds() {
        let new = format!("{FIELD}[events.other]\n");
        let (code, out, _) = run_compare(FIELD, &new);
        assert_eq!(code, OK);
        assert_eq!(
            out,
            "allowed: events.other: event added\n1 changes, 0 breaking\n"
        );
    }

    #[test]
    fn compare_with_a_breaking_change_fails() {
        let new = "[events.e.fields.f]\nkind = \"unit\"\n";
        let (code, out, _) = run_compare(FIELD, new);
        assert_eq!(code, FAILED);
        assert!(out.contains("breaking: events.e.fields.f: kind changed from id to unit"));
        assert!(out.ends_with("1 changes, 1 breaking\n"));
    }

    #[test]
    fn compare_names_the_file_of_an_invalid_input() {
        let (code, out, err) = run_compare(FIELD, "[events.e.fields.f]\nkind = \"nope\"\n");
        assert_eq!(code, UNUSABLE);
        assert!(out.is_empty());
        assert!(err.starts_with("new.toml:2:8:"), "{err}");
    }

    #[test]
    fn compare_reports_both_inputs_when_both_are_invalid() {
        let bad = "[events.e.fields.f]\nkind = \"nope\"\n";
        let (code, _, err) = run_compare(bad, bad);
        assert_eq!(code, UNUSABLE);
        assert!(err.contains("old.toml:2:8:") && err.contains("new.toml:2:8:"));
    }
}
