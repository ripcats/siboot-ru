use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PAGE: &str = include_str!("web.html");
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                   connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 4 * 1024;
const MAX_CONNECTIONS: usize = 16;
const READ_WAIT: Duration = Duration::from_secs(10);
const WRITE_WAIT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(5);
const CLOSE_WAIT: Duration = Duration::from_secs(3);

static SERVER: OnceLock<Server> = OnceLock::new();

pub fn start() -> io::Result<String> {
    let server = Server::bind()?;
    let url = server.url();
    SERVER
        .set(server)
        .map_err(|_| io::Error::other("the page is served already"))?;
    Ok(url)
}

pub fn page() -> Option<&'static Server> {
    SERVER.get()
}

pub fn send(event: Value) {
    if let Some(server) = page() {
        server.send(event);
    }
}

pub fn progress(label: &str, done: u64, total: u64, bytes: bool) {
    if let Some(server) = page() {
        server.progress(label, done, total, bytes);
    }
}

pub fn progress_end() {
    if let Some(server) = page() {
        server.progress_end();
    }
}

pub fn on_stop(stop: impl Fn() + Send + Sync + 'static) {
    if let Some(server) = page() {
        *server
            .shared
            .on_stop
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Box::new(stop));
    }
}

pub fn close() {
    if let Some(server) = page() {
        server.close();
    }
}

#[allow(dead_code)]
pub fn wait_for_page(longest: Duration) -> bool {
    let Some(server) = page() else { return false };
    let shared = &server.shared;
    let deadline = Instant::now() + longest;
    let mut state = shared.lock();
    while state.streams.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        state = shared
            .changed
            .wait_timeout(state, left)
            .map(|(state, _)| state)
            .unwrap_or_else(|e| e.into_inner().0);
    }
    true
}

pub fn available() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(dir.join("minis-open"))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

