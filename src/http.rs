//! The HTTP/1.1 client that subgraph requests over plain `http://` go
//! through.
//!
//! A request is written and its response read by the task that asked for it,
//! on a connection it holds alone, so there is no connection task to hand it
//! to and no shared state besides a stack of idle connections per subgraph.
//! The response body is read into one buffer and handed over without a copy.

use axum::body::Bytes;
use bytes::{Buf, BytesMut};
use std::{
    io::{ErrorKind, Write},
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Idle connections kept per subgraph.
const MAX_IDLE: usize = 1024;
/// A connection idle for longer is checked before it is used again.
const FRESH: Duration = Duration::from_secs(1);
/// How often a request is tried before its failure is reported.
const ATTEMPTS: u32 = 4;
const MAX_HEADERS: usize = 64;
const MAX_HEAD: usize = 64 * 1024;
/// What `Doc` can index.
const MAX_BODY: usize = u32::MAX as usize - 1;

pub struct Endpoint {
    /// `host:port`.
    address: String,
    /// The request up to the value of `content-length`.
    head: Vec<u8>,
    /// Most recently used last.
    idle: Mutex<Vec<(TcpStream, Instant)>>,
}

struct Failure {
    message: String,
    /// Nothing of a response had arrived.
    unanswered: bool,
}

impl Failure {
    fn new(message: impl ToString, unanswered: bool) -> Failure {
        Failure { message: message.to_string(), unanswered }
    }
}

impl Endpoint {
    /// `None` for URLs this client does not do: anything but `http://`
    /// without credentials.
    pub fn new(url: &str) -> Option<Endpoint> {
        let url = reqwest::Url::parse(url).ok()?;
        if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let host = url.host_str()?;
        let port = url.port_or_known_default()?;
        let mut head = format!("POST {}", url.path());
        if let Some(query) = url.query() {
            head.push('?');
            head.push_str(query);
        }
        head.push_str(" HTTP/1.1\r\nhost: ");
        head.push_str(host);
        if let Some(port) = url.port() {
            head.push_str(&format!(":{}", port));
        }
        head.push_str("\r\ncontent-type: application/json\r\naccept: application/json\r\ncontent-length: ");
        Some(Endpoint {
            address: format!("{}:{}", host, port),
            head: head.into_bytes(),
            idle: Mutex::new(Vec::new()),
        })
    }

    /// An idle connection that looks usable. Without `certain`, one used a
    /// moment ago is taken on trust.
    fn checkout(&self, certain: bool) -> Option<TcpStream> {
        loop {
            let (stream, since) = self.idle.lock().unwrap().pop()?;
            if !certain && since.elapsed() < FRESH {
                return Some(stream);
            }
            // Nothing to read and not closed.
            if matches!(stream.try_read(&mut [0u8; 1]), Err(error) if error.kind() == ErrorKind::WouldBlock) {
                return Some(stream);
            }
        }
    }

    fn checkin(&self, stream: TcpStream) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < MAX_IDLE {
            idle.push((stream, Instant::now()));
        }
    }

    /// Posts `body` and returns the body of the response, whatever its
    /// status. `size` is how large the response is expected to be, headers
    /// included.
    ///
    /// A request that may be sent twice (`repeatable`) is sent again on a new
    /// connection if nothing came back: an idle connection had been closed,
    /// or a new one was reset, which is what a server whose accept queue is
    /// full does. Others only go out on a connection that was seen to be
    /// open, and once. Connecting is tried again either way.
    pub async fn post(&self, body: &[u8], size: usize, repeatable: bool) -> Result<Bytes, String> {
        let mut request = Vec::with_capacity(self.head.len() + 24 + body.len());
        request.extend_from_slice(&self.head);
        write!(request, "{}\r\n\r\n", body.len()).unwrap();
        request.extend_from_slice(body);

        let mut idle = self.checkout(!repeatable);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let reused = idle.is_some();
            let mut stream = match idle.take() {
                Some(stream) => stream,
                None => match self.connect().await {
                    Ok(stream) => stream,
                    Err(_) if attempt < ATTEMPTS => {
                        pause(attempt).await;
                        continue;
                    }
                    Err(error) => return Err(error.to_string()),
                },
            };
            match exchange(&mut stream, &request, size).await {
                Ok((body, keep)) => {
                    if keep {
                        self.checkin(stream);
                    }
                    return Ok(body);
                }
                // A closed idle connection says nothing about the server.
                Err(failure) if failure.unanswered && repeatable && reused => {}
                Err(failure) if failure.unanswered && repeatable && attempt < ATTEMPTS => pause(attempt).await,
                Err(failure) => return Err(failure.message),
            }
        }
    }

    async fn connect(&self) -> std::io::Result<TcpStream> {
        let stream = TcpStream::connect(self.address.as_str()).await?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }
}

