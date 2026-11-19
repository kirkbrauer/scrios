//! A small HTTPS client for the developer disk image's files, with the limits a download of someone else's bytes
//! needs: https only, a redirect only to the same host or to GitHub's raw file hosts, a size cap, and timeouts.
//!
//! idevice has an HTTP client of its own (it is what Apple's signing server is asked through), but it follows a
//! redirect to any host and any scheme, reads a body of any size into memory and has no timeout; that is not what is
//! wanted for a file that scrios will hand to a device. This one is the few hundred lines that are: a `GET`, a status
//! line and headers read within a bound, a body read within a bound, TLS by `rustls` on the same `ring` backend, with
//! the Mozilla roots (as idevice uses) and the host's own trust store and `$SSL_CERT_FILE` (so a proxy that inspects
//! TLS with a CA the host trusts is not an error). It speaks `HTTP/1.1` with `Connection: close`, no compression, no cookies, no
//! proxy settings.
//!
//! What is checked is not what is trusted: the files are checked against SHA-256 values that are in the program (or in
//! a manifest the user gave) before anything is done with them ([`crate::ddi::fetch`]). The limits here keep a wrong
//! server from filling the disk or the memory, or sending the client somewhere else.
//!
//! Plain `http://` URLs exist only in the unit tests of this crate (a fake server on the loopback); the code that
//! accepts them is not compiled into the program.

// A response is bytes from the network: a malformed one is an error, never a panic, so the lints that find the panics
// are denied here (the tests below may use them).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

/// The hosts of GitHub's raw file service: a request to one may be answered with a redirect to the other.
pub const GITHUB_RAW_HOSTS: [&str; 2] = ["raw.githubusercontent.com", "objects.githubusercontent.com"];
/// How many redirects one download follows.
pub const MAX_REDIRECTS: u8 = 3;
/// The largest status line and headers that are read.
const MAX_HEAD: usize = 16 * 1024;
/// The most headers that are read.
const MAX_HEADERS: usize = 100;
/// The largest chunk-size line (hex digits and an extension) of a chunked body.
const MAX_SIZE_LINE: usize = 64;
/// The most bytes of trailers after a chunked body.
const MAX_TRAILER: usize = 8 * 1024;
/// How much a read takes at once.
const READ_SIZE: usize = 64 * 1024;

/// Why a download failed (never with the part of a URL that can be a secret: only the host is named).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The URL is not one that is downloaded from (the reason).
    BadUrl(String),
    /// A redirect to somewhere that is not allowed (the reason).
    RedirectRefused(String),
    /// More than [`MAX_REDIRECTS`] redirects.
    TooManyRedirects,
    /// The connection or the TLS handshake failed (the reason).
    Connect(String),
    /// Reading or writing failed (the reason).
    Io(String),
    /// A step took longer than it may (what).
    Timeout(&'static str),
    /// The server's answer is not one this client takes (the reason).
    BadResponse(String),
    /// The server answered with this status instead of 200.
    Status(u16),
    /// The body is, or would be, larger than the limit.
    TooLarge { limit: u64 },
    /// The connection ended before the body did.
    Truncated { got: u64, want: u64 },
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::BadUrl(why) => write!(f, "not a URL to download from: {why}"),
            HttpError::RedirectRefused(why) => write!(f, "a redirect that is not followed: {why}"),
            HttpError::TooManyRedirects => write!(f, "more than {MAX_REDIRECTS} redirects"),
            HttpError::Connect(why) => write!(f, "cannot connect: {why}"),
            HttpError::Io(why) => write!(f, "the connection failed: {why}"),
            HttpError::Timeout(what) => write!(f, "{what} took too long"),
            HttpError::BadResponse(why) => write!(f, "the server's answer is not usable: {why}"),
            HttpError::Status(status) => write!(f, "the server answered {status}"),
            HttpError::TooLarge { limit } => write!(f, "the file is larger than the {limit} bytes it may be"),
            HttpError::Truncated { got, want } => write!(f, "the connection ended after {got} of {want} bytes"),
        }
    }
}

impl std::error::Error for HttpError {}

// ---- URLs ---------------------------------------------------------------------------------------

/// A URL that is downloaded from: `https` (and in the unit tests, `http`), a host, an optional port, and a path with an
/// optional query. No user name or password, no fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

/// The scheme of a URL and what follows it. `plain_ok` lets `http://` through: only the tests of this crate, which talk
/// to a fake server on the loopback, ask for that (`cfg!(test)`); the program never does.
fn split_scheme(text: &str, plain_ok: bool) -> Result<(bool, &str), HttpError> {
    if let Some(rest) = text.strip_prefix("https://") {
        return Ok((true, rest));
    }
    if plain_ok && let Some(rest) = text.strip_prefix("http://") {
        return Ok((false, rest));
    }
    Err(HttpError::BadUrl("it must start with https://".into()))
}

fn bad(why: &str) -> HttpError {
    HttpError::BadUrl(why.into())
}

/// Whether `host` is a name or an address that may be connected to: letters, digits, `-` and `.`, in labels that are
/// not empty, or a bracketed IPv6 address.
fn valid_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
    }
    host.len() <= 253
        && !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Whether `path` (with its query) is made of what a URL may carry here, and has no `.` or `..` segment.
fn valid_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"-._~/?=&+,:;@!$'()*[]%".contains(&b);
    if !path.bytes().all(allowed) {
        return false;
    }
    // A `%` escape is two hex digits.
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%'
            && !(bytes.next().is_some_and(|h| h.is_ascii_hexdigit())
                && bytes.next().is_some_and(|h| h.is_ascii_hexdigit()))
        {
            return false;
        }
    }
    let route = path.split('?').next().unwrap_or(path);
    !route.split('/').any(|segment| segment == ".." || segment == ".")
}

impl Url {
    /// Parses `text`.
    ///
    /// # Errors
    ///
    /// [`HttpError::BadUrl`] if it is not an `https://` URL of a host (and port) and a path that are well formed, or has
    /// a user name, a password or a fragment.
    pub fn parse(text: &str) -> Result<Url, HttpError> {
        Url::parse_with(text, cfg!(test))
    }

