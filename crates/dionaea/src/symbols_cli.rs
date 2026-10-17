//! `dionaea symbols upload`: sends the source maps or mapping files of a
//! release to a server, from CI. The token comes from the environment
//! (`DIONAEA_UPLOAD_TOKEN`), never from an argument, so it stays out of the
//! process list and the build log.
//!
//! Exit status: 0 every file stored, 1 the server refused one, 2 the command
//! could not run (a file that cannot be read, no connection).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dionaea_meta::SymbolKind;

use crate::meta_cli::{OK, REFUSED, UNUSABLE};

/// The largest file the server takes.
const MAX_FILE_BYTES: u64 = 64 << 20;
const TIMEOUT: Duration = Duration::from_secs(300);

/// `%XX` for every byte that is not an unreserved character.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Whether sending a token to `url` is safe: HTTPS, or a name that only this
/// machine reaches. Anything else would put the token on the network in the clear.
pub(crate) fn safe_to_send(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// The name a file is stored under: its path below `root` with `/`
/// separators, or just its file name when there is no root.
pub(crate) fn name_of(file: &Path, root: Option<&Path>) -> Result<String, String> {
    let relative = match root {
        Some(root) => file
            .strip_prefix(root)
            .map_err(|_| format!("{} is not inside {}", file.display(), root.display()))?,
        None => Path::new(
            file.file_name()
                .ok_or_else(|| format!("{} has no file name", file.display()))?,
        ),
    };
    let parts: Option<Vec<&str>> = relative
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect();
    parts
        .map(|p| p.join("/"))
        .ok_or_else(|| format!("{} is not valid UTF-8", file.display()))
}

fn endpoint(url: &str, site: u32, release: &str, kind: SymbolKind, name: &str) -> String {
    format!(
        "{}/api/v1/sites/{site}/symbols?release={}&kind={}&name={}",
        url.trim_end_matches('/'),
        encode(release),
        kind.as_str(),
        encode(name)
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn upload(
    url: &str,
    token: &str,
    site: u32,
    release: &str,
    kind: SymbolKind,
    root: Option<&Path>,
    files: &[PathBuf],
    allow_http: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    if !allow_http && !safe_to_send(url) {
        let _ = writeln!(
            err,
            "error: refusing to send the token over plain HTTP to {url}; use https://, or pass --allow-http"
        );
        return UNUSABLE;
    }
    if files.is_empty() {
        let _ = writeln!(err, "error: name at least one file");
        return UNUSABLE;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let mut status = OK;
    for file in files {
        let name = match name_of(file, root) {
            Ok(name) => name,
            Err(message) => {
                let _ = writeln!(err, "error: {message}");
                return UNUSABLE;
            }
        };
        let size = match std::fs::metadata(file) {
            Ok(meta) => meta.len(),
            Err(e) => {
                let _ = writeln!(err, "error: cannot read {}: {e}", file.display());
                return UNUSABLE;
            }
        };
        if size > MAX_FILE_BYTES {
            let _ = writeln!(err, "error: {} is larger than 64 MiB", file.display());
            return UNUSABLE;
        }
        let bytes = match std::fs::read(file) {
            Ok(bytes) => bytes,
            Err(e) => {
                let _ = writeln!(err, "error: cannot read {}: {e}", file.display());
                return UNUSABLE;
            }
        };
        let response = agent
            .post(&endpoint(url, site, release, kind, &name))
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/octet-stream")
            .send(&bytes[..]);
        let mut response = match response {
            Ok(response) => response,
            Err(e) => {
                let _ = writeln!(err, "error: cannot reach the server: {e}");
                return UNUSABLE;
            }
        };
        let code = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let json: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        if code == 201 {
            let _ = writeln!(
                out,
                "{name}\t{}\t{}",
                json["size"].as_u64().unwrap_or(size),
                json["sha256"].as_str().unwrap_or("-")
            );
        } else {
            let reason = json["error"]["message"].as_str().unwrap_or("refused");
            let _ = writeln!(err, "error: {name}: {code} {reason}");
            status = REFUSED;
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_percent_encoded() {
        assert_eq!(encode("1.4.2+57"), "1.4.2%2B57");
        assert_eq!(encode("static/js/a b.map"), "static%2Fjs%2Fa%20b.map");
        assert_eq!(encode("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn a_name_is_the_path_below_the_root_or_the_file_name() {
        let root = Path::new("build");
        assert_eq!(
            name_of(Path::new("build/static/js/main.js.map"), Some(root)).unwrap(),
            "static/js/main.js.map"
        );
        assert!(name_of(Path::new("other/main.map"), Some(root)).is_err());
        assert_eq!(
            name_of(Path::new("build/static/main.map"), None).unwrap(),
            "main.map"
        );
    }

    #[test]
    fn a_token_is_only_sent_over_https_or_to_this_machine() {
        for good in [
            "https://d.example.com",
            "http://localhost:8090",
            "http://127.0.0.1:8090/x",
            "http://[::1]:8090",
        ] {
            assert!(safe_to_send(good), "{good}");
        }
        for bad in [
            "http://d.example.com",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "ftp://x",
            "d.example.com",
            "http://user@d.example.com",
        ] {
            assert!(!safe_to_send(bad), "{bad}");
        }
    }

    #[test]
    fn the_address_is_built_from_the_pieces() {
        assert_eq!(
            endpoint(
                "https://d.example.com/",
                3,
                "1.4.2+57",
                SymbolKind::SourceMap,
                "a/b.map"
            ),
            "https://d.example.com/api/v1/sites/3/symbols?release=1.4.2%2B57&kind=sourcemap&name=a%2Fb.map"
        );
    }
}
