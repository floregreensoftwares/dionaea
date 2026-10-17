//! The name of the function around a position in original JavaScript source.
//!
//! A source map says where code came from, not what the function is called:
//! the minified name (`u`) is all a stack trace has. When the map carries the
//! original source (`sourcesContent`), the name is read from it by walking
//! backwards from the position to the `{` that opens the enclosing function and
//! reading the head in front of it. It is a scanner, not a parser, so it is
//! approximate: braces inside strings, comments, template literals or regular
//! expressions can mislead it. It is bounded, and the caller falls back to the
//! name the stack had when this finds nothing.
//!
//! It recognises `function NAME(...) {`, `async` and generator forms,
//! `NAME(...) {` (a method), `NAME = function (...) {`, and
//! `NAME = (...) => {` or `NAME: (...) => {`. A function written as an arrow
//! with an expression body has no braces, so it reports the function around it.

/// Most characters looked at before giving up.
const MAX_SCAN: usize = 64 * 1024;
/// Most characters of a function head read.
const MAX_HEAD: usize = 400;
/// Most function heads read for one position: each costs up to `MAX_HEAD`
/// characters, so this bounds the work of a lookup however many unclosed
/// braces the text has.
const MAX_HEADS: usize = 64;

const NOT_FUNCTIONS: &[&str] = &[
    "if", "for", "while", "switch", "catch", "with", "else", "do", "try", "finally",
];

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The byte offset of 1-based `line` and `column`, the column counted in
/// UTF-16 code units as source maps and JavaScript engines count it. A column
/// past the end of the line is the end of the line.
fn offset_of(text: &str, line: u32, column: u32) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let rest = &text[start..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let mut units = 0u32;
    let wanted = column.saturating_sub(1);
    let mut at = 0;
    for (i, c) in rest[..line_end].char_indices() {
        if units >= wanted {
            break;
        }
        units += u32::try_from(c.len_utf16()).unwrap_or(2);
        at = i + c.len_utf8();
    }
    Some(start + at)
}

/// The trailing identifier of `text`, and what is before it.
fn trailing_ident(text: &str) -> Option<(&str, &str)> {
    let end = text.len();
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map(|(i, _)| i)?;
    let ident = &text[start..end];
    // An identifier does not start with a digit.
    if ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((ident, &text[..start]))
}