pub fn show(url: &str) -> bool {
    Command::new("minis-open")
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub struct Server {
    shared: Arc<Shared>,
}

struct Shared {
    key: String,
    port: u16,
    started: Instant,
    state: Mutex<State>,
    changed: Condvar,
    connections: AtomicUsize,
    on_stop: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct State {
    events: Vec<String>,
    asking: Option<u64>,
    answer: Option<Value>,
    questions: u64,
    streams: HashMap<u64, usize>,
    stream_ids: u64,
    progress: Option<(String, u64)>,
    ended: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Code {
    Digits(String),
    More,
    Cancel,
}

impl Server {
    fn bind() -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            key: random_key()?,
            port,
            started: Instant::now(),
            state: Mutex::default(),
            changed: Condvar::new(),
            connections: AtomicUsize::new(0),
            on_stop: Mutex::new(None),
        });
        let serving = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("siboot-web".into())
            .spawn(move || accept(serving, listener))?;
        Ok(Server { shared })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/", self.shared.port, self.shared.key)
    }

    pub fn send(&self, event: Value) {
        let mut state = self.shared.lock();
        self.shared.push(&mut state, event);
    }

    pub fn progress(&self, label: &str, done: u64, total: u64, bytes: bool) {
        let percent = done
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(100)
            .min(100);
        let mut state = self.shared.lock();
        if state
            .progress
            .as_ref()
            .is_some_and(|(shown, at)| shown == label && *at == percent)
        {
            return;
        }
        state.progress = Some((label.to_string(), percent));
        let event =
            json!({"t": "progress", "label": label, "done": done, "total": total, "bytes": bytes});
        self.shared.push(&mut state, event);
    }

    pub fn progress_end(&self) {
        let mut state = self.shared.lock();
        state.progress = None;
        self.shared.push(&mut state, json!({"t": "progress_end"}));
    }

    fn ask(&self, mut question: Value) -> Value {
        let shared = &self.shared;
        let mut state = shared.lock();
        state.questions += 1;
        let id = state.questions;
        question["t"] = json!("ask");
        question["id"] = json!(id);
        state.asking = Some(id);
        state.answer = None;
        shared.push(&mut state, question);
        let answer = loop {
            if let Some(answer) = state.answer.take() {
                break answer;
            }
            state = shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        };
        state.asking = None;
        shared.push(&mut state, json!({"t": "answered", "id": id}));
        answer
    }

    pub fn text(
        &self,
        question: &str,
        label: &str,
        placeholder: &str,
        secret: bool,
    ) -> Option<String> {
        let kind = if secret { "secret" } else { "text" };
        loop {
            let answer = self.ask(json!({
                "kind": kind, "question": question, "label": label, "placeholder": placeholder,
            }));
            if cancelled(&answer) {
                return None;
            }
            if let Some(value) = answer["value"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                return Some(value.to_string());
            }
        }
    }

    pub fn code(&self, question: &str, label: &str, hints: &[(bool, &str)], more: bool) -> Code {
        let hints: Vec<Value> = hints
            .iter()
            .map(|(warning, text)| json!({"warning": warning, "text": text}))
            .collect();
        loop {
            let answer = self.ask(json!({
                "kind": "code", "question": question, "label": label, "hints": hints, "more": more,
            }));
            if cancelled(&answer) {
                return Code::Cancel;
            }
            if more && answer["more"] == json!(true) {
                return Code::More;
            }
            let digits: String = answer["value"]
                .as_str()
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '-')
                .collect();
            if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()) {
                return Code::Digits(digits);
            }
        }
    }

    pub fn confirm(&self, question: &str, default: bool) -> bool {
        let answer = self.ask(json!({"kind": "confirm", "question": question, "default": default}));
        answer["yes"].as_bool().unwrap_or(false)
    }

    pub fn ready(&self, title: &str, steps: &[&str], notes: &[&str], button: &str) -> bool {
        let answer = self.ask(json!({
            "kind": "ready", "question": title, "steps": steps, "notes": notes, "button": button,
        }));
        answer["go"] == json!(true)
    }

    pub fn choose(&self, title: &str, options: &[String]) -> Option<usize> {
        loop {
            let answer = self.ask(json!({"kind": "choose", "question": title, "options": options}));
            if cancelled(&answer) {
                return None;
            }
            if let Some(i) = answer["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
            {
                if i < options.len() {
                    return Some(i);
                }
            }
        }
    }

    pub fn line(&self, prompt: &str) -> Option<String> {
        let question = prompt.trim().trim_end_matches(':');
        let answer =
            self.ask(json!({"kind": "line", "question": question, "label": "", "placeholder": ""}));
        if cancelled(&answer) {
            return None;
        }
        Some(answer["value"].as_str().unwrap_or("").trim().to_string())
    }

    pub fn close(&self) {
        let shared = &self.shared;
        let mut state = shared.lock();
        if !state.ended {
            state.ended = true;
            shared.push(&mut state, json!({"t": "end"}));
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        loop {
            let len = state.events.len();
            if state.streams.values().all(|sent| *sent >= len) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            state = shared
                .changed
                .wait_timeout(state, left)
                .map(|(state, _)| state)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

fn cancelled(answer: &Value) -> bool {
    answer["cancel"] == json!(true)
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn push(&self, state: &mut State, mut event: Value) {
        if let Value::Object(fields) = &mut event {
            fields.insert("at".into(), json!(self.now()));
        }
        state.events.push(event.to_string());
        self.changed.notify_all();
    }

    fn after_key<'a>(&self, path: &'a str) -> Option<&'a str> {
        let rest = path.strip_prefix('/')?;
        let (given, rest) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        same(given.as_bytes(), self.key.as_bytes()).then_some(rest)
    }

    fn is_own_host(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    fn is_own_origin(&self, origin: &str) -> bool {
        origin
            .trim()
            .to_ascii_lowercase()
            .strip_prefix("http://")
            .is_some_and(|host| self.is_own_host(host))
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_key() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().fold(String::new(), |mut key, b| {
        let _ = write!(key, "{b:02x}");
        key
    }))
}

fn accept(shared: Arc<Shared>, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if shared.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let serving = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("siboot-web-request".into())
            .spawn(move || {
                let _ = handle(&serving, stream);
                serving.connections.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(stream: &TcpStream) -> Result<Request, u16> {
    let _ = stream.set_read_timeout(Some(READ_WAIT));
    let mut reader = BufReader::new(stream);
    let timed_out = |e: io::Error| -> u16 {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => 408,
            _ => 400,
        }
    };
    let mut lines = Vec::new();
    let mut read = 0;
    loop {
        let mut raw = Vec::new();
        let limit = (MAX_HEAD + 1 - read) as u64;
        let n = (&mut reader)
            .take(limit)
            .read_until(b'\n', &mut raw)
            .map_err(timed_out)?;
        if n == 0 {
            return Err(400);
        }
        read += n;
        if read > MAX_HEAD {
            return Err(431);
        }
        let line = String::from_utf8(raw).map_err(|_| 400u16)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        lines.push(line.to_string());
    }
    let mut lines = lines.into_iter();
    let first = lines.next().ok_or(400u16)?;
    let mut words = first.split_whitespace();
    let (Some(method), Some(target), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(400);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(400);
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let mut request = Request {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        body: Vec::new(),
    };
    if let Some(length) = request.header("content-length") {
        let length: usize = length.parse().map_err(|_| 400u16)?;
        if length > MAX_BODY {
            return Err(413);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).map_err(timed_out)?;
        request.body = body;
    }
    Ok(request)
}

fn handle(shared: &Shared, stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_WAIT));
    let request = match read_request(&stream) {
        Ok(request) => request,
        Err(status) => {
            refuse(&stream, status)?;
            linger(&stream);
            return Ok(());
        }
    };
    if !request
        .header("host")
        .is_some_and(|host| shared.is_own_host(host))
    {
        return refuse(&stream, 403);
    }
    let path = request.target.split('?').next().unwrap_or("");
    let Some(rest) = shared.after_key(path) else {
        return refuse(&stream, 404);
    };
    match (request.method.as_str(), rest) {
        ("GET", "") => {
            let to = format!("/{}/", shared.key);
            respond(&stream, 301, &[("Location", &to)], b"")
        }
        ("GET", "/") => respond(
            &stream,
            200,
            &[
                ("Content-Type", "text/html; charset=utf-8"),
                ("Content-Security-Policy", CSP),
                ("X-Frame-Options", "DENY"),
            ],
            PAGE.as_bytes(),
        ),
        ("GET", "/events") => events(shared, &stream, &request),
        ("POST", "/answer") => answer(shared, &stream, &request),
        ("POST", "/stop") => stop(shared, &stream, &request),
        (_, "" | "/" | "/events" | "/answer" | "/stop") => refuse(&stream, 405),
        _ => refuse(&stream, 404),
    }
}

fn posted_json(shared: &Shared, request: &Request) -> Result<Value, u16> {
    if let Some(origin) = request.header("origin") {
        if !shared.is_own_origin(origin) {
            return Err(403);
        }
    }
    let json = request.header("content-type").is_some_and(|t| {
        t.trim()
            .to_ascii_lowercase()
            .starts_with("application/json")
    });
    if !json {
        return Err(415);
    }
    serde_json::from_slice(&request.body).map_err(|_| 400)
}

fn answer(shared: &Shared, stream: &TcpStream, request: &Request) -> io::Result<()> {
    let given = match posted_json(shared, request) {
        Ok(given) => given,
        Err(status) => return refuse(stream, status),
    };
    let Some(id) = given["id"].as_u64() else {
        return refuse(stream, 400);
    };
    let taken = {
        let mut state = shared.lock();
        if state.asking == Some(id) && state.answer.is_none() {
            state.answer = Some(given);
            shared.changed.notify_all();
            true
        } else {
            false
        }
    };
    respond(stream, if taken { 204 } else { 409 }, &[], b"")
}

fn stop(shared: &Shared, stream: &TcpStream, request: &Request) -> io::Result<()> {
    if let Err(status) = posted_json(shared, request) {
        return refuse(stream, status);
    }
    respond(stream, 204, &[], b"")?;
    if let Some(stop) = shared
        .on_stop
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        stop();
    }
    Ok(())
}

fn events(shared: &Shared, mut stream: &TcpStream, request: &Request) -> io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
          X-Content-Type-Options: nosniff\r\nConnection: keep-alive\r\n\r\nretry: 1000\n\n",
    )?;
    stream.flush()?;
    let resume = request
        .header("last-event-id")
        .and_then(|id| id.trim().parse::<usize>().ok())
        .map(|id| id + 1);
    let (id, mut sent) = {
        let mut state = shared.lock();
        let sent = resume.unwrap_or(0).min(state.events.len());
        state.stream_ids += 1;
        let id = state.stream_ids;
        state.streams.insert(id, sent);
        shared.changed.notify_all();
        (id, sent)
    };
    let result = (|| -> io::Result<()> {
        loop {
            let batch: Vec<String> = {
                let mut state = shared.lock();
                let deadline = Instant::now() + HEARTBEAT;
                while state.events.len() <= sent {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    state = shared
                        .changed
                        .wait_timeout(state, left)
                        .map(|(state, _)| state)
                        .unwrap_or_else(|e| e.into_inner().0);
                }
                state.events[sent.min(state.events.len())..].to_vec()
            };
            let mut out = String::new();
            if batch.is_empty() {
                let _ = write!(out, "event: ping\ndata: {{\"at\":{}}}\n\n", shared.now());
            }
            for (i, event) in batch.iter().enumerate() {
                let _ = write!(out, "id: {}\ndata: {event}\n\n", sent + i);
            }
            stream.write_all(out.as_bytes())?;
            stream.flush()?;
            sent += batch.len();
            shared.lock().streams.insert(id, sent);
            shared.changed.notify_all();
        }
    })();
    shared.lock().streams.remove(&id);
    shared.changed.notify_all();
    result
}

fn respond(
    mut stream: &TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    let _ = write!(
        head,
        "Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn refuse(stream: &TcpStream, status: u16) -> io::Result<()> {
    respond(
        stream,
        status,
        &[("Content-Type", "text/plain; charset=utf-8")],
        reason(status).as_bytes(),
    )
}

fn linger(stream: &TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut input = stream;
    let mut sink = [0u8; 4096];
    let mut left = 64 * 1024usize;
    while left > 0 {
        match input.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn served() -> Server {
        Server::bind().expect("a loopback port")
    }

    fn host(server: &Server) -> String {
        format!("127.0.0.1:{}", server.shared.port)
    }

    fn base(server: &Server) -> String {
        format!("/{}", server.shared.key)
    }

    fn exchange(server: &Server, raw: &str) -> (u16, Vec<(String, String)>, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", server.shared.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        let reply = String::from_utf8_lossy(&reply).to_string();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        (status, headers, body.to_string())
    }

    fn get(server: &Server, path: &str) -> (u16, Vec<(String, String)>, String) {
        let raw = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n\r\n", host(server));
        exchange(server, &raw)
    }

    fn post(server: &Server, path: &str, body: &str, extra: &str) -> u16 {
        let raw = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n{extra}\r\n{body}",
            host(server),
            body.len()
        );
        exchange(server, &raw).0
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn subscribe(server: &Server, last_id: Option<usize>) -> BufReader<TcpStream> {
        let mut stream = TcpStream::connect(("127.0.0.1", server.shared.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let resume = last_id
            .map(|id| format!("Last-Event-ID: {id}\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "GET {}/events HTTP/1.1\r\nHost: {}\r\n{resume}\r\n",
            base(server),
            host(server)
        );
        stream.write_all(raw.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        reader
    }

    fn next_event(reader: &mut BufReader<TcpStream>) -> (usize, Value) {
        let mut id = None;
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0, "the stream ended");
            let line = line.trim_end();
            if let Some(n) = line.strip_prefix("id: ") {
                id = n.parse().ok();
            } else if let Some(data) = line.strip_prefix("data: ") {
                if let Some(id) = id.take() {
                    return (id, serde_json::from_str(data).unwrap());
                }
            }
        }
    }

    fn until(reader: &mut BufReader<TcpStream>, t: &str) -> Value {
        loop {
            let (_, event) = next_event(reader);
            if event["t"] == t {
                return event;
            }
        }
    }

    #[test]
    fn every_path_starts_with_the_key() {
        let server = served();
        assert_eq!(get(&server, "/").0, 404);
        assert_eq!(get(&server, "/events").0, 404);
        let wrong = format!("/{}/", "0".repeat(32));
        assert_eq!(get(&server, &wrong).0, 404);
        let (status, headers, body) = get(&server, &format!("{}/", base(&server)));
        assert_eq!(status, 200);
        assert!(body.contains("<title>siboot</title>"));
        assert_eq!(header(&headers, "content-security-policy"), Some(CSP));
        assert_eq!(header(&headers, "referrer-policy"), Some("no-referrer"));
        assert_eq!(header(&headers, "cache-control"), Some("no-store"));
        assert_eq!(header(&headers, "x-frame-options"), Some("DENY"));
        let (status, headers, _) = get(&server, &base(&server));
        assert_eq!(status, 301);
        let to = format!("{}/", base(&server));
        assert_eq!(header(&headers, "location"), Some(to.as_str()));
        assert!(server.url().ends_with(&to));
        assert_eq!(server.shared.key.len(), 32);
    }

    #[test]
    fn another_host_is_refused() {
        let server = served();
        let raw = format!(
            "GET {}/ HTTP/1.1\r\nHost: rebound.example:{}\r\n\r\n",
            base(&server),
            server.shared.port
        );
        assert_eq!(exchange(&server, &raw).0, 403);
        let raw = format!("GET {}/ HTTP/1.1\r\n\r\n", base(&server));
        assert_eq!(exchange(&server, &raw).0, 403, "no Host at all");
        let raw = format!(
            "GET {}/ HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
            base(&server),
            server.shared.port
        );
        assert_eq!(exchange(&server, &raw).0, 200);
    }

    #[test]
    fn a_post_must_be_json_from_this_page() {
        let server = served();
        let stop = format!("{}/stop", base(&server));
        let raw = format!(
            "POST {stop} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}",
            host(&server)
        );
        assert_eq!(exchange(&server, &raw).0, 415);
        let foreign = "Origin: https://somewhere.example\r\n";
        assert_eq!(post(&server, &stop, "{}", foreign), 403);
        let raw = format!("OPTIONS {stop} HTTP/1.1\r\nHost: {}\r\n\r\n", host(&server));
        assert_eq!(exchange(&server, &raw).0, 405);
        assert_eq!(
            post(
                &server,
                &format!("{}/answer", base(&server)),
                "not json",
                ""
            ),
            400
        );
        let own = format!("Origin: http://127.0.0.1:{}\r\n", server.shared.port);
        assert_eq!(post(&server, &stop, "{}", &own), 204);
    }

    #[test]
    fn oversized_requests_are_refused() {
        let server = served();
        let raw = format!(
            "GET {}/ HTTP/1.1\r\nHost: {}\r\nX-Pad: {}\r\n\r\n",
            base(&server),
            host(&server),
            "a".repeat(MAX_HEAD)
        );
        assert_eq!(exchange(&server, &raw).0, 431);
        let body = "x".repeat(MAX_BODY + 1);
        assert_eq!(
            post(&server, &format!("{}/answer", base(&server)), &body, ""),
            413
        );
    }

    #[test]
    fn events_are_replayed_then_streamed() {
        let server = served();
        server.send(json!({"t": "header", "version": "9.9.9"}));
        server.send(json!({"t": "step", "n": 1, "title": "Проверка"}));
        let mut reader = subscribe(&server, None);
        let (id, first) = next_event(&mut reader);
        assert_eq!((id, first["t"].as_str()), (0, Some("header")));
        assert!(first["at"].is_u64());
        assert_eq!(next_event(&mut reader).0, 1);
        server.send(json!({"t": "done", "summary": "fine"}));
        let (id, third) = next_event(&mut reader);
        assert_eq!((id, third["summary"].as_str()), (2, Some("fine")));
        let mut again = subscribe(&server, Some(1));
        assert_eq!(next_event(&mut again).0, 2);
    }

    #[test]
    fn progress_sends_one_event_a_percent() {
        let server = served();
        for done in 0..=1000u64 {
            server.progress("downloaded", done, 1000, true);
        }
        server.progress_end();
        let state = server.shared.lock();
        let bars = state
            .events
            .iter()
            .filter(|e| e.contains("\"progress\""))
            .count();
        assert_eq!(bars, 101);
        assert!(state.events.last().unwrap().contains("progress_end"));
    }

    fn asked<T: Send + 'static>(
        server: &'static Server,
        ask: impl FnOnce(&Server) -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        std::thread::spawn(move || ask(server))
    }

    fn leaked() -> &'static Server {
        Box::leak(Box::new(served()))
    }

    fn reply(server: &Server, id: u64, fields: Value) -> u16 {
        let mut body = fields;
        body["id"] = json!(id);
        post(
            server,
            &format!("{}/answer", base(server)),
            &body.to_string(),
            "",
        )
    }

    #[test]
    fn a_question_waits_for_its_answer() {
        let server = leaked();
        let mut reader = subscribe(server, None);
        let email = asked(server, |s| {
            s.text(
                "войдите в учётную запись Apple",
                "Электронная почта",
                "адрес учётной записи Apple",
                false,
            )
        });
        let question = until(&mut reader, "ask");
        assert_eq!(question["kind"], "text");
        assert_eq!(question["label"], "Электронная почта");
        let id = question["id"].as_u64().unwrap();
        assert_eq!(
            reply(server, id + 1, json!({"value": "x"})),
            409,
            "not asked"
        );
        assert_eq!(
            reply(server, id, json!({"value": "  me@example.com "})),
            204
        );
        assert_eq!(
            reply(server, id, json!({"value": "again"})),
            409,
            "answered already"
        );
        assert_eq!(email.join().unwrap().as_deref(), Some("me@example.com"));
        assert_eq!(until(&mut reader, "answered")["id"], id);
        let state = server.shared.lock();
        assert!(state.events.iter().all(|e| !e.contains("me@example.com")));
    }

    #[test]
    fn an_empty_answer_asks_again_and_cancel_stops() {
        let server = leaked();
        let mut reader = subscribe(server, None);
        let password = asked(server, |s| s.text("вход", "Пароль", "", true));
        let first = until(&mut reader, "ask");
        assert_eq!(first["kind"], "secret");
        assert_eq!(
            reply(
                server,
                first["id"].as_u64().unwrap(),
                json!({"value": "  "})
            ),
            204
        );
        let second = until(&mut reader, "ask");
        assert_ne!(second["id"], first["id"]);
        assert_eq!(
            reply(
                server,
                second["id"].as_u64().unwrap(),
                json!({"cancel": true})
            ),
            204
        );
        assert_eq!(password.join().unwrap(), None);
    }

    #[test]
    fn a_code_is_six_digits_or_another_way() {
        let server = leaked();
        let mut reader = subscribe(server, None);
        let code = asked(server, |s| {
            s.code(
                "введите код от Apple",
                "Код",
                &[
                    (true, "Ответ Apple: неверный код"),
                    (false, "отправлен на ваши устройства Apple"),
                ],
                true,
            )
        });
        let question = until(&mut reader, "ask");
        assert_eq!(question["hints"][0]["warning"], true);
        assert_eq!(question["more"], true);
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"value": "12a456"}),
        );
        let again = until(&mut reader, "ask");
        reply(
            server,
            again["id"].as_u64().unwrap(),
            json!({"value": "123 456"}),
        );
        assert_eq!(code.join().unwrap(), Code::Digits("123456".into()));

        let code = asked(server, |s| s.code("введите код от Apple", "Код", &[], true));
        let question = until(&mut reader, "ask");
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"more": true}),
        );
        assert_eq!(code.join().unwrap(), Code::More);

        let code = asked(server, |s| {
            s.code("rootshell предлагает файл сопряжения", "Код", &[], false)
        });
        let question = until(&mut reader, "ask");
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"more": true}),
        );
        let again = until(&mut reader, "ask");
        reply(
            server,
            again["id"].as_u64().unwrap(),
            json!({"cancel": true}),
        );
        assert_eq!(code.join().unwrap(), Code::Cancel);
    }

    #[test]
    fn choices_and_confirmations() {
        let server = leaked();
        let mut reader = subscribe(server, None);
        let options = vec!["Отправить SMS".to_string(), "Позвонить".to_string()];
        let chosen = asked(server, move |s| {
            s.choose("как Apple отправит код?", &options)
        });
        let question = until(&mut reader, "ask");
        assert_eq!(question["options"][1], "Позвонить");
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"index": 2}),
        );
        let again = until(&mut reader, "ask");
        reply(server, again["id"].as_u64().unwrap(), json!({"index": 1}));
        assert_eq!(chosen.join().unwrap(), Some(1));

        let confirmed = asked(server, |s| {
            s.confirm("скопировать журнал в буфер обмена?", true)
        });
        let question = until(&mut reader, "ask");
        assert_eq!(question["default"], true);
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"cancel": true}),
        );
        assert!(!confirmed.join().unwrap(), "cancelling is no");
    }

    #[test]
    fn getting_ready_shows_the_steps_and_starts_or_cancels() {
        let server = leaked();
        let mut reader = subscribe(server, None);
        let started = asked(server, |s| {
            s.ready(
                "Сопряжение iPhone с siboot",
                &["Откройте Настройки.", "Нажмите Pair with siboot."],
                &["Откроется плеер."],
                "Начать сопряжение",
            )
        });
        let question = until(&mut reader, "ask");
        assert_eq!(question["kind"], "ready");
        assert_eq!(question["steps"][1], "Нажмите Pair with siboot.");
        assert_eq!(question["notes"][0], "Откроется плеер.");
        assert_eq!(question["button"], "Начать сопряжение");
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"go": true}),
        );
        assert!(started.join().unwrap());

        let cancelled = asked(server, |s| {
            s.ready("Сопряжение", &["Откройте Настройки."], &[], "Начать")
        });
        let question = until(&mut reader, "ask");
        reply(
            server,
            question["id"].as_u64().unwrap(),
            json!({"cancel": true}),
        );
        assert!(!cancelled.join().unwrap());
    }

    #[test]
    fn stop_runs_the_hook() {
        let server = served();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        *server.shared.on_stop.lock().unwrap() =
            Some(Box::new(move || flag.store(true, Ordering::SeqCst)));
        assert_eq!(
            post(&server, &format!("{}/stop", base(&server)), "{}", ""),
            204
        );
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn closing_ends_the_stream_and_waits_for_the_page() {
        let server = served();
        server.send(json!({"t": "finish", "title": "Готово"}));
        let mut reader = subscribe(&server, None);
        let started = Instant::now();
        server.close();
        server.close();
        assert!(started.elapsed() < CLOSE_WAIT, "the page had everything");
        assert_eq!(until(&mut reader, "end")["t"], "end");
        let ends = server
            .shared
            .lock()
            .events
            .iter()
            .filter(|e| e.contains("\"end\""))
            .count();
        assert_eq!(ends, 1);
        let alone = served();
        let started = Instant::now();
        alone.close();
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
