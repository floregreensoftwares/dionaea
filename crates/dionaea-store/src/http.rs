//! A small HTTP/1.1 client for ClickHouse's HTTP interface: keep-alive
//! connections kept in a short idle list, optional TLS, no redirects. It exists
//! so the store needs no HTTP stack; ClickHouse is a single, known peer that
//! answers plainly.
//!
//! A connection comes back to the idle list only when its response was framed
//! (a length or chunks) and the server did not say it would close. If a reused
//! connection turns out to have been closed by the server in the meantime, the
//! request is sent again on another one. That is safe here because every call
//! is idempotent: statements are `IF NOT EXISTS`, queries only read, and
//! inserts carry a deduplication token.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Largest response body read; a ClickHouse answer to an insert or a DDL
/// statement is empty or a short error.
const MAX_BODY: usize = 1024 * 1024;
const MAX_HEAD: usize = 16 * 1024;
/// Idle connections kept per client.
const MAX_IDLE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    /// Whether the connection can carry another request.
    pub(crate) keep_alive: bool,
}

/// What a connection is: bytes both ways.
trait Transport: Read + Write + Send {}
impl<T: Read + Write + Send> Transport for T {}

pub(crate) struct Tls {
    config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
}

impl Tls {
    /// TLS to `server_name`, trusting the certificates in `ca_file` (PEM) or,
    /// without one, the Mozilla root set.
    pub(crate) fn new(server_name: &str, ca_file: Option<&Path>) -> Result<Self, String> {
        let mut roots = RootCertStore::empty();
        match ca_file {
            Some(path) => {
                let certificates = CertificateDer::pem_file_iter(path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
                let (added, _) = roots.add_parsable_certificates(certificates);
                if added == 0 {
                    return Err(format!("{} holds no usable certificate", path.display()));
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = ServerName::try_from(server_name.to_owned())
            .map_err(|_| format!("`{server_name}` is not a valid server name"))?;
        Ok(Self {
            config: Arc::new(config),
            server_name,
        })
    }
}

/// One ClickHouse server, reached over plain HTTP or TLS, with the idle
/// connections to it.
pub(crate) struct Client {
    host: String,
    port: u16,
    authorization: String,
    timeout: Duration,
    tls: Option<Tls>,
    idle: Mutex<Vec<Box<dyn Transport>>>,
    connects: AtomicU64,
}

impl Client {
    pub(crate) fn new(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        timeout: Duration,
        tls: Option<Tls>,
    ) -> Self {
        Self {
            host: host.to_owned(),
            port,
            authorization: format!("Basic {}", base64(format!("{user}:{password}").as_bytes())),
            timeout,
            tls,
            idle: Mutex::new(Vec::new()),
            connects: AtomicU64::new(0),
        }
    }

    /// How many connections this client has opened.
    pub(crate) fn connects(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
    }

    fn connect(&self) -> io::Result<Box<dyn Transport>> {
        // A name can resolve to several addresses (`localhost` to ::1 and
        // 127.0.0.1); the first that accepts the connection is used.
        let mut last_error = io::Error::other("the host does not resolve");
        let mut connected = None;
        for address in (self.host.as_str(), self.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, self.timeout) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_error = error,
            }
        }
        let stream = connected.ok_or(last_error)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.set_nodelay(true)?;
        self.connects.fetch_add(1, Ordering::Relaxed);
        match &self.tls {
            None => Ok(Box::new(stream)),
            Some(tls) => {
                let connection =
                    ClientConnection::new(Arc::clone(&tls.config), tls.server_name.clone())
                        .map_err(|e| io::Error::other(e.to_string()))?;
                Ok(Box::new(StreamOwned::new(connection, stream)))
            }
        }
    }

    fn take_idle(&self) -> Option<Box<dyn Transport>> {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
    }

    fn put_idle(&self, connection: Box<dyn Transport>) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < MAX_IDLE {
            idle.push(connection);
        }
    }

    /// Sends `POST path` with `body` and returns the answer.
    pub(crate) fn post(&self, path: &str, body: &[u8]) -> io::Result<Response> {
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: {}\r\nContent-Length: {}\r\n\r\n",
            self.host,
            self.port,
            self.authorization,
            body.len()
        );
        loop {
            let (mut connection, reused) = match self.take_idle() {
                Some(connection) => (connection, true),
                None => (self.connect()?, false),
            };
            match exchange(&mut *connection, &head, body) {
                Ok(response) => {
                    if response.keep_alive {
                        self.put_idle(connection);
                    }
                    return Ok(response);
                }
                // The server closed an idle connection: try the next one, or a
                // new one. At most `MAX_IDLE` stale connections are tried.
                Err(error) if reused && is_stale(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// Errors that mean the peer had already closed the connection.
fn is_stale(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

fn exchange(connection: &mut dyn Transport, head: &str, body: &[u8]) -> io::Result<Response> {
    connection.write_all(head.as_bytes())?;
    connection.write_all(body)?;
    connection.flush()?;
    read_response(connection)
}

/// Reads one response: status line, headers, then a body framed by
/// `Content-Length`, by chunks, or by the end of the connection. Reads nothing
/// past the end of the response, so the connection can be reused.
pub(crate) fn read_response(reader: &mut dyn Read) -> io::Result<Response> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEAD {
            return Err(io::Error::other("the response head is too large"));
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection closed before a response",
            ));
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("the status line is not valid"))?;
    let http_11 = status_line.starts_with("HTTP/1.1");
    let mut content_length = None;
    let mut chunked = false;
    let mut close = !http_11;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            } else if name == "connection" && value.to_ascii_lowercase().contains("close") {
                close = true;
            }
        }
    }

