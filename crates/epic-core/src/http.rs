//! Blocking HTTP client.
//!
//! Two backends implement the same contract:
//!
//! * **WinHTTP** (Windows, production): TLS, connection pooling and HTTP/2 stay inside the OS, so
//!   no TLS crate is needed. One session per worker thread keeps connections alive across chunks.
//! * **plain TCP** (all platforms, tests): HTTP/1.1 only, used for loopback integration tests and
//!   as a fallback when WinHTTP is unavailable.
//!
//! The contract is deliberately narrow: [`Client::request`] streams the response body into a
//! caller supplied sink, which is exactly what both control-plane JSON calls and 1 MiB chunk
//! downloads need.

use crate::error::{Error, Result};
use std::io::Write;

/// Method of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// A request to send.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Per-request receive timeout.
    pub timeout_secs: u64,
    /// Abort flag polled while reading the body.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Request {
    /// GET request with headers and a default timeout.
    pub fn get(url: impl Into<String>) -> Request {
        Request {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_secs: 60,
            cancel: None,
        }
    }

    /// Form-encoded POST.
    pub fn post_form(url: impl Into<String>, form: &[(&str, &str)]) -> Request {
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        Request {
            method: Method::Post,
            url: url.into(),
            headers: vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: Some(body.into_bytes()),
            timeout_secs: 60,
            cancel: None,
        }
    }

    /// Add a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Request {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the receive timeout.
    pub fn timeout(mut self, secs: u64) -> Request {
        self.timeout_secs = secs;
        self
    }

    /// Attach a cancellation flag.
    pub fn cancellable(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Request {
        self.cancel = Some(flag);
        self
    }

    /// HTTP Basic authentication header.
    pub fn basic_auth(mut self, user: &str, password: &str) -> Request {
        let raw = crate::cdn::base64_url(&format!("{user}:{password}").into_bytes())
            .replace('-', "+")
            .replace('_', "/");
        // RFC 7617 allows omitted padding but some servers reject it, so pad explicitly.
        let pad = (4 - raw.len() % 4) % 4;
        let token = format!("{raw}{}", "=".repeat(pad));
        self.headers
            .push(("Authorization".to_string(), format!("Basic {token}")));
        self
    }
}

/// Percent-encode a form component.
pub fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Status plus headers of a completed response.
#[derive(Debug, Clone)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// True for 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Backend selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Windows WinHTTP (TLS capable).
    WinHttp,
    /// Plain HTTP/1.1 over TCP, loopback testing only.
    PlainTcp,
}

/// A client bound to one backend.
#[derive(Debug, Clone)]
pub struct Client {
    backend: Backend,
    agent: String,
}

impl Default for Client {
    fn default() -> Client {
        Client::new()
    }
}

impl Client {
    /// Production client: WinHTTP on Windows, plain TCP elsewhere.
    pub fn new() -> Client {
        let backend = if cfg!(windows) && std::env::var_os("EPICD_PLAIN_HTTP").is_none() {
            Backend::WinHttp
        } else {
            Backend::PlainTcp
        };
        Client { backend, agent: format!("epicd/{}", crate::VERSION) }
    }

    /// Force the plain TCP backend (tests, mirrors).
    pub fn plain() -> Client {
        Client { backend: Backend::PlainTcp, agent: format!("epicd/{}", crate::VERSION) }
    }

    /// Which backend is in use.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Send a request, streaming the body into `sink`, and return the response head.
    pub fn request<W: Write>(&self, req: &Request, sink: &mut W) -> Result<ResponseHead> {
        let result = self.request_inner(req, sink);
        result.map_err(|e| match e {
            Error::Net(msg) => Error::Net(format!("{msg} [{req_url}]", req_url = req.url)),
            other => other,
        })
    }

    fn request_inner<W: Write>(&self, req: &Request, sink: &mut W) -> Result<ResponseHead> {
        match self.backend {
            Backend::WinHttp => {
                #[cfg(windows)]
                {
                    winhttp::execute(&self.agent, req, sink)
                }
                #[cfg(not(windows))]
                {
                    Err(Error::net("WinHTTP backend is only available on Windows"))
                }
            }
            Backend::PlainTcp => plain::execute(&self.agent, req, sink),
        }
    }

