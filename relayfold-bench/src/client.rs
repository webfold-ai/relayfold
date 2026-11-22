//! A plain HTTP/1.1 keep-alive client over an already-open `TcpStream`, std only.
//!
//! Every subject this harness measures is plain loopback HTTP, so TLS would be pure overhead.
//! The client reads exactly one response (status line, headers, then a `content-length` or
//! chunked body) and leaves the connection positioned at the start of the next response, which
//! is what lets one socket carry back-to-back requests.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// What the harness needs from a response: the status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Send one request on `conn` and read one response.
///
/// A response without `content-length` or `transfer-encoding: chunked` has no framing on a
/// keep-alive connection, so it is reported as an error rather than read until EOF.
///
/// # Errors
/// Any I/O error, a malformed status line or header block, or an unframed body.
pub fn request_plain_on(
    conn: &mut TcpStream,
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> io::Result<Response> {
    let mut req = format!("{method} {path} HTTP/1.1\r\nhost: {host}:{port}\r\n");
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    if !body.is_empty() {
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    conn.write_all(req.as_bytes())?;
    if !body.is_empty() {
        conn.write_all(body)?;
    }
    read_response(conn)
}

fn read_response(conn: &mut TcpStream) -> io::Result<Response> {
    // The reader is dropped before the next request, so it must not read past this response.
    // `BufReader` over `&mut TcpStream` can over-read into the next response only if the server
    // pipelines, which a request/response client never provokes.
    let mut r = BufReader::new(conn);
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before a status line",
        ));
    }
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("malformed status line"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(bad("connection closed inside the header block"));
        }
        let h = line.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                content_length = Some(v.parse().map_err(|_| bad("bad content-length"))?);
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }

    if (100..200).contains(&status) || status == 204 || status == 304 {
        return Ok(Response { status });
    }
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| bad("bad chunk size"))?;
            if size == 0 {
                // trailers, then the blank line
                loop {
                    line.clear();
                    if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
                        break;
                    }
                }
                break;
            }
            io::copy(&mut r.by_ref().take(size as u64 + 2), &mut io::sink())?;
        }
    } else if let Some(n) = content_length {
        io::copy(&mut r.by_ref().take(n as u64), &mut io::sink())?;
    } else {
        return Err(bad("response has no content-length and is not chunked"));
    }
    Ok(Response { status })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve_once(reply: &'static [u8]) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(reply);
        });
        port
    }

    fn get(port: u16) -> io::Result<Response> {
        let mut c = TcpStream::connect(("127.0.0.1", port))?;
        request_plain_on(&mut c, "127.0.0.1", port, "GET", "/", &[], &[])
    }

    #[test]
    fn reads_a_content_length_response() {
        let p = serve_once(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
        assert_eq!(get(p).unwrap().status, 200);
    }

    #[test]
    fn reads_a_chunked_response() {
        let p = serve_once(
            b"HTTP/1.1 201 Created\r\ntransfer-encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
        );
        assert_eq!(get(p).unwrap().status, 201);
    }

    #[test]
    fn an_unframed_body_is_an_error_not_a_hang() {
        let p = serve_once(b"HTTP/1.1 200 OK\r\n\r\nbody");
        assert!(get(p).is_err());
    }

    #[test]
    fn a_closed_connection_is_an_error() {
        let p = serve_once(b"");
        assert!(get(p).is_err());
    }
}
