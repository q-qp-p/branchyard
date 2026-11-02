//! A minimal blocking HTTP/1.1 client: heads, framing, chunked decoding and
//! URLs from `branchyard-wire` (the workspace's one codec), optional TLS
//! through rustls, over TCP or, for `unix:/path` (a server's `--listen-unix`
//! socket, such as the one `by --remote ssh://` forwards), a Unix domain
//! socket.
//!
//! Connections are kept alive. A [`Pool`] of idle connections to one
//! endpoint is shared by a `Client` and its clones, and a connection goes
//! back into it only once a response body was read to its framed end on a
//! connection the response leaves open (see `Reply::read_body`). A streamed
//! body, a `101`, and the one-shot [`send`] close their connection with
//! their response.
//! Just enough for the Branchyard API; not a general HTTP client.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use branchyard_support::time::now_ms;
use branchyard_support::LockExt;
use branchyard_wire as wire;
use branchyard_wire::{Framing, WireError};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Largest response head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// Idle connections kept for one endpoint.
const MAX_IDLE: usize = 4;
/// How long an idle connection is kept before it is not reused.
const MAX_IDLE_MS: u64 = 5_000;

/// Where the server is: `http://host:port/prefix`, `https://...`, or
/// `unix:/absolute/path/to/socket` (plain HTTP over a Unix domain socket,
/// with `localhost` as the host and no prefix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path prefix without a trailing slash, such as `""` or `/branchyard`.
    pub prefix: String,
    /// The Unix domain socket to connect to instead of `host:port`.
    pub unix: Option<PathBuf>,
}

impl Endpoint {
    pub fn parse(url: &str) -> Result<Endpoint, String> {
        if let Some(path) = url.strip_prefix("unix:") {
            let path = Path::new(path);
            if !path.is_absolute() || path.as_os_str().to_string_lossy().contains(['?', '#']) {
                return Err(format!(
                    "{url:?} must name a socket by its absolute path, as unix:/path/to/socket"
                ));
            }
            return Ok(Endpoint {
                tls: false,
                host: "localhost".into(),
                port: 80,
                prefix: String::new(),
                unix: Some(path.to_path_buf()),
            });
        }
        let base = branchyard::models::BaseUrl::parse(url)?;
        Ok(Endpoint {
            tls: base.tls,
            host: base.host,
            port: base.port,
            prefix: base.prefix,
            unix: None,
        })
    }

    fn host_header(&self) -> String {
        wire::host_header(self.tls, &self.host, self.port)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.unix {
            return write!(f, "unix:{}", path.display());
        }
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.host_header(), self.prefix)
    }
}

/// TLS settings: the Mozilla roots from `webpki-roots`, plus the
/// certificates in `ca_file` when given (for a private CA or a self-signed
/// server certificate).
pub fn tls_config(ca_file: Option<&Path>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_file {
        let certs = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("CA file {}: {e}", path.display()))?;
        let mut added = 0;
        for cert in certs {
            let cert = cert.map_err(|e| format!("CA file {}: {e}", path.display()))?;
            roots
                .add(cert)
                .map_err(|e| format!("CA file {}: {e}", path.display()))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!("CA file {} has no certificates", path.display()));
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
        }
    }
}

impl Stream {
    /// How long a read may wait, set again for a connection being reused.
    fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.set_read_timeout(Some(timeout)),
            Stream::Tls(s) => s.sock.set_read_timeout(Some(timeout)),
            #[cfg(unix)]
            Stream::Unix(s) => s.set_read_timeout(Some(timeout)),
        }
    }

    /// Whether the connection is still open with nothing waiting on it. A
    /// peer that closed it reads as end of file; bytes nobody asked for mean
    /// it is out of step with the requests, so it is not reused either.
    fn idle_and_open(&mut self) -> bool {
        match self {
            Stream::Plain(s) => socket_idle_and_open(s, TcpStream::set_nonblocking),
            Stream::Tls(s) => socket_idle_and_open(&mut s.sock, TcpStream::set_nonblocking),
            #[cfg(unix)]
            Stream::Unix(s) => socket_idle_and_open(s, UnixStream::set_nonblocking),
        }
    }
}