    /// Send a request and collect the whole body.
    pub fn request_vec(&self, req: &Request) -> Result<(ResponseHead, Vec<u8>)> {
        let mut buf = Vec::new();
        let head = self.request(req, &mut buf)?;
        Ok((head, buf))
    }

    /// GET a JSON document, mapping non-2xx replies to [`Error::Api`].
    pub fn get_json(&self, url: &str, bearer: Option<&str>) -> Result<serde_json::Value> {
        let mut req = Request::get(url).timeout(120);
        if let Some(token) = bearer {
            req = req.header("Authorization", token);
        }
        self.json_checked(req)
    }

    /// Send a request and parse JSON, mapping errors like the API layer does.
    pub fn json_checked(&self, req: Request) -> Result<serde_json::Value> {
        let (head, body) = self.request_vec(&req)?;
        let text = String::from_utf8_lossy(&body).to_string();
        if !head.is_success() {
            let (code, message) = crate::net::extract_error(&text);
            return Err(Error::Api { status: head.status, code, message });
        }
        serde_json::from_str(&text)
            .map_err(|e| Error::net(format!("invalid JSON from server: {e}")))
    }
}

/// Split an absolute URL into `(scheme, host, port, path_and_query)`.
pub fn split_url(url: &str) -> Result<(&str, String, u16, String)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::net(format!("URL without scheme: {url}")))?;
    let scheme = match scheme {
        "http" => "http",
        "https" => "https",
        other => return Err(Error::net(format!("unsupported URL scheme '{other}'"))),
    };
    let mut host_port = rest;
    let mut path = "/".to_string();
    if let Some(idx) = rest.find('/') {
        host_port = &rest[..idx];
        path = rest[idx..].to_string();
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u16>().unwrap_or(default_port))
        }
        _ => (host_port.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(Error::net(format!("URL without host: {url}")));
    }
    Ok((scheme, host, port, path))
}