    /// [`Url::parse`], with plain `http://` allowed or not (`parse` allows it in the tests of this crate only).
    fn parse_with(text: &str, plain_ok: bool) -> Result<Url, HttpError> {
        let (tls, rest) = split_scheme(text, plain_ok)?;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at_checked(end).ok_or_else(|| bad("it has no host"))?;
        if authority.contains('@') {
            return Err(bad("it has a user name or password, which scrios does not send"));
        }
        let (host, port) = match authority.rfind(':') {
            // `[::1]` has colons in its host: a port comes after the closing bracket.
            Some(colon) if !authority.get(colon..).is_some_and(|after| after.contains(']')) => {
                let (host, port) = authority.split_at_checked(colon).ok_or_else(|| bad("it has no host"))?;
                let port = port.get(1..).unwrap_or_default();
                let port: u16 = port.parse().map_err(|_| bad("its port is not a number"))?;
                if port == 0 {
                    return Err(bad("its port is 0"));
                }
                (host, Some(port))
            }
            _ => (authority, None),
        };
        if !valid_host(host) {
            return Err(bad("its host is not a name or an address"));
        }
        let path = if tail.starts_with('/') {
            tail.to_string()
        } else if tail.is_empty() {
            "/".to_string()
        } else if tail.starts_with('?') {
            format!("/{tail}")
        } else {
            return Err(bad("it has a fragment"));
        };
        if !valid_path(&path) {
            return Err(bad("its path has a character, an escape or a `..` that is not taken"));
        }
        Ok(Url { tls, host: host.to_ascii_lowercase(), port: port.unwrap_or(if tls { 443 } else { 80 }), path })
    }

    /// The host (without a port).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path and query (`/a/b?c=d`).
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the URL is `https`.
    #[must_use]
    pub fn is_secure(&self) -> bool {
        self.tls
    }

    /// The host, and the port when it is not the scheme's own (what the `Host` header says).
    #[must_use]
    pub fn authority(&self) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default { self.host.clone() } else { format!("{}:{}", self.host, self.port) }
    }

    /// The URL as text, for a request that is not logged: it can be a secret (a mirror's token is in a URL's query).
    #[must_use]
    pub fn as_text(&self) -> String {
        format!("{}://{}{}", if self.tls { "https" } else { "http" }, self.authority(), self.path)
    }
}

/// The URL that a redirect with this `Location` leads to from `from`, if it is one that is followed: `https`, and the
/// same host and port, or one of [`GITHUB_RAW_HOSTS`] (on the port 443).
///
/// # Errors
///
/// [`HttpError::RedirectRefused`] for a redirect to another host, to plain `http`, to another form of address than a
/// path or a full URL, or to a URL that is not well formed.
pub fn redirect_target(from: &Url, location: &str) -> Result<Url, HttpError> {
    redirect_target_with(from, location, cfg!(test))
}

/// [`redirect_target`], with plain `http://` URLs allowed or not.
fn redirect_target_with(from: &Url, location: &str, plain_ok: bool) -> Result<Url, HttpError> {
    let refused = |why: String| HttpError::RedirectRefused(why);
    let target = if location.starts_with("//") {
        return Err(refused("a scheme-relative Location".into()));
    } else if location.starts_with('/') {
        // The same host, another path.
        Url::parse_with(
            &format!("{}://{}{location}", if from.tls { "https" } else { "http" }, from.authority()),
            plain_ok,
        )
        .map_err(|e| refused(e.to_string()))?
    } else if location.contains("://") {
        Url::parse_with(location, plain_ok).map_err(|e| refused(e.to_string()))?
    } else {
        return Err(refused("a relative Location that is not a path".into()));
    };
    if from.tls && !target.tls {
        return Err(refused(format!("to plain http at {}", target.host)));
    }
    let same = target.host == from.host && target.port == from.port;
    let github = target.tls && target.port == 443 && GITHUB_RAW_HOSTS.contains(&target.host.as_str());
    if same || github {
        Ok(target)
    } else {
        Err(refused(format!("to {}, which is neither {} nor a GitHub raw host", target.host, from.host)))
    }
}

// ---- the answer ---------------------------------------------------------------------------------

/// What is read from the status line and the headers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    status: u16,
    content_length: Option<u64>,
    chunked: bool,
    location: Option<String>,
}

/// Parses the status line and headers (without the blank line that ends them).
fn parse_head(head: &[u8]) -> Result<Head, HttpError> {
    let invalid = |why: &str| HttpError::BadResponse(why.into());
    let text = std::str::from_utf8(head).map_err(|_| invalid("the headers are not text"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or_else(|| invalid("no status line"))?;
    let (version, rest) = status_line.split_once(' ').ok_or_else(|| invalid("a status line without a status"))?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(invalid("not an HTTP/1 answer"));
    }
    let code = rest.get(..3).ok_or_else(|| invalid("a status that is not three digits"))?;
    let status: u16 = code.parse().map_err(|_| invalid("a status that is not three digits"))?;
    let mut parsed = Head { status, content_length: None, chunked: false, location: None };
    let mut count = 0_usize;
    let mut encoded = false;
    for line in lines {
        count = count.saturating_add(1);
        if count > MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| invalid("a header without a colon"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let length: u64 = value.parse().map_err(|_| invalid("a Content-Length that is not a number"))?;
            if parsed.content_length.is_some_and(|before| before != length) {
                return Err(invalid("two different Content-Length headers"));
            }
            parsed.content_length = Some(length);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // `chunked` alone is taken; anything else (a compression, a list) is not.
            if value.eq_ignore_ascii_case("chunked") {
                parsed.chunked = true;
            } else {
                return Err(invalid("a transfer coding that is not chunked"));
            }
        } else if name.eq_ignore_ascii_case("content-encoding") {
            encoded |= !value.eq_ignore_ascii_case("identity");
        } else if name.eq_ignore_ascii_case("location") {
            parsed.location = Some(value.to_string());
        }
    }
    if encoded {
        return Err(invalid("a Content-Encoding (the file would not be what was signed)"));
    }
    if parsed.chunked && parsed.content_length.is_some() {
        return Err(invalid("both a Content-Length and a chunked body"));
    }
    Ok(parsed)
}

/// Where a chunked body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// Reading the hex size.
    Size,
    /// After the size: skipping an extension up to the end of the line.
    Extension,
    /// The line feed after the size.
    SizeLf,
    /// This many bytes of data are left in the chunk.
    Data(u64),
    /// The carriage return after the data.
    DataCr,
    /// The line feed after the data.
    DataLf,
    /// After the last chunk: trailers, up to the empty line. The length of the line so far.
    Trailer(usize),
    /// The line feed that ends the trailers' empty line.
    TrailerLf,
    /// The body is complete.
    Done,
}