/// The text before the `(` that matches the `)` ending `head`.
fn before_params(head: &str) -> Option<&str> {
    let mut depth = 0usize;
    for (i, c) in head.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&head[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// What the head in front of a `{` says: `Some(Some(name))` a named function,
/// `Some(None)` an unnamed one, `None` not a function.
fn read_head(head: &str) -> Option<Option<String>> {
    let head = head.trim_end();
    if let Some(before_arrow) = head.strip_suffix("=>") {
        // `NAME = (...) => {`, `NAME = x => {`, `NAME: (...) => {`.
        let params = before_arrow.trim_end();
        let before = if params.ends_with(')') {
            before_params(params)?
        } else {
            trailing_ident(params)?.1
        };
        return Some(assigned_name(before));
    }
    if head.ends_with(')') {
        let before = before_params(head)?.trim_end();
        // `function NAME(`, `function* NAME(`, `async function NAME(`.
        let (ident, rest) = match trailing_ident(before) {
            Some(found) => found,
            None => {
                // `function (...) {` or `function* (...) {`: unnamed, maybe assigned.
                let bare = before.strip_suffix('*').unwrap_or(before).trim_end();
                return bare.strip_suffix("function").map(assigned_name);
            }
        };
        if NOT_FUNCTIONS.contains(&ident) {
            return None;
        }
        if ident == "function" {
            return Some(assigned_name(rest));
        }
        let rest = rest.trim_end();
        let rest = rest.strip_suffix('*').unwrap_or(rest).trim_end();
        if rest.ends_with("function") {
            return Some(Some(ident.to_owned()));
        }
        // A method: `NAME(...) {`, possibly after `async`, `static`, `get`, `set`.
        return Some(Some(ident.to_owned()));
    }
    None
}

/// For text before an unnamed function or arrow: the name it is assigned to
/// (`NAME =`, `NAME:`, `this.NAME =`), or none.
fn assigned_name(before: &str) -> Option<String> {
    let before = before.trim_end();
    let before = before.strip_suffix("async").map_or(before, str::trim_end);
    let before = before
        .strip_suffix('=')
        .or_else(|| before.strip_suffix(':'))?
        .trim_end();
    trailing_ident(before).map(|(ident, _)| ident.to_owned())
}

/// The name of the function around the 1-based `line` and `column` of `source`.
pub fn enclosing_function(source: &str, line: u32, column: u32) -> Option<String> {
    let at = offset_of(source, line, column)?;
    let low = at.saturating_sub(MAX_SCAN);
    // Never start in the middle of a character.
    let low = (low..=at).find(|i| source.is_char_boundary(*i))?;
    let window = &source[low..at];
    let mut depth = 0usize;
    let mut heads = 0usize;
    for (i, c) in window.char_indices().rev() {
        match c {
            '}' => depth += 1,
            '{' => {
                if depth > 0 {
                    depth -= 1;
                    continue;
                }
                heads += 1;
                if heads > MAX_HEADS {
                    return None;
                }
                let head_start = window[..i]
                    .char_indices()
                    .rev()
                    .nth(MAX_HEAD)
                    .map_or(0, |(k, _)| k);
                if let Some(named) = read_head(&window[head_start..i]) {
                    return named;
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "export function parseQuantity(input) {\n  const value = Number(input);\n  if (Number.isNaN(value)) {\n    throw new TypeError(\"x\");\n  }\n  return value;\n}\n\nexport function total(items) {\n  return items.reduce((sum, item) => sum + item.price * parseQuantity(item.quantity), 0);\n}\n\nexport class Cart {\n  checkout() {\n    const amount = total(this.items);\n    return amount;\n  }\n}\n\nconst helper = (a, b) => {\n  return a + b;\n};\n\nconst other = async function (x) {\n  return x;\n};\n\nconst table = {\n  run: (n) => {\n    return n;\n  },\n  walk(m) {\n    return m;\n  },\n};\n\nfunction* gen() {\n  yield 1;\n}\n\ntry {\n  top();\n} catch (error) {\n  report();\n}\n";

    fn name(line: u32, column: u32) -> Option<String> {
        enclosing_function(SOURCE, line, column)
    }

    #[test]
    fn a_position_in_a_function_gets_its_name_even_inside_a_block() {
        assert_eq!(
            name(4, 11).as_deref(),
            Some("parseQuantity"),
            "inside an if block"
        );
        assert_eq!(name(6, 3).as_deref(), Some("parseQuantity"));
        assert_eq!(
            name(10, 20).as_deref(),
            Some("total"),
            "an arrow with an expression body reports the function around it"
        );
    }

    #[test]
    fn methods_arrows_function_expressions_and_generators_are_named() {
        assert_eq!(name(15, 5).as_deref(), Some("checkout"));
        assert_eq!(name(21, 3).as_deref(), Some("helper"));
        assert_eq!(name(25, 3).as_deref(), Some("other"));
        assert_eq!(name(30, 5).as_deref(), Some("run"));
        assert_eq!(name(33, 5).as_deref(), Some("walk"));
        assert_eq!(name(38, 3).as_deref(), Some("gen"));
    }

    #[test]
    fn top_level_code_has_no_function() {
        assert_eq!(name(42, 3), None);
        assert_eq!(name(44, 3), None, "a catch block is not a function");
        assert_eq!(name(1, 1), None);
    }

    #[test]
    fn positions_outside_the_text_find_nothing() {
        assert_eq!(enclosing_function(SOURCE, 999, 1), None);
        assert_eq!(enclosing_function(SOURCE, 0, 0), None);
        assert_eq!(enclosing_function("", 1, 1), None);
    }

    #[test]
    fn the_column_counts_utf16_units_and_stops_at_the_end_of_the_line() {
        // `}` is at UTF-16 column 13: the emoji takes two units, one character.
        let text = "function f() {\n  x = \"\u{1f600}\"; }\nfoo();\n";
        assert_eq!(enclosing_function(text, 2, 13).as_deref(), Some("f"));
        // A column past the end of the line does not reach the next line.
        assert_eq!(
            enclosing_function(text, 2, 9999).as_deref(),
            None,
            "after the closing brace"
        );
        assert_eq!(
            enclosing_function("function f() {\n}\nfoo", 1, 999).as_deref(),
            Some("f")
        );
        assert_eq!(offset_of("ab\ncd", 1, 99), Some(2));
        assert_eq!(offset_of("\u{1f600}x", 1, 3), Some(4), "two units, then x");
    }

    #[test]
    fn unicode_and_long_lines_are_handled() {
        let text = "function caf\u{e9}() {\n  return \"\u{1f600}\u{1f600}\";\n}\n";
        assert_eq!(
            enclosing_function(text, 2, 12).as_deref(),
            Some("caf\u{e9}")
        );
        let long = format!("function big() {{ {} }}", "x;".repeat(200_000));
        // Further back than the scan reaches: nothing, and no panic.
        assert_eq!(enclosing_function(&long, 1, 300_000), None);
        assert_eq!(enclosing_function(&long, 1, 100).as_deref(), Some("big"));
    }

    #[test]
    fn a_text_of_unclosed_braces_costs_a_bounded_amount() {
        let text = format!("function f() {{ {} x", "{ ".repeat(30_000));
        let started = std::time::Instant::now();
        assert_eq!(enclosing_function(&text, 1, 60_000), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn unbalanced_text_does_not_panic() {
        for text in [
            "}}}}",
            "{{{{",
            ")",
            "(((",
            "=> {",
            "function (",
            "a = => {",
            "\u{0}",
        ] {
            let _ = enclosing_function(text, 1, 3);
        }
    }
}