/// Waits a little longer after every failed attempt.
async fn pause(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(1 << attempt)).await;
}

/// Reads more of the response. `Ok(false)` at the end of the stream.
async fn fill(stream: &mut TcpStream, buffer: &mut BytesMut) -> Result<bool, Failure> {
    if buffer.capacity() == buffer.len() {
        buffer.reserve(buffer.len().max(4096));
    }
    match stream.read_buf(buffer).await {
        Ok(read) => Ok(read > 0),
        Err(error) => Err(Failure::new(error, buffer.is_empty())),
    }
}

/// Like `fill`, for when the response is known not to be over.
async fn more(stream: &mut TcpStream, buffer: &mut BytesMut) -> Result<(), Failure> {
    match fill(stream, buffer).await? {
        true => Ok(()),
        false => Err(Failure::new("connection closed before the response was complete", buffer.is_empty())),
    }
}

enum Length {
    Known(usize),
    Chunked,
    UntilClose,
}

/// Sends the request and reads the response. Returns the body and whether
/// the connection can be used again.
async fn exchange(stream: &mut TcpStream, request: &[u8], size: usize) -> Result<(Bytes, bool), Failure> {
    stream.write_all(request).await.map_err(|e| Failure::new(e, true))?;

    let mut buffer = BytesMut::with_capacity(size.clamp(4096, MAX_BODY));
    let (head, length, mut keep) = loop {
        more(stream, &mut buffer).await?;
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut response = httparse::Response::new(&mut headers);
        let head = match response.parse(&buffer) {
            Ok(httparse::Status::Complete(head)) => head,
            Ok(httparse::Status::Partial) if buffer.len() < MAX_HEAD => continue,
            Ok(httparse::Status::Partial) => return Err(Failure::new("response headers are too large", false)),
            Err(error) => return Err(Failure::new(format!("invalid response: {}", error), false)),
        };
        let status = response.code.unwrap_or(0);
        if (100..200).contains(&status) {
            // An interim response: the real one follows.
            buffer.advance(head);
            continue;
        }
        let mut keep = response.version == Some(1);
        let mut length = match status {
            204 | 304 => Length::Known(0),
            _ => Length::UntilClose,
        };
        for header in response.headers.iter() {
            let value = std::str::from_utf8(header.value).unwrap_or_default().trim();
            if header.name.eq_ignore_ascii_case("content-length") {
                if !matches!(length, Length::Chunked) {
                    let length_value = value.parse().ok().filter(|length| *length <= MAX_BODY);
                    length = Length::Known(length_value.ok_or_else(|| Failure::new("invalid content-length", false))?);
                }
            } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
                if value.to_ascii_lowercase().split(',').any(|coding| coding.trim() == "chunked") {
                    length = Length::Chunked;
                }
            } else if header.name.eq_ignore_ascii_case("connection") {
                for option in value.split(',') {
                    if option.trim().eq_ignore_ascii_case("close") {
                        keep = false;
                    } else if option.trim().eq_ignore_ascii_case("keep-alive") {
                        keep = true;
                    }
                }
            }
        }
        break (head, length, keep);
    };

    match length {
        Length::Known(length) => {
            let end = head + length;
            if buffer.len() < end {
                buffer.reserve(end - buffer.len());
            }
            while buffer.len() < end {
                more(stream, &mut buffer).await?;
            }
            // Nothing should follow a response we have not asked for.
            keep &= buffer.len() == end;
            buffer.truncate(end);
            buffer.advance(head);
            Ok((buffer.freeze(), keep))
        }
        Length::UntilClose => {
            while fill(stream, &mut buffer).await? {
                if buffer.len() > MAX_BODY {
                    return Err(Failure::new("response is too large", false));
                }
            }
            buffer.advance(head);
            Ok((buffer.freeze(), false))
        }
        Length::Chunked => {
            let mut body = Vec::new();
            let mut at = head;
            loop {
                let line = line_end(stream, &mut buffer, at).await?;
                let digits = buffer[at..line].split(|byte| *byte == b';').next().unwrap_or_default();
                let chunk = std::str::from_utf8(digits)
                    .ok()
                    .and_then(|digits| usize::from_str_radix(digits.trim(), 16).ok())
                    .filter(|chunk| *chunk <= MAX_BODY - body.len())
                    .ok_or_else(|| Failure::new("invalid chunk size", false))?;
                at = line + 2;
                if chunk == 0 {
                    // Trailers, up to an empty line.
                    loop {
                        let line = line_end(stream, &mut buffer, at).await?;
                        let empty = line == at;
                        at = line + 2;
                        if empty {
                            break;
                        }
                    }
                    break;
                }
                while buffer.len() < at + chunk + 2 {
                    more(stream, &mut buffer).await?;
                }
                body.extend_from_slice(&buffer[at..at + chunk]);
                if &buffer[at + chunk..at + chunk + 2] != b"\r\n" {
                    return Err(Failure::new("invalid chunk", false));
                }
                at += chunk + 2;
            }
            keep &= at == buffer.len();
            Ok((body.into(), keep))
        }
    }
}