/// A decoder of a chunked body, fed in pieces of any size.
#[derive(Debug)]
struct ChunkDecoder {
    state: Chunk,
    /// The hex digits of the size being read.
    digits: Vec<u8>,
    /// The bytes of trailers so far.
    trailer: usize,
}

impl ChunkDecoder {
    fn new() -> ChunkDecoder {
        ChunkDecoder { state: Chunk::Size, digits: Vec::new(), trailer: 0 }
    }

    /// Takes `input`, appends the data in it to `out` (which may not grow past `max` bytes), and says whether the body
    /// is complete (what comes after it, if anything, is not read).
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, max: u64) -> Result<bool, HttpError> {
        let malformed = || HttpError::BadResponse("a chunked body that is not well formed".into());
        let mut rest = input;
        while !rest.is_empty() && self.state != Chunk::Done {
            if let Chunk::Data(left) = self.state {
                let take = usize::try_from(left).unwrap_or(usize::MAX).min(rest.len());
                let (data, tail) = rest.split_at_checked(take).ok_or_else(malformed)?;
                let size = u64::try_from(out.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(take).unwrap_or(u64::MAX));
                if size > max {
                    return Err(HttpError::TooLarge { limit: max });
                }
                out.extend_from_slice(data);
                rest = tail;
                let left = left.saturating_sub(u64::try_from(take).unwrap_or(u64::MAX));
                self.state = if left == 0 { Chunk::DataCr } else { Chunk::Data(left) };
                continue;
            }
            let (&byte, tail) = rest.split_first().ok_or_else(malformed)?;
            rest = tail;
            self.state = match (self.state, byte) {
                (Chunk::Size, b'\r') => {
                    if self.digits.is_empty() {
                        return Err(malformed());
                    }
                    Chunk::SizeLf
                }
                (Chunk::Size, b';') => {
                    if self.digits.is_empty() {
                        return Err(malformed());
                    }
                    Chunk::Extension
                }
                (Chunk::Size, digit) if digit.is_ascii_hexdigit() => {
                    if self.digits.len() >= 16 {
                        return Err(malformed());
                    }
                    self.digits.push(digit);
                    Chunk::Size
                }
                (Chunk::Extension, b'\r') => Chunk::SizeLf,
                (Chunk::Extension, _) => {
                    self.trailer = self.trailer.saturating_add(1);
                    if self.trailer > MAX_SIZE_LINE {
                        return Err(malformed());
                    }
                    Chunk::Extension
                }
                (Chunk::SizeLf, b'\n') => {
                    let text = std::str::from_utf8(&self.digits).map_err(|_| malformed())?;
                    let size = u64::from_str_radix(text, 16).map_err(|_| malformed())?;
                    self.digits.clear();
                    self.trailer = 0;
                    if size == 0 { Chunk::Trailer(0) } else { Chunk::Data(size) }
                }
                (Chunk::DataCr, b'\r') => Chunk::DataLf,
                (Chunk::DataLf, b'\n') => Chunk::Size,
                (Chunk::Trailer(0), b'\r') => Chunk::TrailerLf,
                (Chunk::Trailer(_), b'\n') => Chunk::Trailer(0),
                (Chunk::Trailer(length), b'\r') => Chunk::Trailer(length),
                (Chunk::Trailer(length), _) => {
                    self.trailer = self.trailer.saturating_add(1);
                    if self.trailer > MAX_TRAILER {
                        return Err(malformed());
                    }
                    Chunk::Trailer(length.saturating_add(1))
                }
                (Chunk::TrailerLf, b'\n') => Chunk::Done,
                _ => return Err(malformed()),
            };
        }
        Ok(self.state == Chunk::Done)
    }
}

// ---- the exchange -------------------------------------------------------------------------------

/// What a request is limited by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The largest body that is read.
    pub max_bytes: u64,
    /// How long connecting (and the TLS handshake) may take.
    pub connect: Duration,
    /// How long the server may take to send the next bytes.
    pub idle: Duration,
    /// How long the whole download may take.
    pub total: Duration,
    /// How many redirects are followed.
    pub max_redirects: u8,
}

impl Limits {
    /// The limits of a download of a file of at most `max_bytes`: 20 s to connect, 30 s of silence, 15 minutes in all,
    /// [`MAX_REDIRECTS`] redirects.
    #[must_use]
    pub fn for_file(max_bytes: u64) -> Limits {
        Limits {
            max_bytes,
            connect: Duration::from_secs(20),
            idle: Duration::from_secs(30),
            total: Duration::from_secs(15 * 60),
            max_redirects: MAX_REDIRECTS,
        }
    }
}

/// The result of one request: the body, or where to ask next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    Body(Vec<u8>),
    Redirect(String),
}

fn io_error(e: &std::io::Error) -> HttpError {
    HttpError::Io(e.kind().to_string())
}

/// One read within the idle limit.
async fn read_some<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut [u8], idle: Duration) -> Result<usize, HttpError> {
    tokio::time::timeout(idle, stream.read(buf))
        .await
        .map_err(|_| HttpError::Timeout("waiting for the server"))?
        .map_err(|e| io_error(&e))
}

