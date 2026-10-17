//! Stack frames as they travel in the `frames` text field of an `app_error`.
//!
//! A frame is a code location and nothing else: the module (a file or class),
//! the function, the line and column, and whether it is the app's own code.
//! The field is a single text because event properties are scalar. Each frame
//! is `module|function|line|column|flag`, frames are joined by `;`, and `%`,
//! `|` and `;` inside a name are written `%25`, `%7C` and `%3B`. The flag is
//! `1` for the app's code and `0` for a library or the platform. The first
//! frame is the top of the stack (where the error was thrown).
//!
//! [`parse`] is tolerant and bounded: a malformed frame, such as one cut in
//! half by the field's length limit, is skipped, names are cut to their
//! maximum, and at most [`MAX_FRAMES`] frames are kept. [`encode`] writes what
//! [`parse`] reads.

use crate::scrub::strip_query;

/// Most frames kept; deeper frames add nothing to grouping.
pub const MAX_FRAMES: usize = 64;
/// Longest module and function, in characters.
pub const MAX_MODULE_CHARS: usize = 256;
pub const MAX_FUNCTION_CHARS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub module: String,
    pub function: String,
    pub line: u32,
    pub column: u32,
    pub in_app: bool,
}

/// The frames in `text`.
pub fn parse(text: &str) -> Vec<Frame> {
    text.split(';')
        .filter_map(parse_frame)
        .take(MAX_FRAMES)
        .collect()
}

fn parse_frame(text: &str) -> Option<Frame> {
    let mut parts = text.split('|');
    let (module, function, line, column, flag) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if parts.next().is_some() {
        return None;
    }
    let number = |text: &str| -> Option<u32> {
        if text.is_empty() {
            Some(0)
        } else if text.len() <= 9 && text.bytes().all(|b| b.is_ascii_digit()) {
            text.parse().ok()
        } else {
            None
        }
    };
    let in_app = match flag {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    Some(Frame {
        module: cut(&strip_query(&decode(module)?), MAX_MODULE_CHARS),
        function: cut(&decode(function)?, MAX_FUNCTION_CHARS),
        line: number(line)?,
        column: number(column)?,
        in_app,
    })
}

/// The frames written as the field's text. Frames past [`MAX_FRAMES`] are not
/// written.
pub fn encode(frames: &[Frame]) -> String {
    let mut out = String::new();
    for (index, frame) in frames.iter().take(MAX_FRAMES).enumerate() {
        if index > 0 {
            out.push(';');
        }
        out.push_str(&escape(&frame.module));
        out.push('|');
        out.push_str(&escape(&frame.function));
        out.push('|');
        if frame.line > 0 {
            out.push_str(&frame.line.to_string());
        }
        out.push('|');
        if frame.column > 0 {
            out.push_str(&frame.column.to_string());
        }
        out.push('|');
        out.push(if frame.in_app { '1' } else { '0' });
    }
    out
}

fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '%' => out.push_str("%25"),
            '|' => out.push_str("%7C"),
            ';' => out.push_str("%3B"),
            c => out.push(c),
        }
    }
    out
}

/// Undoes [`escape`]; `None` for a `%` that starts anything else.
fn decode(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let code = rest.get(at + 1..at + 3)?;
        out.push(match code {
            "25" => '%',
            "7C" => '|',
            "3B" => ';',
            _ => return None,
        });
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    Some(out)
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// What the server stores for a `frames` field: parsed, query strings removed
/// from the modules, bounded, and written again.
pub fn normalize(text: &str) -> String {
    encode(&parse(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(module: &str, function: &str, line: u32, column: u32, in_app: bool) -> Frame {
        Frame {
            module: module.to_owned(),
            function: function.to_owned(),
            line,
            column,
            in_app,
        }
    }

    #[test]
    fn frames_round_trip_including_the_characters_that_delimit() {
        let frames = vec![
            frame("app/cart.js", "checkout", 42, 7, true),
            frame("a|b;c%d", "f|g;h%i", 0, 0, false),
        ];
        let text = encode(&frames);
        assert_eq!(
            text,
            "app/cart.js|checkout|42|7|1;a%7Cb%3Bc%25d|f%7Cg%3Bh%25i|||0"
        );
        assert_eq!(parse(&text), frames);
    }

    #[test]
    fn malformed_and_cut_frames_are_skipped() {
        let text = "a|f|1|2|1;broken;b|g|x|2|1;c|h|1|2|9;d|i|3|4|1;e|j|5|6";
        assert_eq!(
            parse(text),
            vec![frame("a", "f", 1, 2, true), frame("d", "i", 3, 4, true)]
        );
        assert!(parse("").is_empty());
        assert!(parse("%ZZ|f|1|2|1").is_empty());
        assert!(parse("a|f|1|2|1|extra").is_empty());
    }

    #[test]
    fn query_strings_leave_the_module_and_names_are_bounded() {
        let long = "m".repeat(1000);
        let text = format!("https://cdn.example.com/app.js?v=3#x|f|1|2|1;{long}|{long}|1|1|0");
        let frames = parse(&text);
        assert_eq!(frames[0].module, "https://cdn.example.com/app.js");
        assert_eq!(frames[1].module.chars().count(), MAX_MODULE_CHARS);
        assert_eq!(frames[1].function.chars().count(), MAX_FUNCTION_CHARS);
    }

    #[test]
    fn no_more_than_the_maximum_number_of_frames_is_kept() {
        let text = (0..200)
            .map(|i| format!("m{i}|f|1|1|1"))
            .collect::<Vec<_>>()
            .join(";");
        assert_eq!(parse(&text).len(), MAX_FRAMES);
        assert_eq!(normalize(&text).split(';').count(), MAX_FRAMES);
    }

    #[test]
    fn numbers_are_bounded_to_nine_digits() {
        assert!(parse("a|f|1234567890|1|1").is_empty());
        assert_eq!(parse("a|f|123456789|1|1")[0].line, 123_456_789);
    }
}