/// One non-blocking read of the socket itself, below TLS: a read that would
/// block means open and quiet. Any byte or end of file means not reusable.
fn socket_idle_and_open<S: Read>(
    socket: &mut S,
    nonblocking: fn(&S, bool) -> io::Result<()>,
) -> bool {
    if nonblocking(socket, true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let quiet = matches!(socket.read(&mut byte), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
    let restored = nonblocking(socket, false).is_ok();
    quiet && restored
}

/// A connection to the server, buffered for reading its responses.
pub(crate) struct Conn {
    reader: BufReader<Stream>,
}

impl Conn {
    /// Wrap a freshly connected stream.
    pub(crate) fn new(stream: Stream) -> Conn {
        Conn {
            reader: BufReader::new(stream),
        }
    }
}

/// The idle connections to one endpoint, shared by a `Client` and its
/// clones. At most [`MAX_IDLE`] are kept, each for at most [`MAX_IDLE_MS`].
#[derive(Default)]
pub(crate) struct Pool {
    idle: Mutex<Vec<Idle>>,
}

struct Idle {
    conn: Conn,
    /// When it went idle, in milliseconds since the epoch.
    since_ms: u64,
}

impl Pool {
    /// An idle connection that is still open and not too old, its reads
    /// allowed `timeout`; `None` when there is none.
    pub(crate) fn checkout(&self, timeout: Duration, now: u64) -> Option<Conn> {
        loop {
            let idle = self.idle.lock_recovering("http connection pool").pop()?;
            if now.saturating_sub(idle.since_ms) > MAX_IDLE_MS {
                continue;
            }
            let mut conn = idle.conn;
            if conn.reader.get_mut().idle_and_open()
                && conn.reader.get_mut().set_read_timeout(timeout).is_ok()
            {
                return Some(conn);
            }
        }
    }

    /// Keep a connection whose response was read to its end. One with
    /// bytes buffered past that response is out of step, so it is dropped.
    pub(crate) fn checkin(&self, conn: Conn, now: u64) {
        if !conn.reader.buffer().is_empty() {
            return;
        }
        let mut idle = self.idle.lock_recovering("http connection pool");
        if idle.len() < MAX_IDLE {
            idle.push(Idle {
                conn,
                since_ms: now,
            });
        }
    }
}

/// Open a connection, completing the TLS handshake for `https`.
pub fn connect(
    endpoint: &Endpoint,
    tls: Option<&Arc<ClientConfig>>,
    read_timeout: Duration,
) -> io::Result<Stream> {
    if let Some(path) = &endpoint.unix {
        #[cfg(unix)]
        {
            let stream = UnixStream::connect(path)?;
            stream.set_read_timeout(Some(read_timeout))?;
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
            return Ok(Stream::Unix(stream));
        }
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{} needs Unix domain sockets", path.display()),
        ));
    }
    let addrs: Vec<_> = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()?
        .collect();
    let mut last = io::Error::new(io::ErrorKind::NotFound, "the host has no address");
    let mut tcp = None;
    // A caller that gives a request less than that does not wait longer
    // to connect (`by hook`'s requests).
    let connect_wait = read_timeout.min(Duration::from_secs(10));
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, connect_wait) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(e) => last = e,
        }
    }
    let tcp = tcp.ok_or(last)?;
    tcp.set_read_timeout(Some(read_timeout))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_nodelay(true)?;
    if !endpoint.tls {
        return Ok(Stream::Plain(tcp));
    }
    let config = tls.ok_or_else(|| io::Error::other("no TLS configuration"))?;
    let name = ServerName::try_from(endpoint.host.clone())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let connection =
        ClientConnection::new(config.clone(), name).map_err(|e| io::Error::other(e.to_string()))?;
    let mut stream = StreamOwned::new(connection, tcp);
    // Handshake now, so certificate errors surface as connection errors.
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(Stream::Tls(Box::new(stream)))
}

pub struct Request<'a> {
    pub method: &'a str,
    /// Absolute path with query, including the endpoint's prefix.
    pub target: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub body: Option<&'a [u8]>,
}

/// A response: its status and headers, and its body as a stream.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The whole body, up to `limit` bytes.
    pub fn read_body(mut self, limit: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut self.body)
            .take(limit as u64 + 1)
            .read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response body larger than {limit} bytes"),
            ));
        }
        Ok(out)
    }
}