/// Sends the `GET` for `url` on `stream` and reads the answer within `limits`; `progress` is told how many bytes of the body
/// there are so far.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    url: &Url,
    limits: &Limits,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<Reply, HttpError> {
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: scrios/{}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        url.path(),
        url.authority(),
        env!("CARGO_PKG_VERSION")
    );
    tokio::time::timeout(limits.idle, async {
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| HttpError::Timeout("sending the request"))?
    .map_err(|e| io_error(&e))?;

    // The head: up to the blank line, within MAX_HEAD. What was read after it is the start of the body.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; READ_SIZE];
    let head_end = loop {
        if let Some(at) = find_blank_line(&buf) {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpError::BadResponse("the headers are too long".into()));
        }
        let n = read_some(stream, &mut chunk, limits.idle).await?;
        if n == 0 {
            return Err(HttpError::BadResponse("the connection ended before the headers did".into()));
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    };
    let (head_bytes, after) =
        buf.split_at_checked(head_end).ok_or_else(|| HttpError::BadResponse("no headers".into()))?;
    let head = parse_head(head_bytes)?;
    let mut body_start: Vec<u8> = after.get(4..).unwrap_or_default().to_vec();

    match head.status {
        200 => {}
        301 | 302 | 303 | 307 | 308 => {
            return head
                .location
                .map(Reply::Redirect)
                .ok_or_else(|| HttpError::BadResponse("a redirect without a Location".into()));
        }
        other => return Err(HttpError::Status(other)),
    }

    let max = limits.max_bytes;
    let mut body: Vec<u8> = Vec::new();
    if head.chunked {
        let mut decoder = ChunkDecoder::new();
        let mut done = decoder.feed(&body_start, &mut body, max)?;
        progress(u64::try_from(body.len()).unwrap_or(u64::MAX));
        while !done {
            let n = read_some(stream, &mut chunk, limits.idle).await?;
            if n == 0 {
                return Err(HttpError::Truncated { got: u64::try_from(body.len()).unwrap_or(u64::MAX), want: 0 });
            }
            done = decoder.feed(chunk.get(..n).unwrap_or_default(), &mut body, max)?;
            progress(u64::try_from(body.len()).unwrap_or(u64::MAX));
        }
        return Ok(Reply::Body(body));
    }
    if let Some(length) = head.content_length {
        if length > max {
            return Err(HttpError::TooLarge { limit: max });
        }
        body_start.truncate(usize::try_from(length).unwrap_or(usize::MAX));
        body = body_start;
        progress(u64::try_from(body.len()).unwrap_or(u64::MAX));
        while u64::try_from(body.len()).unwrap_or(u64::MAX) < length {
            let n = read_some(stream, &mut chunk, limits.idle).await?;
            if n == 0 {
                return Err(HttpError::Truncated { got: u64::try_from(body.len()).unwrap_or(u64::MAX), want: length });
            }
            let want = usize::try_from(length).unwrap_or(usize::MAX).saturating_sub(body.len());
            body.extend_from_slice(chunk.get(..n.min(want)).unwrap_or_default());
            progress(u64::try_from(body.len()).unwrap_or(u64::MAX));
        }
        return Ok(Reply::Body(body));
    }
    // No length and not chunked: the body ends when the server closes the connection.
    body = body_start;
    progress(u64::try_from(body.len()).unwrap_or(u64::MAX));
    loop {
        if u64::try_from(body.len()).unwrap_or(u64::MAX) > max {
            return Err(HttpError::TooLarge { limit: max });
        }
        let n = read_some(stream, &mut chunk, limits.idle).await?;
        if n == 0 {
            return Ok(Reply::Body(body));
        }
        body.extend_from_slice(chunk.get(..n).unwrap_or_default());
        progress(u64::try_from(body.len()).unwrap_or(u64::MAX));
    }
}

