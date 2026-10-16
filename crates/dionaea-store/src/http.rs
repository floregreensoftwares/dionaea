//! A small HTTP/1.1 client for ClickHouse's HTTP interface: one request per
//! connection, no TLS, no redirects. It exists so the store needs no HTTP
//! stack; ClickHouse is a single, known peer that answers plainly.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Largest response body read; a ClickHouse answer to an insert or a DDL
/// statement is empty or a short error.
const MAX_BODY: usize = 1024 * 1024;
const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) struct Endpoint<'a> {
    pub(crate) host: &'a str,
    pub(crate) port: u16,
    pub(crate) user: &'a str,
    pub(crate) password: &'a str,
    pub(crate) timeout: Duration,
}

/// Sends `POST path` with `body` and returns the answer.
pub(crate) fn post(endpoint: &Endpoint<'_>, path: &str, body: &[u8]) -> io::Result<Response> {
    let address = (endpoint.host, endpoint.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other("the host does not resolve"))?;
    let mut stream = TcpStream::connect_timeout(&address, endpoint.timeout)?;
    stream.set_read_timeout(Some(endpoint.timeout))?;
    stream.set_write_timeout(Some(endpoint.timeout))?;
    stream.set_nodelay(true)?;

    let credentials = base64(format!("{}:{}", endpoint.user, endpoint.password).as_bytes());
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Basic {credentials}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.host,
        endpoint.port,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    read_response(&mut stream)
}

/// Reads one response: status line, headers, then a body framed by
/// `Content-Length`, by chunks, or by the end of the connection.
pub(crate) fn read_response(reader: &mut impl Read) -> io::Result<Response> {
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
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("the status line is not valid"))?;
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse::<usize>().ok();
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }

    let mut rest = buffer[head_end + 4..].to_vec();
    let body = if chunked {
        read_chunked(reader, &mut rest)?
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
        rest.truncate(length);
        rest
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
        rest
    };
    Ok(Response { status, body })
}

/// Decodes a chunked body. `pending` holds bytes already read past the head.
fn read_chunked(reader: &mut impl Read, pending: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // Ensure a full size line is buffered.
        let line_end = loop {
            if let Some(position) = find(pending, b"\r\n") {
                break position;
            }
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "a chunk size is missing",
                ));
            }
            pending.extend_from_slice(&chunk[..n]);
        };
        let size_text = String::from_utf8_lossy(&pending[..line_end]).into_owned();
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| io::Error::other("a chunk size is not valid"))?;
        pending.drain(..line_end + 2);
        if size == 0 {
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(io::Error::other("the response body is too large"));
        }
        while pending.len() < size + 2 {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "a chunk ended early",
                ));
            }
            pending.extend_from_slice(&chunk[..n]);
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
    fn a_content_length_response_is_read() {
        let response = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(
            response,
            Response {
                status: 200,
                body: b"hello".to_vec()
            }
        );
    }

    #[test]
    fn an_empty_body_is_fine() {
        let response = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
        assert!(response.body.is_empty());
    }

    #[test]
    fn a_chunked_response_is_decoded() {
        let response = parse(
            b"HTTP/1.1 500 Internal Server Error\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(response.status, 500);
        assert_eq!(response.body, b"hello world");
    }

    #[test]
    fn a_body_with_no_framing_runs_to_the_end() {
        let response = parse(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil the end").unwrap();
        assert_eq!(response.body, b"until the end");
    }

    #[test]
    fn framing_is_read_in_any_split() {
        // One byte at a time, to cross every buffer boundary.
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
        let bytes: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let response = read_response(&mut OneByte(bytes)).unwrap();
        assert_eq!(response.body, b"abcde");
    }

    #[test]
    fn truncated_and_malformed_responses_are_errors() {
        assert!(parse(b"").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse(b"NOT HTTP\r\n\r\n").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab").is_err());
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