/// A request that failed, and whether it may be sent again on a fresh
/// connection: nothing of the response had arrived, and the connection was
/// dropped (closed, reset or broken), so the server cannot have answered.
#[derive(Debug)]
pub(crate) struct SendError {
    pub(crate) error: io::Error,
    pub(crate) before_response: bool,
}

impl SendError {
    fn fatal(error: impl Into<io::Error>) -> SendError {
        SendError {
            error: error.into(),
            before_response: false,
        }
    }
}

/// The errors a peer that dropped its end of an idle connection produces.
fn dropped(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

/// A response whose body is still to be read, on the connection it came on.
/// The client reads it with [`Reply::read_body`], which returns the
/// connection to its pool once the body is read to its end, when the
/// response leaves the connection open.
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    conn: Conn,
    framing: Framing,
    /// Set only when the response may keep its connection open.
    pool: Option<Arc<Pool>>,
}

impl Reply {
    /// The whole body, up to `limit` bytes. A body read to its framed end
    /// returns its connection to the pool; a body that fails, or is refused
    /// for its size, drops the connection.
    pub(crate) fn read_body(self, limit: usize) -> io::Result<Vec<u8>> {
        let Reply {
            mut conn,
            framing,
            pool,
            ..
        } = self;
        let body =
            wire::read_body(&mut conn.reader, framing, limit).map_err(|error| match error {
                WireError::BodyTooLarge { .. } => io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("response body larger than {limit} bytes"),
                ),
                other => other.into(),
            })?;
        if let Some(pool) = pool {
            pool.checkin(conn, now_ms());
        }
        Ok(body)
    }

    /// The body as a stream, read as it arrives. Its connection closes with
    /// the stream and is never pooled, so the request was sent without
    /// keep-alive.
    pub(crate) fn into_stream(self) -> Body {
        wire::Body::new(self.conn.reader, self.framing)
    }
}

/// Send one request on a new connection that closes with the response, and
/// read the head. The body is read as a stream.
pub fn send(stream: Stream, endpoint: &Endpoint, request: &Request<'_>) -> io::Result<Response> {
    let reply =
        send_on(Conn::new(stream), endpoint, request, None).map_err(|failed| failed.error)?;
    let Reply {
        status,
        headers,
        conn,
        framing,
        ..
    } = reply;
    Ok(Response {
        status,
        headers,
        body: wire::Body::new(conn.reader, framing),
    })
}

/// Send one request on `conn` and read the response head. With `pool`, the
/// connection stays open for later requests (the request has no
/// `Connection: close`), and [`Reply::read_body`] returns it to the pool
/// when the response allows. Without it, the connection closes after the
/// response.
pub(crate) fn send_on(
    mut conn: Conn,
    endpoint: &Endpoint,
    request: &Request<'_>,
    pool: Option<Arc<Pool>>,
) -> Result<Reply, SendError> {
    let host = endpoint.host_header();
    let agent = concat!("branchyard-client/", env!("CARGO_PKG_VERSION"));
    let mut fixed: Vec<(&str, &str)> = vec![("Host", host.as_str()), ("User-Agent", agent)];
    if pool.is_none() {
        fixed.push(("Connection", "close"));
    }
    let head = wire::request_head(
        request.method,
        request.target,
        fixed
            .into_iter()
            .chain(request.headers.iter().map(|(n, v)| (*n, v.as_str()))),
        request.body.map(|body| body.len() as u64),
    )
    .map_err(SendError::fatal)?;
    let mut bytes = head;
    if let Some(body) = request.body {
        bytes.extend_from_slice(body);
    }
    let stream = conn.reader.get_mut();
    stream
        .write_all(&bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| SendError {
            before_response: dropped(error.kind()),
            error,
        })?;
    // Wait for the first byte. The end of the connection before it is a
    // request that got no answer; an error here is the same as a write's.
    match conn.reader.fill_buf() {
        Ok([]) => {
            return Err(SendError {
                error: WireError::ConnectionClosed.into(),
                before_response: true,
            })
        }
        Ok(_) => {}
        Err(error) => {
            return Err(SendError {
                before_response: dropped(error.kind()),
                error,
            })
        }
    }
    read_response(conn, request.method == "HEAD", pool)
}