    let mut rest = buffer[head_end + 4..].to_vec();
    let (body, framed) = if chunked {
        (read_chunked(reader, &mut rest)?, rest.is_empty())
    } else if let Some(length) = content_length {
        if length > MAX_BODY {
            return Err(io::Error::other("the response body is too large"));
        }
        while rest.len() < length {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the body ended early",
                ));
            }
            rest.extend_from_slice(&chunk[..n]);
        }
        let extra = rest.len() > length;
        rest.truncate(length);
        (rest, !extra)
    } else {
        // No framing: the body runs to the end of the connection.
        loop {
            if rest.len() > MAX_BODY {
                return Err(io::Error::other("the response body is too large"));
            }
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&chunk[..n]);
        }
        (rest, false)
    };
    Ok(Response {
        status,
        body,
        keep_alive: framed && !close,
    })
}

/// Decodes a chunked body, including its trailer. `pending` holds bytes
/// already read past the head and ends up holding what follows the response.
fn read_chunked(reader: &mut dyn Read, pending: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut fill = |pending: &mut Vec<u8>, reader: &mut dyn Read, what: &str| -> io::Result<()> {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                what.to_owned(),
            ));
        }
        pending.extend_from_slice(&chunk[..n]);
        Ok(())
    };
    loop {
        let line_end = loop {
            if let Some(position) = find(pending, b"\r\n") {
                break position;
            }
            fill(pending, reader, "a chunk size is missing")?;
        };
        let size_text = String::from_utf8_lossy(&pending[..line_end]).into_owned();
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| io::Error::other("a chunk size is not valid"))?;
        pending.drain(..line_end + 2);
        if size == 0 {
            // The trailer: header lines up to an empty line.
            loop {
                let end = loop {
                    if let Some(position) = find(pending, b"\r\n") {
                        break position;
                    }
                    fill(pending, reader, "the trailer is missing")?;
                };
                let empty = end == 0;
                pending.drain(..end + 2);
                if empty {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > MAX_BODY {
            return Err(io::Error::other("the response body is too large"));
        }
        while pending.len() < size + 2 {
            fill(pending, reader, "a chunk ended early")?;
        }
        body.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Percent-encodes everything except unreserved characters, for query strings.
pub(crate) fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let n = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        out.push(char::from(ALPHABET[(n >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(n >> 12) as usize & 63]));
        out.push(if group.len() > 1 {
            char::from(ALPHABET[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            char::from(ALPHABET[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("user:pass", "dXNlcjpwYXNz"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input}");
        }
    }

    #[test]
    fn url_encoding_keeps_only_unreserved_characters() {
        assert_eq!(url_encode("abc-_.~09"), "abc-_.~09");
        assert_eq!(url_encode("INSERT INTO t"), "INSERT%20INTO%20t");
        assert_eq!(url_encode("a&b=c/d?e#f%"), "a%26b%3Dc%2Fd%3Fe%23f%25");
        assert_eq!(url_encode("é"), "%C3%A9");
    }

    fn parse(bytes: &[u8]) -> io::Result<Response> {
        read_response(&mut &bytes[..])
    }

    #[test]
    fn a_content_length_response_is_read_and_keeps_the_connection() {
        let response = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(
            response,
            Response {
                status: 200,
                body: b"hello".to_vec(),
                keep_alive: true
            }
        );
    }

    #[test]
    fn an_empty_body_is_fine() {
        let response = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
        assert!(response.body.is_empty() && response.keep_alive);
    }

    #[test]
    fn a_chunked_response_is_decoded() {
        let response = parse(
            b"HTTP/1.1 500 Internal Server Error\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.status, 500);
        assert_eq!(response.body, b"hello world");
        assert!(response.keep_alive);
    }

    #[test]
    fn a_body_with_no_framing_runs_to_the_end_and_closes() {
        let response = parse(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil the end").unwrap();
        assert_eq!(response.body, b"until the end");
        assert!(!response.keep_alive);
    }

    #[test]
    fn the_server_saying_close_or_speaking_http_1_0_ends_reuse() {
        assert!(
            !parse(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                .unwrap()
                .keep_alive
        );
        assert!(
            !parse(b"HTTP/1.0 200 OK\r\nContent-Length: 1\r\n\r\nx")
                .unwrap()
                .keep_alive
        );
        assert!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: keep-alive\r\n\r\nx")
                .unwrap()
                .keep_alive
        );
    }

    #[test]
    fn two_responses_in_a_row_are_read_without_eating_the_second() {
        let bytes: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        // A reader that hands out one byte at a time cannot read ahead by accident.
        struct OneByte<'a>(&'a [u8]);
        impl Read for OneByte<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let Some((first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                buf[0] = *first;
                self.0 = rest;
                Ok(1)
            }
        }
        let mut reader = OneByte(bytes);
        let first = read_response(&mut reader).unwrap();
        assert_eq!(first.body, b"abc");
        assert!(first.keep_alive);
        let second = read_response(&mut reader).unwrap();
        assert_eq!(second.body, b"ok");
    }

    #[test]
    fn a_response_that_arrives_with_extra_bytes_is_not_reused() {
        let response = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokEXTRA").unwrap();
        assert_eq!(response.body, b"ok");
        assert!(!response.keep_alive);
    }

    #[test]
    fn truncated_and_malformed_responses_are_errors() {
        assert!(parse(b"").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse(b"NOT HTTP\r\n\r\n").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n").is_err());
    }

    #[test]
    fn oversized_bodies_and_heads_are_refused() {
        let big = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(parse(big.as_bytes()).is_err());
        let mut head = b"HTTP/1.1 200 OK\r\n".to_vec();
        head.extend(std::iter::repeat_n(b'x', MAX_HEAD + 100));
        assert!(parse(&head).is_err());
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            MAX_BODY + 1
        );
        assert!(parse(chunked.as_bytes()).is_err());
    }
}