fn cancelled(req: &Request) -> bool {
    req.cancel
        .as_ref()
        .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// plain HTTP/1.1 backend
// ---------------------------------------------------------------------------------------------

mod plain {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    /// Longest time-to-first-byte a cancellable request waits for.
    const HEADER_WAIT_SECS: u64 = 10;
    /// Body poll cadence once the headers are in.
    const BODY_POLL: Duration = Duration::from_secs(1);

    fn is_timeout(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    }

    /// Read up to `buf.len()` bytes, polling cancellation and retrying read timeouts.
    ///
    /// `idle` is pushed forward on every successful read, so a slow-but-alive transfer is fine while
    /// a stalled one fails after the request timeout instead of blocking a cancel for minutes.
    fn read_body<R: Read>(
        reader: &mut R,
        buf: &mut [u8],
        req: &Request,
        idle: &mut Instant,
        window: Duration,
    ) -> Result<usize> {
        loop {
            if cancelled(req) {
                return Err(Error::net("request cancelled"));
            }
            match reader.read(buf) {
                Ok(n) => {
                    *idle = Instant::now() + window;
                    return Ok(n);
                }
                Err(e) if is_timeout(&e) => {
                    if req.cancel.is_none() || Instant::now() >= *idle {
                        return Err(Error::net(format!("read timed out: {e}")));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(Error::net(format!("read body failed: {e}"))),
            }
        }
    }

    /// Same contract as [`read_body`], for a line-oriented read (chunked encoding framing).
    fn read_line_body<R: BufRead>(
        reader: &mut R,
        line: &mut String,
        req: &Request,
        idle: &mut Instant,
        window: Duration,
    ) -> Result<usize> {
        loop {
            if cancelled(req) {
                return Err(Error::net("request cancelled"));
            }
            match reader.read_line(line) {
                Ok(n) => {
                    *idle = Instant::now() + window;
                    return Ok(n);
                }
                Err(e) if is_timeout(&e) => {
                    if req.cancel.is_none() || Instant::now() >= *idle {
                        return Err(Error::net(format!("read timed out: {e}")));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(Error::net(format!("read body failed: {e}"))),
            }
        }
    }

    pub fn execute<W: Write>(agent: &str, req: &Request, sink: &mut W) -> Result<ResponseHead> {
        let (scheme, host, port, path) = split_url(&req.url)?;
        if scheme != "http" {
            return Err(Error::net(
                "the plain backend only supports http:// URLs; use the WinHTTP backend for https",
            ));
        }
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr)
            .map_err(|e| Error::net(format!("connect to {addr} failed: {e}")))?;
        // A cancellable request must not wait the full request timeout for response headers.
        let header_secs = if req.cancel.is_some() {
            req.timeout_secs.min(HEADER_WAIT_SECS)
        } else {
            req.timeout_secs
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(header_secs.max(1))))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .ok();

        let mut head = format!("{} {path} HTTP/1.1\r\n", req.method.as_str());
        head.push_str(&format!("Host: {host}:{port}\r\n"));
        head.push_str(&format!("User-Agent: {agent}\r\n"));
        head.push_str("Accept: */*\r\n");
        head.push_str("Connection: keep-alive\r\n");
        let mut has_len = false;
        for (name, value) in &req.headers {
            if name.eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(body) = &req.body {
            if !has_len {
                head.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
        }
        head.push_str("\r\n");

        let mut stream = stream;
        let mut buf = head.into_bytes();
        if let Some(body) = &req.body {
            buf.extend_from_slice(body);
        }
        stream
            .write_all(&buf)
            .map_err(|e| Error::net(format!("write failed: {e}")))?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| Error::net(format!("read status failed: {e}")))?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::net(format!("malformed status line: {status_line:?}")))?;

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(|e| Error::net(format!("read header failed: {e}")))?;
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let head = ResponseHead { status, headers };

        let chunked = head
            .header("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        let content_length = head
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok());

        if req.cancel.is_some() {
            // Poll the cancel flag once a second while the body arrives.
            reader.get_ref().set_read_timeout(Some(BODY_POLL)).ok();
        }
        let window = Duration::from_secs(req.timeout_secs.max(1));
        let mut idle = Instant::now() + window;

        if chunked {
            loop {
                let mut size_line = String::new();
                if read_line_body(&mut reader, &mut size_line, req, &mut idle, window)? == 0 {
                    break;
                }
                let size =
                    usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                        .unwrap_or(0);
                if size == 0 {
                    let mut trailer = String::new();
                    let _ = read_line_body(&mut reader, &mut trailer, req, &mut idle, window);
                    break;
                }
                let mut chunk = vec![0u8; size];
                let mut filled = 0;
                while filled < size {
                    let n =
                        read_body(&mut reader, &mut chunk[filled..], req, &mut idle, window)?;
                    if n == 0 {
                        return Err(Error::net("connection closed in the middle of a chunk"));
                    }
                    filled += n;
                }
                sink.write_all(&chunk)
                    .map_err(|e| Error::net(format!("write body failed: {e}")))?;
                let mut crlf = [0u8; 2];
                let mut got = 0;
                while got < 2 {
                    let n = read_body(&mut reader, &mut crlf[got..], req, &mut idle, window)?;
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
            }
        } else if let Some(len) = content_length {
            let mut remaining = len;
            let mut buf = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let want = remaining.min(buf.len() as u64) as usize;
                let n = read_body(&mut reader, &mut buf[..want], req, &mut idle, window)?;
                if n == 0 {
                    break;
                }
                sink.write_all(&buf[..n])
                    .map_err(|e| Error::net(format!("write body failed: {e}")))?;
                remaining -= n as u64;
            }
        } else {
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = read_body(&mut reader, &mut buf, req, &mut idle, window)?;
                if n == 0 {
                    break;
                }
                sink.write_all(&buf[..n])
                    .map_err(|e| Error::net(format!("write body failed: {e}")))?;
            }
        }

        Ok(head)
    }
}

// ---------------------------------------------------------------------------------------------
// WinHTTP backend
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod winhttp {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;

    const ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
    const FLAG_SECURE: u32 = 0x0080_0000;
    const QUERY_STATUS_CODE: u32 = 19;
    const QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const ERROR_WINHTTP_TIMEOUT: u32 = 12_001;
    /// Receive timeout for body reads of a cancellable request: small enough that a cancel is noticed
    /// quickly, large enough that a genuinely slow transfer keeps making progress.
    const BODY_POLL_MS: i32 = 1_000;
    /// Longest time-to-first-byte a cancellable request waits for. A host that takes longer than
    /// this is treated as stalled and retried on another CDN property.
    const CANCELLABLE_HEADER_TIMEOUT_SECS: u64 = 10;

    #[link(name = "winhttp")]
    extern "system" {
        fn WinHttpOpen(
            agent: *const u16,
            access_type: u32,
            proxy: *const u16,
            proxy_bypass: *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpConnect(session: Handle, server: *const u16, port: u16, reserved: u32) -> Handle;
        fn WinHttpOpenRequest(
            connect: Handle,
            verb: *const u16,
            object: *const u16,
            version: *const u16,
            referrer: *const u16,
            accept_types: *const *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpSetTimeouts(
            handle: Handle,
            resolve: i32,
            connect: i32,
            send: i32,
            receive: i32,
        ) -> i32;
        fn WinHttpAddRequestHeaders(
            request: Handle,
            headers: *const u16,
            len: u32,
            modifiers: u32,
        ) -> i32;
        fn WinHttpSendRequest(
            request: Handle,
            headers: *const u16,
            headers_len: u32,
            optional: *mut c_void,
            optional_len: u32,
            total_len: u32,
            context: usize,
        ) -> i32;
        fn WinHttpReceiveResponse(request: Handle, reserved: *mut c_void) -> i32;
        fn WinHttpQueryHeaders(
            request: Handle,
            info_level: u32,
            name: *const u16,
            buffer: *mut c_void,
            buffer_len: *mut u32,
            index: *mut u32,
        ) -> i32;
        fn WinHttpQueryDataAvailable(request: Handle, bytes_available: *mut u32) -> i32;
        fn WinHttpReadData(
            request: Handle,
            buffer: *mut c_void,
            to_read: u32,
            read: *mut u32,
        ) -> i32;
        fn WinHttpCloseHandle(handle: Handle) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLastError() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn last_error(what: &str) -> Error {
        let code = unsafe { GetLastError() };
        Error::net(format!("{what} failed (WinHTTP error {code})"))
    }

    struct Owned(Handle);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    thread_local! {
        static SESSION: RefCell<Option<Owned>> = const { RefCell::new(None) };
    }

    pub fn execute<W: Write>(agent: &str, req: &Request, sink: &mut W) -> Result<ResponseHead> {
        let (scheme, host, port, path) = split_url(&req.url)?;
        let secure = scheme == "https";
        let agent_w = wide(agent);

        SESSION.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let handle = unsafe {
                    WinHttpOpen(
                        agent_w.as_ptr(),
                        ACCESS_TYPE_DEFAULT_PROXY,
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                    )
                };
                if handle.is_null() {
                    return Err(last_error("WinHttpOpen"));
                }
                *slot = Some(Owned(handle));
            }
            let session = slot.as_ref().map(|o| o.0).unwrap_or(std::ptr::null_mut());
            do_request(session, secure, &host, port, &path, req, sink)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn do_request<W: Write>(
        session: Handle,
        secure: bool,
        host: &str,
        port: u16,
        path: &str,
        req: &Request,
        sink: &mut W,
    ) -> Result<ResponseHead> {
        let host_w = wide(host);
        let connect = unsafe { WinHttpConnect(session, host_w.as_ptr(), port, 0) };
        if connect.is_null() {
            return Err(last_error("WinHttpConnect"));
        }
        let connect = Owned(connect);

        let verb = wide(req.method.as_str());
        let object = wide(path);
        let flags = if secure { FLAG_SECURE } else { 0 };
        let request = unsafe {
            WinHttpOpenRequest(
                connect.0,
                verb.as_ptr(),
                object.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                flags,
            )
        };
        if request.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        let request = Owned(request);

        // Cancellable requests cap how long they wait for the first byte: a stalled host must not
        // hold the worker (and with it the cancel button) for the full request timeout.
        let cancellable = req.cancel.is_some();
        let header_secs = if cancellable {
            req.timeout_secs.min(CANCELLABLE_HEADER_TIMEOUT_SECS)
        } else {
            req.timeout_secs
        };
        // DNS and TCP/TLS setup block outside our control, so a cancellable request uses shorter
        // budgets there as well; a dead host is retried on another CDN property anyway.
        let resolve_ms: i32 = if cancellable { 5_000 } else { 20_000 };
        let connect_ms: i32 = if cancellable { 5_000 } else { 15_000 };
        let timeout = (header_secs.min(u32::MAX as u64) as i32) * 1000;
        unsafe {
            WinHttpSetTimeouts(request.0, resolve_ms, connect_ms, 30_000, timeout);
        }

        if cancelled(req) {
            return Err(Error::net("request cancelled"));
        }

        if !req.headers.is_empty() {
            let mut block = String::new();
            for (name, value) in &req.headers {
                block.push_str(&format!("{name}: {value}\r\n"));
            }
            let block_w = wide(&block);
            let ok = unsafe {
                WinHttpAddRequestHeaders(request.0, block_w.as_ptr(), u32::MAX, 0x2000_0000)
            };
            if ok == 0 {
                return Err(last_error("WinHttpAddRequestHeaders"));
            }
        }

        let (body_ptr, body_len) = match &req.body {
            Some(body) => (body.as_ptr() as *mut c_void, body.len() as u32),
            None => (std::ptr::null_mut(), 0),
        };
        let ok = unsafe {
            WinHttpSendRequest(
                request.0,
                std::ptr::null(),
                0,
                body_ptr,
                body_len,
                body_len,
                0,
            )
        };
        if ok == 0 {
            return Err(last_error("WinHttpSendRequest"));
        }
        let ok = unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) };
        if ok == 0 {
            if cancelled(req) {
                return Err(Error::net("request cancelled"));
            }
            return Err(last_error("WinHttpReceiveResponse"));
        }

        let mut status: u32 = 0;
        let mut len: u32 = std::mem::size_of::<u32>() as u32;
        let ok = unsafe {
            WinHttpQueryHeaders(
                request.0,
                QUERY_STATUS_CODE | QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut status as *mut u32 as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_INSUFFICIENT_BUFFER {
                return Err(last_error("WinHttpQueryHeaders"));
            }
        }

        let mut buf = vec![0u8; 64 * 1024];
        if req.cancel.is_some() {
            // From here on reads poll at ~1 s so cancellation is honoured even when the host stops
            // sending; the idle deadline preserves the "no progress for N seconds" failure mode.
            unsafe {
                WinHttpSetTimeouts(request.0, resolve_ms, connect_ms, 30_000, BODY_POLL_MS);
            }
        }
        let idle_window = Duration::from_secs(req.timeout_secs.max(1));
        let mut idle_deadline = Instant::now() + idle_window;
        loop {
            if cancelled(req) {
                return Err(Error::net("request cancelled"));
            }
            if Instant::now() > idle_deadline {
                return Err(Error::net(format!(
                    "no data for {} s; request timed out",
                    req.timeout_secs.max(1)
                )));
            }
            let mut available: u32 = 0;
            let ok = unsafe { WinHttpQueryDataAvailable(request.0, &mut available) };
            if ok == 0 {
                // A cancelled request reports the cancellation, not the transport error it caused.
                if cancelled(req) {
                    return Err(Error::net("request cancelled"));
                }
                let code = unsafe { GetLastError() };
                if req.cancel.is_some() && code == ERROR_WINHTTP_TIMEOUT {
                    // Nothing arrived within the poll window; loop so a cancel is seen promptly.
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                return Err(last_error("WinHttpQueryDataAvailable"));
            }
            if available == 0 {
                break;
            }
            let want = available.min(buf.len() as u32);
            let mut read: u32 = 0;
            let ok = unsafe {
                WinHttpReadData(
                    request.0,
                    buf.as_mut_ptr() as *mut c_void,
                    want,
                    &mut read,
                )
            };
            if ok == 0 {
                if cancelled(req) {
                    return Err(Error::net("request cancelled"));
                }
                return Err(last_error("WinHttpReadData"));
            }
            if read == 0 {
                break;
            }
            sink.write_all(&buf[..read as usize])
                .map_err(|e| Error::net(format!("write body failed: {e}")))?;
            idle_deadline = Instant::now() + idle_window;
        }

        Ok(ResponseHead { status: status as u16, headers: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls() {
        let (scheme, host, port, path) =
            split_url("https://account-public-service-prod.ol.epicgames.com/account/api/oauth/token")
                .unwrap();
        assert_eq!(scheme, "https");
        assert_eq!(host, "account-public-service-prod.ol.epicgames.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/account/api/oauth/token");

        let (scheme, host, port, path) = split_url("http://127.0.0.1:8080/a/b?x=1").unwrap();
        assert_eq!(scheme, "http");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);
        assert_eq!(path, "/a/b?x=1");

        assert!(split_url("ftp://example.com").is_err());
        assert!(split_url("example.com/path").is_err());
    }

    #[test]
    fn urlencodes_forms() {
        assert_eq!(urlencode("grant_type"), "grant_type");
        assert_eq!(urlencode("a b+c/d"), "a%20b%2Bc%2Fd");
    }

    #[test]
    fn builds_form_post() {
        let req = Request::post_form("https://x/y", &[("grant_type", "device_code")]);
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.body.unwrap(), b"grant_type=device_code".to_vec());
        assert!(req.headers.iter().any(|(k, _)| k == "Content-Type"));
    }

    /// The chunked branch of the plain backend was rewritten for cancellable reads; keep a guard on
    /// the framing itself.
    #[test]
    fn plain_backend_reads_chunked_bodies() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mocked server");
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n",
            );
            let _ = stream.flush();
        });

        let client = Client::plain();
        let (head, body) = client
            .request_vec(&Request::get(format!("http://{addr}/chunked")))
            .expect("chunked request");
        assert_eq!(head.status, 200);
        assert_eq!(body, b"hello world");
        server.join().unwrap();
    }

    /// The production backend must break out of a stalled body read promptly on cancel.
    ///
    /// Regression guard for the case where `WinHttpQueryDataAvailable` blocked for the full request
    /// timeout (120 s), which made the GUI's cancel button feel broken.
    #[cfg(windows)]
    #[test]
    fn winhttp_cancel_breaks_a_stalled_body_read() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stall server");
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\n\r\n");
            let _ = stream.flush();
            while !server_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let client = Client::new();
        assert_eq!(client.backend(), Backend::WinHttp, "this test exercises WinHTTP");

        let cancel = Arc::new(AtomicBool::new(false));
        let url = format!("http://{addr}/stall");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            let req = Request::get(url).timeout(120).cancellable(worker_cancel);
            let mut sink: Vec<u8> = Vec::new();
            let outcome = match client.request(&req, &mut sink) {
                Ok(head) => Ok(head.status),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(outcome);
        });

        // Let the request reach the blocked body read.
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        let observed = rx.recv_timeout(Duration::from_secs(10));
        let elapsed = started.elapsed();

        stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
        let _ = server.join();

        let observed = observed.expect("request did not return after cancel");
        eprintln!("winhttp cancel latency on a stalled body: {elapsed:?} ({observed:?})");
        // WinHTTP's own stall detection needs a few seconds before it reports the dead connection;
        // what matters is that the request no longer sits there for the 120 s request timeout.
        assert!(elapsed < Duration::from_secs(8), "cancel took {elapsed:?}");
        match observed {
            Err(message) => {
                assert!(message.contains("cancel"), "unexpected error: {message}")
            }
            Ok(status) => panic!("request finished with HTTP {status} instead of being cancelled"),
        }
    }
}