/// Read the head of a response on `conn`. The connection is kept only when
/// the response leaves it open: HTTP/1.1, no `Connection: close`, not a
/// `101` (which switches protocols), and a body framed by length or chunks.
fn read_response(
    mut conn: Conn,
    head_only: bool,
    pool: Option<Arc<Pool>>,
) -> Result<Reply, SendError> {
    let head = wire::read_response_head(&mut conn.reader, MAX_HEAD).map_err(SendError::fatal)?;
    let framing =
        wire::response_framing(head.status, &head.headers, head_only).map_err(SendError::fatal)?;
    let reusable = head.minor == 1
        && head.status != 101
        && framing != Framing::UntilClose
        && !wants_close(&head.headers);
    Ok(Reply {
        status: head.status,
        headers: head.headers,
        conn,
        framing,
        pool: pool.filter(|_| reusable),
    })
}

/// Whether a response's `Connection` fields name the `close` option.
fn wants_close(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("connection"))
        .any(|(_, v)| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
}

/// A response body as a stream, its framing undone by the wire codec.
pub type Body = wire::Body<BufReader<Stream>>;

/// Everything but `A-Z a-z 0-9 - . _ ~`.
const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode one path segment or query value.
pub fn encode(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, COMPONENT).to_string()
}

/// Undo [`encode`]: `%XX` escapes become bytes (a trailing `%41` too), a
/// malformed escape (`%zz`, a lone `%4`) stays as written, and bytes that
/// are not UTF-8 become U+FFFD. `+` is not a space; see [`decode_form`].
pub fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// [`decode`] for an `application/x-www-form-urlencoded` query value, where
/// a literal `+` is a space (an encoded one, `%2B`, stays a plus).
pub fn decode_form(text: &str) -> String {
    decode(&text.replace('+', " "))
}