/// The offset of the first `\r\n\r\n` in `buf`, if there is one.
fn find_blank_line(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---- the connection -----------------------------------------------------------------------------

/// The certificate authorities a server may be signed by: the Mozilla roots that are built in, and with them the ones
/// the host trusts (its trust store: the keychain on macOS, the certificate store on Windows, the OpenSSL folders on
/// Linux; and the files that `$SSL_CERT_FILE` and `$SSL_CERT_DIR` name, on every host). A proxy that inspects TLS
/// signs with a CA of its own, which only the host knows. A host with no trust store at all still has the built-in
/// roots, so GitHub is reached from a bare container too.
fn root_store() -> rustls::RootCertStore {
    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        debug!("reading the host's trusted certificates: {e}");
    }
    let roots = roots_with(native.certs);
    debug!("TLS roots: {} ({} built in)", roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    roots
}

/// The built-in roots and `extra` (those of `extra` that are not certificates a chain can end at are left out).
fn roots_with(extra: Vec<rustls::pki_types::CertificateDer<'static>>) -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let (_, ignored) = roots.add_parsable_certificates(extra);
    if ignored > 0 {
        debug!("{ignored} of the host's trusted certificates are not usable as roots");
    }
    roots
}

/// The TLS settings: the roots of [`root_store`], the protocol versions `rustls` takes by default, on the `ring`
/// provider. Made once: reading the host's trust store is not free.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, HttpError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, HttpError>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| HttpError::Connect(format!("TLS: {e}")))?
                .with_root_certificates(root_store())
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

/// What a failed TLS handshake is reported as. A certificate signed by nobody the host trusts is, on a network that
/// inspects TLS, the proxy's: the message says how to make its CA known.
fn tls_error(e: &std::io::Error) -> HttpError {
    let text = e.to_string();
    if text.contains("UnknownIssuer") {
        return HttpError::Connect(format!(
            "TLS: {text}: the server's certificate is signed by an authority this host does not trust. If a proxy on \
             this network inspects TLS, add its CA certificate to the system's trust store, or point SSL_CERT_FILE at \
             a PEM file that holds it"
        ));
    }
    HttpError::Connect(format!("TLS: {text}"))
}

/// Connects to `url`, sends the request and reads the answer.
async fn request(url: &Url, limits: &Limits, progress: &mut (dyn FnMut(u64) + Send)) -> Result<Reply, HttpError> {
    // A bracketed IPv6 address is connected to without its brackets.
    let host = url.host().trim_start_matches('[').trim_end_matches(']').to_string();
    let connect = async {
        tokio::time::timeout(limits.connect, TcpStream::connect((host.as_str(), url.port())))
            .await
            .map_err(|_| HttpError::Timeout("connecting"))?
            .map_err(|e| HttpError::Connect(e.kind().to_string()))
    };
    let tcp = connect.await?;
    #[cfg(test)]
    if !url.is_secure() {
        let mut tcp = tcp;
        return exchange(&mut tcp, url, limits, progress).await;
    }
    let server_name = rustls::pki_types::ServerName::try_from(host)
        .map_err(|_| HttpError::Connect("the host is not a valid TLS name".into()))?;
    let connector = tokio_rustls::TlsConnector::from(tls_config()?);
    let mut tls = tokio::time::timeout(limits.connect, connector.connect(server_name, tcp))
        .await
        .map_err(|_| HttpError::Timeout("the TLS handshake"))?
        .map_err(|e| tls_error(&e))?;
    exchange(&mut tls, url, limits, progress).await
}

/// Downloads `url` within `limits`: follows the redirects that [`redirect_target`] allows, and returns the body of the
/// 200 answer. `progress` is told how many bytes of the body there are so far.
///
/// # Errors
///
/// An [`HttpError`]: the connection, the answer, a redirect that is not allowed, a body that is too large, a timeout.
pub async fn get(url: &Url, limits: &Limits, progress: &mut (dyn FnMut(u64) + Send)) -> Result<Vec<u8>, HttpError> {
    let all = async {
        let mut url = url.clone();
        let mut followed = 0_u8;
        loop {
            match Box::pin(request(&url, limits, progress)).await? {
                Reply::Body(body) => return Ok(body),
                Reply::Redirect(location) => {
                    if followed >= limits.max_redirects {
                        return Err(HttpError::TooManyRedirects);
                    }
                    followed = followed.saturating_add(1);
                    url = redirect_target(&url, &location)?;
                }
            }
        }
    };
    tokio::time::timeout(limits.total, all).await.map_err(|_| HttpError::Timeout("the download"))?
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "test code: a panic is a failed test, and the inputs are small, fixed values"
    )]
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn https(text: &str) -> Url {
        Url::parse_with(text, false).unwrap()
    }

    // ---- URLs and redirects: the policy, as the program has it (https only) ----------------------

    #[test]
    fn a_url_is_https_with_a_host_a_port_and_a_path_and_nothing_else() {
        let url = https("https://Raw.GitHubUserContent.com/o/r/abc/PersonalizedImages/X/Image.dmg");
        assert_eq!((url.host(), url.port(), url.is_secure()), ("raw.githubusercontent.com", 443, true));
        assert_eq!(url.path(), "/o/r/abc/PersonalizedImages/X/Image.dmg");
        assert_eq!(url.authority(), "raw.githubusercontent.com");
        let url = https("https://mirror.example:8443/a?b=c");
        assert_eq!((url.host(), url.port(), url.path()), ("mirror.example", 8443, "/a?b=c"));
        assert_eq!(url.authority(), "mirror.example:8443");
        assert_eq!(url.as_text(), "https://mirror.example:8443/a?b=c");
        assert_eq!(https("https://h").path(), "/");
        assert_eq!(https("https://h?x=1").path(), "/?x=1");
        let v6 = https("https://[::1]:8443/x");
        assert_eq!((v6.host(), v6.port()), ("[::1]", 8443));
        assert_eq!(https("https://[2001:db8::1]/x").port(), 443);
        for bad in [
            "http://example.com/x",
            "ftp://example.com/x",
            "example.com/x",
            "//example.com/x",
            "https://",
            "https:///x",
            "https://user@example.com/x",
            "https://user:pw@example.com/x",
            "https://example.com:0/x",
            "https://example.com:99999/x",
            "https://example.com:port/x",
            "https://exa mple.com/x",
            "https://-example.com/x",
            "https://example..com/x",
            "https://example.com/x#fragment",
            "https://example.com/a/../b",
            "https://example.com/a/./b",
            "https://example.com/a b",
            "https://example.com/a\\b",
            "https://example.com/%zz",
            "https://example.com/%4",
            "https://example.com/\u{e9}",
            "https://[]/x",
            "https://[zz]/x",
        ] {
            assert!(Url::parse_with(bad, false).is_err(), "{bad} was taken");
        }
        // A percent escape is taken, and the message never carries the URL (it can hold a secret).
        assert!(Url::parse_with("https://example.com/a%20b", false).is_ok());
        let why = Url::parse_with("https://example.com/x?token=secret#f", false).unwrap_err().to_string();
        assert!(!why.contains("secret"), "{why}");
    }

    #[test]
    fn plain_http_is_only_for_the_tests_that_ask_for_it() {
        assert!(Url::parse_with("http://127.0.0.1:8080/x", false).is_err());
        let plain = Url::parse_with("http://127.0.0.1:8080/x", true).unwrap();
        assert!(!plain.is_secure());
        // `parse`, which the program calls, is the first in a program and the second in these tests.
        assert_eq!(Url::parse("http://127.0.0.1:8080/x").is_ok(), cfg!(test));
        // A redirect from https never goes to plain http, nor from plain http to another host.
        let from = https("https://raw.githubusercontent.com/o/r/abc/f");
        assert!(redirect_target_with(&from, "http://raw.githubusercontent.com/x", false).is_err());
        assert!(matches!(
            redirect_target_with(&from, "http://raw.githubusercontent.com/x", true),
            Err(HttpError::RedirectRefused(why)) if why.contains("plain http")
        ));
    }

    #[test]
    fn a_redirect_goes_to_the_same_host_or_to_github_raw_over_https_and_nowhere_else() {
        let github = https("https://raw.githubusercontent.com/o/r/abc/PersonalizedImages/X/Image.dmg");
        let follow = |from: &Url, location: &str| redirect_target_with(from, location, false).map(|u| u.as_text());
        // Allowed: another path of the host, GitHub's two raw hosts.
        assert_eq!(follow(&github, "/o/r2/abc/f").unwrap(), "https://raw.githubusercontent.com/o/r2/abc/f");
        assert_eq!(
            follow(
                &github,
                "https://objects.githubusercontent.com/github-production-repository-file/x?X-Amz-Signature=1"
            )
            .unwrap(),
            "https://objects.githubusercontent.com/github-production-repository-file/x?X-Amz-Signature=1"
        );
        assert!(follow(&github, "https://raw.githubusercontent.com/o/r/abc/g").is_ok());
        assert!(follow(&github, "https://RAW.GITHUBUSERCONTENT.COM/o/r/abc/g").is_ok());
        // Refused: another host, in any spelling that tries to look like GitHub's.
        for location in [
            "https://evil.example/x",
            "https://github.com/o/r/raw/abc/f",
            "https://raw.githubusercontent.com.evil.example/x",
            "https://evil.example/raw.githubusercontent.com/x",
            "https://raw.githubusercontent.com@evil.example/x",
            "https://evil.example@raw.githubusercontent.com/x",
            "https://raw.githubusercontent.com:8443/x",
            "https://objects.githubusercontent.com:8443/x",
            "http://raw.githubusercontent.com/x",
            "//evil.example/x",
            "x/y",
            "../x",
            "ftp://raw.githubusercontent.com/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "",
        ] {
            assert!(
                matches!(follow(&github, location), Err(HttpError::RedirectRefused(_))),
                "{location:?} was followed"
            );
        }
        // A mirror: its own host and port, and GitHub's raw hosts; not the host at another port, not another host.
        let mirror = https("https://mirror.example:8443/ddi/abc/f");
        assert_eq!(follow(&mirror, "/ddi/abc/g").unwrap(), "https://mirror.example:8443/ddi/abc/g");
        assert!(follow(&mirror, "https://mirror.example:8443/other").is_ok());
        assert!(follow(&mirror, "https://objects.githubusercontent.com/x").is_ok());
        for location in ["https://mirror.example/x", "https://mirror.example:9443/x", "https://other.example/x"] {
            assert!(follow(&mirror, location).is_err(), "{location}");
        }
    }

    // ---- the answer -------------------------------------------------------------------------------

    #[test]
    fn the_status_line_and_headers_are_read_strictly() {
        let head = |text: &str| parse_head(text.as_bytes());
        let ok = head(
            "HTTP/1.1 200 OK\r\ncontent-LENGTH: 12\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes",
        )
        .unwrap();
        assert_eq!(ok, Head { status: 200, content_length: Some(12), chunked: false, location: None });
        let chunked = head("HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked").unwrap();
        assert!(chunked.chunked && chunked.content_length.is_none());
        let redirect = head("HTTP/1.0 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0").unwrap();
        assert_eq!((redirect.status, redirect.location.as_deref()), (302, Some("/elsewhere")));
        assert_eq!(head("HTTP/1.1 404").unwrap().status, 404);
        assert!(head("HTTP/1.1 200 OK\r\nContent-Encoding: identity").is_ok());
        // Refused: not HTTP/1, a status that is not a number, a length that is not one or two that differ, a
        // compression (the file would not be the one that was signed), both ways of saying how long the body is.
        for bad in [
            "",
            "HTTP/2 200",
            "SPDY/3 200 OK",
            "HTTP/1.1",
            "HTTP/1.1 2x0 OK",
            "HTTP/1.1 20",
            "HTTP/1.1 200 OK\r\nContent-Length: abc",
            "HTTP/1.1 200 OK\r\nContent-Length: -1",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2",
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5",
            "HTTP/1.1 200 OK\r\nno colon here",
        ] {
            assert!(head(bad).is_err(), "{bad:?} was taken");
        }
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nX: \xff").is_err());
        // The same length twice is one length.
        assert_eq!(head("HTTP/1.1 200 OK\r\nContent-Length: 7\r\ncontent-length: 7").unwrap().content_length, Some(7));
        // At the most headers there may be, and one more.
        let headers = |n: usize| (0..n).map(|k| format!("\r\nX-{k}: v")).collect::<Vec<_>>().concat();
        assert!(head(&format!("HTTP/1.1 200 OK{}", headers(MAX_HEADERS))).is_ok());
        assert!(head(&format!("HTTP/1.1 200 OK{}", headers(MAX_HEADERS + 1))).is_err());
    }

    fn decode(pieces: &[&[u8]], max: u64) -> Result<Vec<u8>, HttpError> {
        let mut decoder = ChunkDecoder::new();
        let mut out = Vec::new();
        let mut done = false;
        for piece in pieces {
            done = decoder.feed(piece, &mut out, max)?;
        }
        if done { Ok(out) } else { Err(HttpError::Truncated { got: out.len() as u64, want: 0 }) }
    }

    #[test]
    fn a_chunked_body_is_decoded_whatever_the_pieces_it_arrives_in() {
        let body: &[u8] = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\nC\r\n in\r\nchunks.\r\n0\r\nTrailer: x\r\n\r\n";
        let whole = decode(&[body], 100).unwrap();
        assert_eq!(whole, b"Wikipedia in\r\nchunks.");
        // One byte at a time, and split at every point.
        let bytes: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(decode(&bytes, 100).unwrap(), whole);
        for at in 0..body.len() {
            let (a, b) = body.split_at(at);
            assert_eq!(decode(&[a, b], 100).unwrap(), whole, "split at {at}");
        }
        // Nothing but the end, and bytes after the end that are not read.
        assert_eq!(decode(&[b"0\r\n\r\n"], 0).unwrap(), b"");
        assert_eq!(decode(&[b"1\r\nx\r\n0\r\n\r\nGARBAGE"], 1).unwrap(), b"x");
        // Not finished is not done.
        assert!(matches!(decode(&[b"4\r\nWi"], 100), Err(HttpError::Truncated { .. })));
        assert!(matches!(decode(&[b"1\r\nx\r\n0\r\n"], 100), Err(HttpError::Truncated { .. })));
    }

    #[test]
    fn a_chunked_body_that_is_malformed_or_too_large_is_refused() {
        for bad in [
            &b"\r\n"[..],
            b"g\r\nx\r\n0\r\n\r\n",
            b"1\nx\r\n",
            b"1\r\nxy\r\n",
            b"1\r\nx\n0\r\n\r\n",
            b";ext\r\n",
            b"fffffffffffffffff\r\n",
            b"1;aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n",
        ] {
            assert!(matches!(decode(&[bad], 100), Err(HttpError::BadResponse(_))), "{bad:?}");
        }
        // The cap is on what is decoded, and is hit as soon as it is passed.
        assert!(matches!(decode(&[b"5\r\nhello\r\n0\r\n\r\n"], 4), Err(HttpError::TooLarge { limit: 4 })));
        assert!(matches!(decode(&[b"3\r\nabc\r\n3\r\ndef\r\n"], 5), Err(HttpError::TooLarge { limit: 5 })));
        assert_eq!(decode(&[b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n"], 6).unwrap(), b"abcdef");
        // A size that claims a huge chunk is no allocation: the data is what arrives.
        assert!(matches!(decode(&[b"ffffffff\r\nabc"], 100), Err(HttpError::Truncated { .. })));
        // Trailers are bounded.
        let long = format!("0\r\n{}\r\n\r\n", "X: y\r\n".repeat(MAX_TRAILER));
        assert!(matches!(decode(&[long.as_bytes()], 10), Err(HttpError::BadResponse(_))));
    }

    // ---- the exchange, over an in-memory pipe (no network, no clock) ------------------------------

    /// What a scripted server does with one connection: reads the request (all of its head), then writes `answer`, then
    /// closes. Returns the request it read.
    async fn answer_with(answer: Vec<u8>, mut server: tokio::io::DuplexStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if server.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
        }
        server.write_all(&answer).await.unwrap();
        server.shutdown().await.unwrap();
        String::from_utf8(request).unwrap()
    }

    async fn ask(answer: &[u8], max: u64) -> (Result<Reply, HttpError>, String, Vec<u64>) {
        let (mut client, server) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(answer_with(answer.to_vec(), server));
        let url = https("https://example.com/some/file?x=1");
        let limits = Limits { max_bytes: max, ..Limits::for_file(max) };
        let mut seen = Vec::new();
        let reply = exchange(&mut client, &url, &limits, &mut |n| seen.push(n)).await;
        (reply, server.await.unwrap(), seen)
    }

    #[tokio::test]
    async fn the_request_is_a_plain_get_that_asks_for_nothing_unusual() {
        let (reply, request, _) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi", 10).await;
        assert_eq!(reply.unwrap(), Reply::Body(b"hi".to_vec()));
        assert!(request.starts_with("GET /some/file?x=1 HTTP/1.1\r\nHost: example.com\r\n"), "{request}");
        for want in ["Connection: close\r\n", "Accept-Encoding: identity\r\n", "User-Agent: scrios/"] {
            assert!(request.contains(want), "{want} missing from {request}");
        }
        assert!(
            !request.to_ascii_lowercase().contains("authorization") && !request.to_ascii_lowercase().contains("cookie")
        );
    }

    #[tokio::test]
    async fn a_body_is_read_by_its_length_by_chunks_or_to_the_end() {
        let (reply, _, seen) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", 10).await;
        assert_eq!(reply.unwrap(), Reply::Body(b"hello".to_vec()));
        assert_eq!(seen.last(), Some(&5));
        let (reply, _, _) =
            ask(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n", 10).await;
        assert_eq!(reply.unwrap(), Reply::Body(b"hello".to_vec()));
        let (reply, _, _) = ask(b"HTTP/1.0 200 OK\r\n\r\nuntil the end", 100).await;
        assert_eq!(reply.unwrap(), Reply::Body(b"until the end".to_vec()));
        // More bytes than the length are not part of the body.
        let (reply, _, _) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhello", 10).await;
        assert_eq!(reply.unwrap(), Reply::Body(b"he".to_vec()));
        let (reply, _, _) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", 10).await;
        assert_eq!(reply.unwrap(), Reply::Body(Vec::new()));
    }

    #[tokio::test]
    async fn a_body_larger_than_the_cap_is_refused_whatever_it_says_about_itself() {
        // By its length, before a byte of it is read.
        let (reply, _, seen) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world", 10).await;
        assert_eq!(reply.unwrap_err(), HttpError::TooLarge { limit: 10 });
        assert!(seen.is_empty(), "{seen:?}");
        // Chunked, and with no length at all.
        let (reply, _, _) =
            ask(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n", 10)
                .await;
        assert_eq!(reply.unwrap_err(), HttpError::TooLarge { limit: 10 });
        let (reply, _, _) = ask(b"HTTP/1.0 200 OK\r\n\r\nhello world", 10).await;
        assert_eq!(reply.unwrap_err(), HttpError::TooLarge { limit: 10 });
        // At the cap exactly is fine.
        let (reply, _, _) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789", 10).await;
        assert!(reply.is_ok());
    }

    #[tokio::test]
    async fn a_connection_that_ends_early_or_an_answer_that_is_not_one_is_an_error() {
        let (reply, _, _) = ask(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello", 100).await;
        assert_eq!(reply.unwrap_err(), HttpError::Truncated { got: 5, want: 10 });
        let (reply, _, _) = ask(b"HTTP/1.1 200 OK\r\nContent-Le", 100).await;
        assert!(matches!(reply.unwrap_err(), HttpError::BadResponse(why) if why.contains("before the headers")));
        let (reply, _, _) = ask(b"", 100).await;
        assert!(matches!(reply.unwrap_err(), HttpError::BadResponse(_)));
        let (reply, _, _) = ask(b"garbage\r\n\r\n", 100).await;
        assert!(matches!(reply.unwrap_err(), HttpError::BadResponse(_)));
        // Headers that never end.
        let endless = format!("HTTP/1.1 200 OK\r\nX: {}", "a".repeat(2 * MAX_HEAD));
        let (reply, _, _) = ask(endless.as_bytes(), 100).await;
        assert!(matches!(reply.unwrap_err(), HttpError::BadResponse(why) if why.contains("too long")));
        // Statuses that are not 200.
        let (reply, _, _) = ask(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n", 100).await;
        assert_eq!(reply.unwrap_err(), HttpError::Status(404));
        let (reply, _, _) = ask(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\n\r\nx", 100).await;
        assert_eq!(reply.unwrap_err(), HttpError::Status(206));
        let (reply, _, _) = ask(b"HTTP/1.1 302 Found\r\n\r\n", 100).await;
        assert!(matches!(reply.unwrap_err(), HttpError::BadResponse(why) if why.contains("Location")));
        let (reply, _, _) = ask(b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere.example/x\r\n\r\n", 100).await;
        assert_eq!(reply.unwrap(), Reply::Redirect("https://elsewhere.example/x".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_goes_quiet_is_given_up_on_by_the_clock_not_waited_for() {
        // The server reads the request and then says nothing, without closing. The paused clock moves on by itself
        // when nothing else can: the idle limit ends the wait at once, with no real time passing.
        let limits = Limits::for_file(10);
        let url = https("https://example.com/x");
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        let quiet = tokio::spawn(async move {
            let mut buf = [0_u8; 1024];
            let _ = server.read(&mut buf).await;
            std::future::pending::<()>().await;
        });
        let started = tokio::time::Instant::now();
        let reply = exchange(&mut client, &url, &limits, &mut |_| {}).await;
        assert_eq!(reply.unwrap_err(), HttpError::Timeout("waiting for the server"));
        assert_eq!(started.elapsed(), limits.idle);
        quiet.abort();
        // The same in the middle of a body.
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        let slow = tokio::spawn(async move {
            let mut buf = [0_u8; 1024];
            let _ = server.read(&mut buf).await;
            server.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhalf").await.unwrap();
            std::future::pending::<()>().await;
        });
        let reply = exchange(&mut client, &url, &limits, &mut |_| {}).await;
        assert_eq!(reply.unwrap_err(), HttpError::Timeout("waiting for the server"));
        slow.abort();
    }

    // ---- the whole download, against a fake server on the loopback -------------------------------

    /// A fake HTTP server on the loopback (plain http: the tests' own knob). It answers each connection with the next
    /// of the answers it was given (after reading the whole request head), and records the request lines. `finish`
    /// stops it by connecting once more, so that nothing waits on a clock, and returns what was asked.
    struct FakeServer {
        port: u16,
        stop: Arc<AtomicBool>,
        asked: Arc<Mutex<Vec<String>>>,
        thread: std::thread::JoinHandle<()>,
    }

    impl FakeServer {
        fn start(answers: Vec<Vec<u8>>) -> FakeServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let asked = Arc::new(Mutex::new(Vec::new()));
            let (stop_flag, log) = (stop.clone(), asked.clone());
            let thread = std::thread::spawn(move || {
                let mut answers = answers.into_iter();
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    // The whole head of the request before answering: closing with request bytes unread resets the
                    // connection, and the client could see the reset before the answer.
                    let mut request = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                        request.push(byte[0]);
                    }
                    let line =
                        String::from_utf8_lossy(request.split(|b| *b == b'\r').next().unwrap_or_default()).to_string();
                    log.lock().unwrap().push(line);
                    let answer = answers
                        .next()
                        .unwrap_or_else(|| b"HTTP/1.1 500 Out Of Answers\r\nContent-Length: 0\r\n\r\n".to_vec());
                    let _ = stream.write_all(&answer);
                    let _ = stream.flush();
                    // Our side is done; the client closes its own (nothing unread here, so no reset).
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    let _ = stream.read_to_end(&mut Vec::new());
                }
            });
            FakeServer { port, stop, asked, thread }
        }

        fn url(&self, path: &str) -> Url {
            Url::parse(&format!("http://127.0.0.1:{}{path}", self.port)).unwrap()
        }

        fn finish(self) -> Vec<String> {
            self.stop.store(true, Ordering::SeqCst);
            // Wakes the accept that waits, if it does.
            drop(std::net::TcpStream::connect(("127.0.0.1", self.port)));
            self.thread.join().unwrap();
            self.asked.lock().unwrap().clone()
        }
    }

    fn ok_answer(body: &[u8]) -> Vec<u8> {
        let mut answer =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        answer.extend_from_slice(body);
        answer
    }

    #[tokio::test]
    async fn a_download_follows_a_redirect_on_its_own_host_and_returns_the_body() {
        let body = vec![7_u8; 300_000];
        let server = FakeServer::start(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /moved/file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ok_answer(&body),
        ]);
        let mut seen = Vec::new();
        let got = get(&server.url("/file"), &Limits::for_file(1_000_000), &mut |n| seen.push(n)).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(seen.last(), Some(&300_000));
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(server.finish(), vec!["GET /file HTTP/1.1".to_string(), "GET /moved/file HTTP/1.1".to_string()]);
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_is_not_followed_and_the_other_host_is_never_asked() {
        let server = FakeServer::start(vec![
            // 127.0.0.2 is not this host, whatever it is: the redirect is refused before anything connects to it.
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ]);
        let reply = get(&server.url("/file"), &Limits::for_file(10), &mut |_| {}).await;
        assert!(matches!(&reply, Err(HttpError::RedirectRefused(why)) if why.contains("127.0.0.2")), "{reply:?}");
        assert_eq!(server.finish().len(), 1);
        // A loop of redirects ends at the limit.
        let hop = b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let server = FakeServer::start(vec![hop; usize::from(MAX_REDIRECTS) + 1]);
        let reply = get(&server.url("/start"), &Limits::for_file(10), &mut |_| {}).await;
        assert_eq!(reply.unwrap_err(), HttpError::TooManyRedirects);
        assert_eq!(server.finish().len(), usize::from(MAX_REDIRECTS) + 1);
    }

    #[tokio::test]
    async fn a_download_that_is_too_large_or_missing_or_cut_off_is_an_error() {
        let server = FakeServer::start(vec![
            ok_answer(&[0; 100]),
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nonly a little".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n".to_vec(),
        ]);
        let limits = Limits::for_file(50);
        assert_eq!(get(&server.url("/a"), &limits, &mut |_| {}).await.unwrap_err(), HttpError::TooLarge { limit: 50 });
        assert_eq!(get(&server.url("/b"), &limits, &mut |_| {}).await.unwrap_err(), HttpError::Status(404));
        let cut = get(&server.url("/c"), &Limits::for_file(500), &mut |_| {}).await.unwrap_err();
        assert_eq!(cut, HttpError::Truncated { got: 13, want: 100 });
        assert_eq!(get(&server.url("/d"), &limits, &mut |_| {}).await.unwrap(), b"abc");
        assert_eq!(server.finish().len(), 4);
    }

    #[tokio::test]
    async fn an_address_that_nobody_listens_on_is_a_connection_error() {
        // Port 1 is not used by anything on a build machine, and is never bound by these tests.
        let reply = get(&Url::parse("http://127.0.0.1:1/x").unwrap(), &Limits::for_file(10), &mut |_| {}).await;
        assert!(matches!(reply, Err(HttpError::Connect(_))), "{reply:?}");
    }

    #[test]
    fn the_tls_settings_build_with_the_mozilla_roots_and_the_hosts() {
        let config = tls_config().unwrap();
        assert!(config.alpn_protocols.is_empty());
        let built_in = webpki_roots::TLS_SERVER_ROOTS.len();
        assert!(built_in > 100);
        // The host's roots are added to the built-in ones, never instead of them.
        assert!(root_store().len() >= built_in);
        // What the host lists that is not a certificate is left out, and is not an error.
        let junk = rustls::pki_types::CertificateDer::from(vec![0x30, 0x03, 0x01, 0x01, 0xff]);
        assert_eq!(roots_with(vec![junk]).len(), built_in);
    }

    #[test]
    fn an_unknown_issuer_says_how_to_make_a_proxys_ca_known() {
        let unknown = std::io::Error::other("invalid peer certificate: UnknownIssuer");
        let HttpError::Connect(why) = tls_error(&unknown) else { panic!("not a connection error") };
        assert!(why.contains("SSL_CERT_FILE") && why.contains("trust store"), "{why}");
        let HttpError::Connect(other) = tls_error(&std::io::Error::other("handshake failure")) else { panic!() };
        assert_eq!(other, "TLS: handshake failure");
    }
}