/// Where the line that starts at `at` ends, reading until it is complete.
async fn line_end(stream: &mut TcpStream, buffer: &mut BytesMut, at: usize) -> Result<usize, Failure> {
    let mut searched = at;
    loop {
        if let Some(found) = buffer[searched..].windows(2).position(|pair| pair == b"\r\n") {
            return Ok(searched + found);
        }
        searched = buffer.len().saturating_sub(1).max(at);
        if buffer.len() - at > MAX_HEAD {
            return Err(Failure::new("invalid chunked response", false));
        }
        more(stream, buffer).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A server that answers each connection's requests with the next of
    /// `responses`, and closes the connection after one marked with `!`.
    async fn serve(responses: Vec<&'static str>) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut responses = responses.into_iter();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 4096];
                loop {
                    let Ok(read) = stream.read(&mut request).await else { break };
                    if read == 0 {
                        break;
                    }
                    let Some(response) = responses.next() else { break };
                    // `!` alone: closed without an answer.
                    let (response, close) = match response.strip_prefix('!') {
                        Some(response) => (response, true),
                        None => (response, false),
                    };
                    // In two pieces, so the client has to read more than once.
                    let (first, second) = response.split_at(response.len() / 2);
                    stream.write_all(first.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    stream.write_all(second.as_bytes()).await.unwrap();
                    if close {
                        break;
                    }
                }
            }
        });
        Endpoint::new(&format!("http://{}/graphql?a=1", address)).unwrap()
    }

    #[test]
    fn only_plain_http() {
        assert!(Endpoint::new("https://example.com/graphql").is_none());
        assert!(Endpoint::new("http://user:secret@example.com/graphql").is_none());
        let endpoint = Endpoint::new("http://example.com/graphql?a=1").unwrap();
        assert_eq!(endpoint.address, "example.com:80");
        assert!(endpoint.head.starts_with(b"POST /graphql?a=1 HTTP/1.1\r\nhost: example.com\r\n"));
        let endpoint = Endpoint::new("http://[::1]:4001").unwrap();
        assert_eq!(endpoint.address, "[::1]:4001");
        assert!(endpoint.head.starts_with(b"POST / HTTP/1.1\r\nhost: [::1]:4001\r\n"));
    }

    #[tokio::test]
    async fn reads_bodies_of_every_framing() {
        let endpoint = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"data\":{}}",
            "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}",
            "HTTP/1.1 500 Oops\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3;x=y\r\n:1}\r\n0\r\nt: 1\r\n\r\n",
            "HTTP/1.1 204 No Content\r\n\r\n",
            "!HTTP/1.1 200 OK\r\n\r\n[1,2,3]",
        ])
        .await;
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "{\"data\":{}}");
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "{}");
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "{\"a\":1}");
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "");
        // One connection served all of those.
        assert_eq!(endpoint.idle.lock().unwrap().len(), 1);
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "[1,2,3]");
        assert_eq!(endpoint.idle.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn replaces_a_closed_connection() {
        let endpoint = serve(vec![
            "!HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n1",
            "HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n2",
            "!HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n3",
            "HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n4",
        ])
        .await;
        // The server closes the connection without saying so.
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "1");
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Taken on trust, found closed, sent again.
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "2");
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "3");
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Checked first, so it is only sent once.
        assert_eq!(endpoint.post(b"{}", 0, false).await.unwrap(), "4");
    }

    #[tokio::test]
    async fn tries_again_when_nothing_came_back() {
        let endpoint = serve(vec!["!", "!", "HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n1", "!"]).await;
        assert_eq!(endpoint.post(b"{}", 0, true).await.unwrap(), "1");
        // Not what is only to be sent once.
        endpoint.idle.lock().unwrap().clear();
        assert!(endpoint.post(b"{}", 0, false).await.is_err());
        // Nor without end.
        let endpoint = serve(vec!["!"; 8]).await;
        assert!(endpoint.post(b"{}", 0, true).await.is_err());
        // Nobody listening.
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let endpoint = Endpoint::new(&format!("http://127.0.0.1:{}/", port)).unwrap();
        assert!(endpoint.post(b"{}", 0, true).await.is_err());
    }

    #[tokio::test]
    async fn reports_a_response_cut_short() {
        let endpoint = serve(vec!["!HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\n12345"]).await;
        assert!(endpoint.post(b"{}", 0, true).await.is_err());
    }
}