/// [`decode_form`] on bytes, which need not be UTF-8 (a webhook's form body
/// carries a JSON payload as bytes).
pub fn decode_form_bytes(value: &[u8]) -> Vec<u8> {
    let spaced: Vec<u8> = value
        .iter()
        .map(|b| if *b == b'+' { b' ' } else { *b })
        .collect();
    percent_encoding::percent_decode(&spaced).collect()
}

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse() {
        let e = Endpoint::parse("http://127.0.0.1:8421").unwrap();
        assert_eq!(
            (e.tls, e.host.as_str(), e.port, e.prefix.as_str()),
            (false, "127.0.0.1", 8421, "")
        );
        let e = Endpoint::parse("https://by.example/api/").unwrap();
        assert_eq!((e.tls, e.port, e.prefix.as_str()), (true, 443, "/api"));
        assert_eq!(e.to_string(), "https://by.example/api");
        let e = Endpoint::parse("unix:/run/by/by.sock").unwrap();
        assert_eq!(e.unix.as_deref(), Some(Path::new("/run/by/by.sock")));
        assert_eq!((e.tls, e.host_header().as_str()), (false, "localhost"));
        assert_eq!(e.to_string(), "unix:/run/by/by.sock");
        for bad in ["unix:", "unix:relative/by.sock", "unix:/a?b"] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
        let e = Endpoint::parse("http://[::1]:9000/").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("::1", 9000));
        assert_eq!(e.host_header(), "[::1]:9000");
        for bad in [
            "ftp://x",
            "http://",
            "http://h:port",
            "http://u@h",
            "http://h/?q",
            "http://[::1",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn encoding_keeps_unreserved_bytes() {
        assert_eq!(encode("fix-it_1.2~"), "fix-it_1.2~");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }

    /// The decoding cases every percent-decoding call site shares: the
    /// server's query values, OTLP headers and storage routes all go
    /// through `decode`/`decode_form`.
    #[test]
    fn decoding_handles_the_edges() {
        // A trailing escape decodes (the hand-rolled decoders it replaced
        // left `%41` at the end of input as written).
        assert_eq!(decode("%41"), "A");
        assert_eq!(decode("a%41"), "aA");
        // Malformed escapes stay as written.
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%4"), "%4");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%%41"), "%A");
        // Multi-byte UTF-8, whole and cut.
        assert_eq!(decode("%C3%A9"), "\u{e9}");
        assert_eq!(decode("%E2%82%AC"), "\u{20ac}");
        assert_eq!(decode("%C3"), "\u{fffd}");
        // `+` is a space only in a form value.
        assert_eq!(decode("a+b"), "a+b");
        assert_eq!(decode_form("a+b%2Bc%20d"), "a b+c d");
        assert_eq!(decode_form("%41"), "A");
        assert_eq!(decode_form_bytes(b"a+b%2Bc%FF%41"), b"a b+c\xffA");
    }

    #[test]
    fn encoding_round_trips() {
        for text in [
            "",
            "plain",
            "a b/c?d=e&f",
            "100%",
            "%41",
            "caf\u{e9} \u{20ac} \u{1f600}",
            "+ plus",
            "~-._",
        ] {
            assert_eq!(decode(&encode(text)), text, "{text:?}");
            assert_eq!(decode_form(&encode(text)), text, "{text:?}");
        }
    }
    #[test]
    fn an_idle_connection_expires_after_its_limit() {
        let server =
            branchyard_testkit::MockHttp::start(|_| branchyard_testkit::Response::new(200, ""));
        let tcp = TcpStream::connect(server.addr()).unwrap();
        let pool = Pool::default();
        let timeout = Duration::from_secs(10);
        pool.checkin(Conn::new(Stream::Plain(tcp)), 1_000);
        assert!(
            pool.checkout(timeout, 1_000 + MAX_IDLE_MS + 1).is_none(),
            "older than the limit"
        );
        let tcp = TcpStream::connect(server.addr()).unwrap();
        pool.checkin(Conn::new(Stream::Plain(tcp)), 1_000);
        assert!(pool.checkout(timeout, 1_000 + MAX_IDLE_MS).is_some());
    }

    #[test]
    fn connection_close_is_read_by_token() {
        let field = |v: &str| vec![("Connection".to_owned(), v.to_owned())];
        assert!(wants_close(&field("close")));
        assert!(wants_close(&field("Keep-Alive, Close")));
        assert!(!wants_close(&field("keep-alive")));
        assert!(!wants_close(&field("closed")));
        assert!(!wants_close(&[]));
    }

    /// Only a response that keeps its connection open is handed a pool: an
    /// HTTP/1.0 answer, one that says `Connection: close`, and a `101`
    /// (which switches protocols) are not.
    #[cfg(unix)]
    #[test]
    fn only_a_response_that_keeps_its_connection_is_pooled() {
        use std::os::unix::net::UnixStream;
        let cases: [(&[u8], bool); 4] = [
            (b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", true),
            (b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n", false),
            (
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                false,
            ),
            (
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: x\r\n\r\n",
                false,
            ),
        ];
        for (bytes, pooled) in cases {
            let (mut server, client) = UnixStream::pair().unwrap();
            server.write_all(bytes).unwrap();
            let pool = Arc::new(Pool::default());
            let reply = read_response(Conn::new(Stream::Unix(client)), false, Some(pool))
                .unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(
                reply.pool.is_some(),
                pooled,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// A server that sends `bytes` and closes, read through `read_response`.
    fn answer(bytes: &[u8], head_only: bool) -> io::Result<(u16, Vec<u8>)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (client, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        server.write_all(bytes).unwrap();
        drop(server);
        let reply = read_response(Conn::new(Stream::Plain(client)), head_only, None)
            .map_err(|e| e.error)?;
        let status = reply.status;
        Ok((status, reply.read_body(1 << 20)?))
    }

    #[test]
    fn every_valid_response_in_the_wire_corpus_is_read() {
        for (name, bytes, body) in wire::corpus::responses_valid() {
            let got = answer(&bytes, false).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(got.1, body, "{name}");
        }
    }

    #[test]
    fn every_malformed_response_in_the_wire_corpus_fails_cleanly() {
        for (name, bytes) in wire::corpus::responses_malformed() {
            let got = answer(&bytes, false);
            assert!(got.is_err(), "{name}: read as {got:?}");
        }
    }

    #[test]
    fn a_framing_failure_carries_its_typed_cause() {
        let error = answer(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(
            wire::wire_error(&error),
            Some(wire::WireError::BadChunkSize(_))
        ));
        let error = answer(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc", false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_request_head_refuses_what_could_split_it() {
        let headers = [("X-Evil", "a\r\nInjected: 1".to_owned())];
        let request = Request {
            method: "GET",
            target: "/",
            headers: &headers,
            body: None,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        let Err(error) = send(Stream::Plain(tcp), &endpoint, &request) else {
            panic!("a header with a line break was sent");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
