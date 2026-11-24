//! HTTP over `Api`: routes, authentication and the event stream, and no logic of its own.
//! See "Client and server" in DESIGN.md; API.md is the contract for clients.
use crate::api::{
    Answer, Api, Code, Command, Error, Event, PinTarget, Query, Reply, Section, State, TaskRef,
    VERSION, Verb,
};
use anyhow::{Context as _, Result, bail};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path as P, Query as Q, Request, State as S};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use libc::{SIGHUP, SIGINT, SIGTERM};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Semaphore;
use tokio::sync::broadcast::{self, error::RecvError};

pub const PORT: u16 = 7878;
const BODY_LIMIT: usize = 64 * 1024;
/// For a whole request head, and then for its body.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections at a time, event streams included.
const CONNECTIONS: usize = 64;
const KEEPALIVE: Duration = Duration::from_secs(15);
const MAX_REQUEST_ID: usize = 128;

/// Refuses every command but `Refresh`.
pub struct ReadOnly(pub Arc<dyn Api>);

impl Api for ReadOnly {
    fn state(&self) -> Arc<State> {
        self.0.state()
    }

    fn query(&self, q: Query) -> Result<Answer, Error> {
        let mut a = self.0.query(q)?;
        if let Answer::Server(s) = &mut a {
            s.read_only = true;
        }
        Ok(a)
    }

    fn command(&self, c: Command) -> Result<Reply, Error> {
        match c.verb {
            Verb::Refresh { .. } => self.0.command(c),
            _ => Err(Error::new(Code::ReadOnly, "this server is read-only")),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribe()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// 0 picks a free one.
    pub port: u16,
    pub read_only: bool,
    pub allow_origin: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: PORT,
            read_only: false,
            allow_origin: vec![],
        }
    }
}

/// An exact web origin, as a browser sends it. No wildcard and no `null`.
pub fn parse_origin(s: &str) -> Result<String> {
    let o = s.to_ascii_lowercase();
    let host = o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"));
    let exact = |h: &str| h.parse::<Authority>().is_ok_and(|a| a.as_str() == h);
    match host {
        Some(h) if !h.contains(['*', '@', '/']) && exact(h) => Ok(o),
        _ => bail!("--allow-origin {s:?} is not an origin like http://localhost:5173"),
    }
}

/// What `server.json` says about the running server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub address: String,
    pub pid: u32,
    pub version: String,
    pub instance: String,
}

/// The right to be this data directory's one server, held until dropped.
pub struct Claim {
    dir: PathBuf,
    lock: File,
}

impl Claim {
    pub fn take(dir: &Path) -> Result<Claim> {
        std::fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join("server.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let who = std::fs::read_to_string(dir.join("server.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<Info>(&s).ok())
                    .map(|i| format!(": http://{} (pid {})", i.address, i.pid));
                bail!(
                    "a server already runs for {}{}",
                    dir.display(),
                    who.unwrap_or_default()
                )
            }
            Err(TryLockError::Error(e)) => return Err(e).context("server.lock"),
        }
        // Left by a server that is gone; nobody may read it as this one's.
        match std::fs::remove_file(dir.join("server.json")) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e).context("server.json"),
            _ => {}
        }
        Ok(Claim {
            dir: dir.to_path_buf(),
            lock,
        })
    }

    /// Serve `api` on loopback, on its own threads.
    pub fn serve(self, api: Arc<dyn Api>, cfg: &Config) -> Result<Running> {
        let token = token(&self.dir.join("server.token"))?;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, cfg.port))
            .with_context(|| format!("port {}", cfg.port))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let info = Info {
            address: address.to_string(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            instance: api.state().instance.clone(),
        };
        let api: Arc<dyn Api> = match cfg.read_only {
            true => Arc::new(ReadOnly(api)),
            false => api,
        };
        let app = router(Arc::new(Server {
            api,
            token,
            origins: cfg.allow_origin.clone(),
        }));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let json = self.dir.join("server.json");
        let tmp = self.dir.join("server.json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all((serde_json::to_string_pretty(&info)? + "\n").as_bytes())?;
        std::fs::rename(&tmp, &json)?;
        // Built first, so that a failure here leaves no file behind.
        let gone = json.clone();
        let thread = std::thread::spawn(move || {
            rt.block_on(async {
                for kind in [SIGTERM, SIGHUP, SIGINT] {
                    remove_on(kind, gone.clone());
                }
                accept(tokio::net::TcpListener::from_std(listener)?, app).await
            })
        });
        Ok(Running {
            address,
            json,
            token_file: self.dir.join("server.token"),
            thread: Some(thread),
            _claim: self,
        })
    }
}

/// Remove `file` when `kind` ends the process, so that no client sends the token to whatever
/// takes the port next. A signal being ignored (nohup) stays ignored.
fn remove_on(kind: i32, file: PathBuf) {
    // SAFETY: reads the disposition into a zeroed struct of its own.
    let ignored = unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(kind, std::ptr::null(), &mut old) != 0 || old.sa_sigaction == libc::SIG_IGN
    };
    if ignored {
        return;
    }
    let Ok(mut sig) = signal(SignalKind::from_raw(kind)) else {
        return;
    };
    tokio::spawn(async move {
        sig.recv().await;
        let _ = std::fs::remove_file(file);
        // SAFETY: restores the default, then ends the process as the signal would have.
        unsafe {
            libc::signal(kind, libc::SIG_DFL);
            libc::raise(kind);
        }
    });
}

/// `axum::serve` sets no timeout and no limit, so a stalled client could hold every descriptor.
async fn accept(l: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
    let slots = Arc::new(Semaphore::new(CONNECTIONS));
    loop {
        let slot = slots.clone().acquire_owned().await;
        let tcp = match l.accept().await {
            Ok((tcp, _)) => tcp,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let app = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(READ_TIMEOUT)
                .serve_connection(TokioIo::new(tcp), app)
                .await;
        });
    }
}

/// A running server; ends with the process.
pub struct Running {
    pub address: SocketAddr,
    pub token_file: PathBuf,
    json: PathBuf,
    thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    _claim: Claim,
}

impl Running {
    pub fn url(&self) -> String {
        format!("http://{}/{VERSION}", self.address)
    }

    /// Until the server stops, which only a failure makes it do.
    pub fn wait(mut self) -> Result<()> {
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(r)) => Ok(r?),
            _ => bail!("the server stopped"),
        }
    }
}

impl Drop for Claim {
    /// Explicit, since a child forked just now may still hold a copy of the descriptor.
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.json);
    }
}

/// The token in `path`, made on first use. One that others could have read is not used.
fn token(path: &Path) -> Result<String> {
    let new = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    match new {
        Ok(mut f) => {
            let mut raw = [0u8; 32];
            let made = File::open("/dev/urandom")
                .and_then(|mut r| r.read_exact(&mut raw))
                .map(|()| raw.iter().map(|b| format!("{b:02x}")).collect::<String>())
                .and_then(|t| f.write_all(t.as_bytes()).map(|()| t));
            // A half-written token must not be found by the next start.
            if made.is_err() {
                let _ = std::fs::remove_file(path);
            }
            made.context("making a token")
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let mut f = File::open(path).with_context(|| path.display().to_string())?;
            let m = f.metadata()?;
            let again = format!("delete {} to get a new token", path.display());
            // SAFETY: geteuid cannot fail and touches no memory.
            if m.mode() & 0o077 != 0 || m.uid() != unsafe { libc::geteuid() } {
                bail!("the token file is open to other users; {again}");
            }
            let mut t = String::new();
            f.read_to_string(&mut t)?;
            let t = t.trim();
            if t.len() < 32 || !t.bytes().all(|b| b.is_ascii_alphanumeric()) {
                bail!("the token file holds no usable token; {again}");
            }
            Ok(t.to_string())
        }
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

struct Server {
    api: Arc<dyn Api>,
    token: String,
    origins: Vec<String>,
}

type Ctx = S<Arc<Server>>;

fn router(s: Arc<Server>) -> Router {
    let v1 = Router::new()
        .route("/server", get(server))
        .route("/state", get(state))
        .route("/events", get(events))
        .route("/tasks/{context}/{id}", get(task))
        .route("/tasks/{context}/{id}/timeline", get(timeline))
        .route("/tasks/{context}/{id}/conversation", get(conversation))
        .route("/tasks/{context}/{id}/answer", post(answer))
        .route("/tasks/{context}/{id}/pin", post(pin))
        .route("/sessions", get(sessions))
        .route("/sessions/{id}/log", get(log))
        .route("/sessions/{id}/pane", get(pane))
        .route("/sessions/{id}/send", post(send))
        .route("/sessions/{id}/describe", post(describe))
        .route("/sessions/{id}/close", post(close))
        .route("/sessions/{id}/reopen", post(reopen))
        .route("/transcripts/{ref}", get(transcript))
        .route("/deliveries/{id}/resend", post(resend))
        .route("/refresh", post(refresh))
        .route("/snapshots", post(snapshot))
        .route("/snapshots/restore", post(restore));
    Router::new()
        .nest(&format!("/{VERSION}"), v1)
        .fallback(unknown)
        .method_not_allowed_fallback(unknown)
        .layer(middleware::from_fn_with_state(s.clone(), guard))
        .with_state(s)
}

async fn unknown(r: Request) -> Response {
    refuse(Error::not_found(format!(
        "{} {}",
        r.method(),
        r.uri().path()
    )))
}

// Admission.

/// Whether `host` is exactly the address listened on, with or without a port. No name: one may
/// resolve to an address another user can listen on.
fn loopback(host: &str) -> bool {
    let port = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    host.strip_prefix("127.0.0.1")
        .is_some_and(|r| r.is_empty() || r.strip_prefix(':').is_some_and(port))
}

/// Compares every byte whatever differs, so timing does not reveal a prefix.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |x, (a, b)| x | (a ^ b)) == 0
}

/// The header's only value; `Err` when it is repeated or not text.
fn one(h: &HeaderMap, name: HeaderName) -> Result<Option<&str>, ()> {
    let mut v = h.get_all(name).iter();
    match (v.next(), v.next()) {
        (None, _) => Ok(None),
        (Some(x), None) => x.to_str().map(Some).map_err(|_| ()),
        _ => Err(()),
    }
}

impl Server {
    /// The request's allowed origin, if it has one, or why it may not go on.
    fn admit(&self, r: &Request) -> Result<Option<String>, Error> {
        let no = |m: &str| Error::new(Code::Forbidden, m);
        let h = r.headers();
        let host = one(h, header::HOST).ok().flatten();
        let target = r.uri().authority().is_none_or(|a| loopback(a.as_str()));
        if !target || !host.is_some_and(loopback) {
            return Err(no("not a loopback host"));
        }
        match one(h, header::ORIGIN) {
            Ok(None) => Ok(None),
            Ok(Some(o)) if self.origins.iter().any(|a| a == o) => Ok(Some(o.into())),
            _ => Err(no("origin not allowed")),
        }
    }

    fn authorized(&self, h: &HeaderMap) -> bool {
        let Ok(Some(v)) = one(h, header::AUTHORIZATION) else {
            return false;
        };
        v.split_once(' ')
            .is_some_and(|(k, t)| k.eq_ignore_ascii_case("bearer") && same(t, &self.token))
    }
}

async fn guard(S(s): Ctx, r: Request, next: Next) -> Response {
    let origin = match s.admit(&r) {
        Ok(o) => o,
        Err(e) => return finish(refuse(e), None),
    };
    // A browser's preflight carries no token; it is answered only for an allowed origin.
    let preflight = r.method() == Method::OPTIONS && origin.is_some();
    let resp = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else if s.authorized(r.headers()) {
        next.run(r).await
    } else {
        refuse(Error::new(Code::Unauthorized, "a bearer token is needed"))
    };
    let mut resp = finish(resp, origin.as_deref());
    if preflight {
        let h = resp.headers_mut();
        let put = |h: &mut HeaderMap, k, v| h.insert(k, HeaderValue::from_static(v));
        put(h, header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST");
        put(
            h,
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "authorization, content-type, if-none-match",
        );
        put(h, header::ACCESS_CONTROL_MAX_AGE, "600");
    }
    resp
}

fn finish(mut r: Response, origin: Option<&str>) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(o) = origin.and_then(|o| HeaderValue::from_str(o).ok()) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, o);
        h.insert(header::VARY, HeaderValue::from_static("origin"));
        h.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("etag"),
        );
    }
    r
}

// Replies.

fn status(c: Code) -> StatusCode {
    match c {
        Code::BadRequest => StatusCode::BAD_REQUEST,
        Code::Unauthorized => StatusCode::UNAUTHORIZED,
        Code::Forbidden | Code::ReadOnly => StatusCode::FORBIDDEN,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::NeedsConfirm | Code::Gone => StatusCode::CONFLICT,
        Code::SourceFailed => StatusCode::BAD_GATEWAY,
    }
}

fn refuse(e: Error) -> Response {
    let mut r = (
        status(e.code),
        axum::Json(serde_json::json!({ "error": e })),
    )
        .into_response();
    if e.code == Code::Unauthorized {
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    r
}

fn bad(e: impl std::fmt::Display) -> Error {
    Error::new(Code::BadRequest, e.to_string())
}

fn reply<T: Serialize>(r: Result<T, Error>) -> Response {
    match r {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => refuse(e),
    }
}

/// Run something that may block, off the server's own threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Err(Error::source("internal error")))
}

async fn query(s: Arc<Server>, q: Result<Query, Error>) -> Response {
    reply(match q {
        Ok(q) => blocking(move || s.api.query(q)).await,
        Err(e) => Err(e),
    })
}

// Reads.

type Segments<T> = Result<P<T>, PathRejection>;

fn at(p: Segments<(String, String)>) -> Result<TaskRef, Error> {
    let P((context, id)) = p.map_err(bad)?;
    Ok(TaskRef { context, id })
}

fn id(p: Segments<String>) -> Result<String, Error> {
    Ok(p.map_err(bad)?.0)
}

async fn server(S(s): Ctx) -> Response {
    query(s, Ok(Query::Server)).await
}

fn etag(s: &State) -> String {
    format!("\"{}-{}\"", s.instance, s.rev)
}

async fn state(S(s): Ctx, h: HeaderMap) -> Response {
    let st = s.api.state();
    let tag = etag(&st);
    let seen = h
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().trim_start_matches("W/"))
        .any(|t| t == tag);
    let body = match seen {
        true => StatusCode::NOT_MODIFIED.into_response(),
        false => axum::Json(&*st).into_response(),
    };
    ([(header::ETAG, tag)], body).into_response()
}

async fn task(S(s): Ctx, p: Segments<(String, String)>) -> Response {
    query(s, at(p).map(Query::Task)).await
}

async fn timeline(S(s): Ctx, p: Segments<(String, String)>) -> Response {
    query(s, at(p).map(Query::Timeline)).await
}

async fn conversation(S(s): Ctx, p: Segments<(String, String)>) -> Response {
    query(s, at(p).map(Query::Conversation)).await
}

#[derive(Deserialize)]
struct Closed {
    #[serde(default)]
    closed: bool,
}

async fn sessions(S(s): Ctx, q: Result<Q<Closed>, QueryRejection>) -> Response {
    let q = q
        .map_err(bad)
        .map(|Q(c)| Query::Sessions { closed: c.closed });
    query(s, q).await
}

async fn log(S(s): Ctx, p: Segments<String>) -> Response {
    query(s, id(p).map(|session| Query::Log { session })).await
}

async fn pane(S(s): Ctx, p: Segments<String>) -> Response {
    query(s, id(p).map(|session| Query::Pane { session })).await
}

#[derive(Deserialize)]
struct Around {
    around: Option<String>,
}

async fn transcript(
    S(s): Ctx,
    p: Segments<String>,
    q: Result<Q<Around>, QueryRejection>,
) -> Response {
    let q = id(p).and_then(|reference| {
        let Q(Around { around }) = q.map_err(bad)?;
        Ok(Query::Transcript { reference, around })
    });
    query(s, q).await
}

/// Everything may have changed since a client last looked.
fn all_changed(s: &State) -> Event {
    Event {
        instance: s.instance.clone(),
        rev: s.rev,
        sections: Section::ALL.to_vec(),
    }
}

async fn events(S(s): Ctx) -> Response {
    // Subscribed before the first event is made, so nothing falls between them.
    let rx = s.api.subscribe();
    let stream = futures_util::stream::unfold((s, rx, true), |(s, mut rx, first)| async move {
        let e = if first {
            all_changed(&s.api.state())
        } else {
            match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => all_changed(&s.api.state()),
                Err(RecvError::Closed) => return None,
            }
        };
        let e = sse::Event::default().json_data(e).unwrap_or_default();
        Some((Ok::<_, Infallible>(e), (s, rx, false)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response()
}

// Commands.

/// The command in a request: its body as `T`, with what every command takes beside it.
async fn body<T: DeserializeOwned>(
    r: Request<Body>,
) -> Result<(T, Option<String>, Option<String>), Error> {
    let kind = one(r.headers(), header::CONTENT_TYPE).ok().flatten();
    let kind = kind.and_then(|v| v.split(';').next()).unwrap_or_default();
    if !kind.trim().eq_ignore_ascii_case("application/json") {
        return Err(bad("content-type must be application/json"));
    }
    let raw = tokio::time::timeout(READ_TIMEOUT, to_bytes(r.into_body(), BODY_LIMIT))
        .await
        .map_err(|_| bad("body not sent in time"))?
        .map_err(|_| bad(format!("body unreadable or over {BODY_LIMIT} bytes")))?;
    let raw = if raw.is_empty() { &b"{}"[..] } else { &raw[..] };
    let mut m: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(raw).map_err(bad)?;
    let mut text = |k: &str| match m.remove(k) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{k} must be a string"))),
    };
    let (confirm, request_id) = (text("confirm")?, text("request_id")?);
    if let Some(id) = &request_id
        && (id.is_empty() || id.len() > MAX_REQUEST_ID)
    {
        return Err(bad(format!(
            "request_id must be 1 to {MAX_REQUEST_ID} bytes"
        )));
    }
    let t = serde_json::from_value(m.into()).map_err(bad)?;
    Ok((t, confirm, request_id))
}

async fn run<T: DeserializeOwned>(
    s: Arc<Server>,
    r: Request,
    verb: impl FnOnce(T) -> Result<Verb, Error>,
) -> Response {
    let c = match body::<T>(r).await {
        Ok((t, confirm, request_id)) => verb(t).map(|verb| Command {
            verb,
            confirm,
            request_id,
        }),
        Err(e) => Err(e),
    };
    let c = match c {
        Ok(c) => c,
        Err(e) => return refuse(e),
    };
    reply(blocking(move || logged(&*s.api, c)).await)
}

/// Not in the handler, which a client that hangs up stops while its command goes on.
fn logged(api: &dyn Api, c: Command) -> Result<Reply, Error> {
    let target = c.verb.target();
    let out = api.command(c);
    let how = match &out {
        Ok(_) => "ok".into(),
        Err(e) => format!("{:?}", e.code),
    };
    crate::log::info(log_line(&target, &how));
    out
}

/// The target holds ids a client chose: one line, and no longer than it needs to be.
fn log_line(target: &str, how: &str) -> String {
    let t: String = target.chars().filter(|c| !c.is_control()).collect();
    format!("api {}: {how}", crate::util::truncate(&t, 200))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Nothing {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshBody {
    #[serde(default)]
    force: bool,
    context: Option<String>,
    task: Option<String>,
}

async fn refresh(S(s): Ctx, r: Request) -> Response {
    run(s, r, |b: RefreshBody| {
        let task = match (b.context, b.task) {
            (None, None) => None,
            (Some(c), Some(t)) => Some(TaskRef::new(c, t)),
            _ => return Err(bad("context and task go together")),
        };
        Ok(Verb::Refresh {
            force: b.force,
            task,
        })
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerBody {
    text: String,
    decision: Option<String>,
    #[serde(default)]
    ask: bool,
}

async fn answer(S(s): Ctx, p: Segments<(String, String)>, r: Request) -> Response {
    run(s, r, |b: AnswerBody| {
        Ok(Verb::Answer {
            task: at(p)?,
            text: b.text,
            decision: b.decision,
            ask: b.ask,
        })
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinBody {
    target: PinTarget,
    pinned: bool,
}

async fn pin(S(s): Ctx, p: Segments<(String, String)>, r: Request) -> Response {
    run(s, r, |b: PinBody| {
        Ok(Verb::Pin {
            task: at(p)?,
            target: b.target,
            pinned: b.pinned,
        })
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextBody {
    text: String,
}

async fn send(S(s): Ctx, p: Segments<String>, r: Request) -> Response {
    run(s, r, |b: TextBody| {
        Ok(Verb::Send {
            session: id(p)?,
            text: b.text,
        })
    })
    .await
}

async fn describe(S(s): Ctx, p: Segments<String>, r: Request) -> Response {
    run(s, r, |b: TextBody| {
        Ok(Verb::Describe {
            session: id(p)?,
            text: b.text,
        })
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseBody {
    #[serde(default)]
    keep_pane: bool,
}

async fn close(S(s): Ctx, p: Segments<String>, r: Request) -> Response {
    run(s, r, |b: CloseBody| {
        Ok(Verb::Close {
            session: id(p)?,
            keep_pane: b.keep_pane,
        })
    })
    .await
}

async fn reopen(S(s): Ctx, p: Segments<String>, r: Request) -> Response {
    run(s, r, |_: Nothing| Ok(Verb::Reopen { session: id(p)? })).await
}

async fn resend(S(s): Ctx, p: Segments<String>, r: Request) -> Response {
    run(s, r, |_: Nothing| Ok(Verb::Resend { delivery: id(p)? })).await
}

async fn snapshot(S(s): Ctx, r: Request) -> Response {
    run(s, r, |_: Nothing| Ok(Verb::SnapshotSave)).await
}

async fn restore(S(s): Ctx, r: Request) -> Response {
    run(s, r, |_: Nothing| Ok(Verb::SnapshotRestore)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::{Rig, rig, task as a_task};
    use crate::routing::tests::{ids, sess};
    use crate::store::tests::{Scratch, scratch};
    use std::net::TcpStream;
    use std::os::unix::fs::PermissionsExt;

    struct Web {
        rig: Rig,
        run: Running,
        token: String,
        dir: Scratch,
    }

    /// A response: its status, its head in lower case, and its body.
    struct Got(u16, String, String);

    impl Got {
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.2).unwrap_or_else(|e| panic!("{e}: {}", self.2))
        }
        fn code(&self) -> String {
            self.json()["error"]["code"].as_str().unwrap().into()
        }
        fn head(&self, name: &str) -> Option<&str> {
            let p = format!("{name}: ");
            self.1.lines().find_map(|l| l.strip_prefix(&p))
        }
    }

    fn web(name: &str, cfg: Config) -> Web {
        let rig = rig(&format!("web-{name}"));
        rig.session(ids("s-1", sess("/wt/a", "a:1.1", Some(11), "idle")));
        rig.load(vec![a_task("a", "/wt/a")]);
        let dir = scratch(&format!("web-{name}"));
        let cfg = Config { port: 0, ..cfg };
        let run = Claim::take(&dir.0)
            .and_then(|c| c.serve(Arc::new(rig.e.clone()), &cfg))
            .unwrap();
        let token = std::fs::read_to_string(dir.0.join("server.token")).unwrap();
        Web {
            rig,
            run,
            token,
            dir,
        }
    }

    impl Web {
        /// A request with exactly these headers.
        fn raw(&self, line: &str, headers: &[&str], body: &[u8]) -> Got {
            let mut s = TcpStream::connect(self.run.address).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut head = format!("{line} HTTP/1.1\r\nConnection: close\r\n");
            for h in headers {
                head += &format!("{h}\r\n");
            }
            if !body.is_empty() {
                head += &format!("Content-Length: {}\r\n", body.len());
            }
            s.write_all((head + "\r\n").as_bytes()).unwrap();
            // A server that refuses early may close before all of it is written.
            let _ = s.write_all(body);
            let mut out = vec![];
            let _ = s.read_to_end(&mut out);
            let out = String::from_utf8_lossy(&out).into_owned();
            let (head, body) = out.split_once("\r\n\r\n").expect(&out);
            let status = head.split(' ').nth(1).unwrap().parse().unwrap();
            Got(
                status,
                head.to_ascii_lowercase().replace("\r\n", "\n"),
                body.into(),
            )
        }

        fn auth(&self) -> String {
            format!("Authorization: Bearer {}", self.token)
        }

        /// A well-formed request, and the headers given.
        fn req(&self, line: &str, headers: &[&str], body: &str) -> Got {
            let auth = self.auth();
            let mut h = vec!["Host: 127.0.0.1", auth.as_str()];
            h.extend(headers);
            self.raw(line, &h, body.as_bytes())
        }

        fn get(&self, path: &str) -> Got {
            self.req(&format!("GET /v1{path}"), &[], "")
        }

        fn post(&self, path: &str, body: &str) -> Got {
            let h = ["Content-Type: application/json"];
            self.req(&format!("POST /v1{path}"), &h, body)
        }
    }

    const SEND: &str = r#"{"text":"hello"}"#;
    const READS: [&str; 10] = [
        "/server",
        "/state",
        "/tasks/c/a",
        "/tasks/c/a/timeline",
        "/tasks/c/a/conversation",
        "/sessions",
        "/sessions/s-1/log",
        "/sessions/s-1/pane",
        "/transcripts/x",
        "/events",
    ];
    /// Every command, with a body it accepts.
    const COMMANDS: [(&str, &str); 10] = [
        ("/refresh", "{}"),
        ("/tasks/c/a/answer", r#"{"text":"go"}"#),
        ("/tasks/c/a/pin", r#"{"target":"ticket","pinned":true}"#),
        ("/deliveries/d/resend", "{}"),
        ("/sessions/s-1/send", SEND),
        ("/sessions/s-1/describe", SEND),
        ("/sessions/s-1/close", "{}"),
        ("/sessions/s-1/reopen", "{}"),
        ("/snapshots", "{}"),
        ("/snapshots/restore", "{}"),
    ];

    #[test]
    fn every_request_needs_the_token() {
        let w = web("token", Config::default());
        let t = w.token.clone();
        let bad = [
            String::new(),
            "Authorization: Bearer ".into(),
            "Authorization: Bearer x".into(),
            format!("Authorization: {t}"),
            format!("Authorization: Basic {t}"),
            format!("Authorization: Bearer {}", &t[..t.len() - 1]),
            format!("Authorization: Bearer {t}0"),
            format!("Authorization: Bearer {t} {t}"),
            format!("Cookie: token={t}"),
            format!("Authorization: Bearer {t}\r\nAuthorization: Bearer {t}"),
        ];
        for b in &bad {
            let mut h = vec!["Host: 127.0.0.1", "Content-Type: application/json"];
            if !b.is_empty() {
                h.push(b);
            }
            for p in READS {
                let g = w.raw(&format!("GET /v1{p}"), &h, b"");
                assert_eq!((g.0, g.code().as_str()), (401, "unauthorized"), "{p} {b}");
                assert_eq!(g.head("www-authenticate"), Some("bearer"));
            }
            for (p, body) in COMMANDS {
                let g = w.raw(&format!("POST /v1{p}"), &h, body.as_bytes());
                assert_eq!(g.0, 401, "{p} {b}");
            }
            // Not even whether a path exists.
            assert_eq!(w.raw("GET /v1/nope", &h, b"").0, 401);
        }
        let h = ["Host: 127.0.0.1"];
        for q in ["token", "access_token", "authorization"] {
            let g = w.raw(&format!("GET /v1/state?{q}={t}"), &h, b"");
            assert_eq!(g.0, 401, "never from a URL");
        }
        assert_eq!(w.rig.typed(), 0);
        assert_eq!(w.get("/state").0, 200);
        let lower = format!("authorization: bearer {t}");
        assert_eq!(
            w.raw("GET /v1/state", &["Host: 127.0.0.1", &lower], b"").0,
            200
        );
    }

    #[test]
    fn only_a_loopback_host_is_answered() {
        let w = web("host", Config::default());
        let auth = w.auth();
        for bad in [
            "example.com",
            "example.com:7878",
            "localhost",
            "localhost:7878",
            "localhost.example.com",
            "[::1]:7878",
            "127.0.0.1:",
            "127.0.0.1.example.com",
            "127.0.0.1@example.com",
            "example.com@127.0.0.1",
            "127.0.0.2",
            "0.0.0.0",
            "[::]",
            "localhost:x",
            "localhost/x",
            "192.168.0.1",
            "",
        ] {
            let h = format!("Host: {bad}");
            let g = w.raw("GET /v1/state", &[&h, &auth], b"");
            assert!(g.0 == 403 || g.0 == 400, "{bad}: {}", g.0);
            assert!(!g.2.contains("instance"), "{bad}");
        }
        assert_ne!(w.raw("GET /v1/state", &[&auth], b"").0, 200, "no host");
        let two = ["Host: 127.0.0.1", "Host: example.com", &auth];
        assert_ne!(w.raw("GET /v1/state", &two, b"").0, 200);
        // The absolute form names its own host beside the header.
        let abs = "GET http://example.com/v1/state";
        assert_eq!(w.raw(abs, &["Host: 127.0.0.1", &auth], b"").0, 403);
        for ok in ["127.0.0.1", "127.0.0.1:7878"] {
            let h = format!("Host: {ok}");
            assert_eq!(w.raw("GET /v1/state", &[&h, &auth], b"").0, 200, "{ok}");
        }
    }

    #[test]
    fn a_web_page_is_refused_unless_its_origin_is_allowed() {
        let w = web("origin", Config::default());
        let own = format!("Origin: http://{}", w.run.address);
        for o in ["Origin: https://evil.example", "Origin: null", &own] {
            let g = w.req("GET /v1/state", &[o], "");
            assert_eq!((g.0, g.code().as_str()), (403, "forbidden"), "{o}");
            assert!(!g.1.contains("access-control"), "{o}");
            let h = [o, "Content-Type: application/json"];
            assert_eq!(w.req("POST /v1/sessions/s-1/send", &h, SEND).0, 403, "{o}");
            let pre = [o, "Access-Control-Request-Method: POST"];
            let g = w.req("OPTIONS /v1/sessions/s-1/send", &pre, "");
            assert_eq!(g.0, 403, "{o}");
            assert!(!g.1.contains("access-control"), "{o}");
        }
        assert_eq!(w.rig.typed(), 0);
        assert!(!w.get("/state").1.contains("access-control"));

        let cfg = Config {
            allow_origin: vec!["http://localhost:5173".into()],
            ..Default::default()
        };
        let w = web("origin2", cfg);
        for o in [
            "Origin: http://localhost:51730",
            "Origin: http://localhost",
            "Origin: https://localhost:5173",
            "Origin: http://localhost:5173/",
            "Origin: http://localhost:5173 http://evil.example",
            "Origin: http://evil.example\r\nOrigin: http://localhost:5173",
        ] {
            assert_eq!(w.req("GET /v1/state", &[o], "").0, 403, "{o}");
        }
        let o = "Origin: http://localhost:5173";
        let g = w.req("GET /v1/state", &[o], "");
        assert_eq!(g.0, 200);
        let allow = "access-control-allow-origin";
        assert_eq!(g.head(allow), Some("http://localhost:5173"));
        assert!(!g.1.contains("allow-credentials"));
        // A preflight carries no token, and tells nothing else.
        let g = w.raw("OPTIONS /v1/state", &["Host: 127.0.0.1", o], b"");
        assert_eq!((g.0, g.2.as_str()), (204, ""));
        assert_eq!(g.head(allow), Some("http://localhost:5173"));
        // An allowed origin still needs the token, and can read that it does.
        let g = w.raw("GET /v1/state", &["Host: 127.0.0.1", o], b"");
        assert_eq!((g.0, g.head(allow).is_some()), (401, true));
    }

    #[test]
    fn origins_are_exact() {
        assert_eq!(
            parse_origin("HTTP://LocalHost:5173").unwrap(),
            "http://localhost:5173"
        );
        for bad in [
            "*",
            "null",
            "",
            "http://",
            "http://*",
            "https://*.example.com",
            "localhost:5173",
            "http://localhost:5173/",
            "http://u@localhost",
            "file:///x",
            "http://a b",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_command_must_be_json_and_small() {
        let w = web("body", Config::default());
        let path = "POST /v1/sessions/s-1/send";
        for ct in [
            "",
            "Content-Type: text/plain",
            "Content-Type: application/x-www-form-urlencoded",
            "Content-Type: multipart/form-data",
            "Content-Type: application/jsonx",
            "Content-Type: text/plain; application/json",
        ] {
            let h: Vec<&str> = [ct].into_iter().filter(|c| !c.is_empty()).collect();
            let g = w.req(path, &h, SEND);
            assert_eq!((g.0, g.code().as_str()), (400, "bad_request"), "{ct}");
        }
        let long = "x".repeat(BODY_LIMIT);
        let long_id = format!(r#"{{"text":"hi","request_id":"{}"}}"#, "x".repeat(129));
        for body in [
            "",
            "{}",
            "[]",
            "null",
            "\"hello\"",
            "{",
            r#"{"text":1}"#,
            r#"{"text":"hi","txet":1}"#,
            r#"{"text":"hi","session":"s-2"}"#,
            r#"{"text":"hi","confirm":1}"#,
            r#"{"text":"hi","request_id":""}"#,
            &long_id,
            &format!(r#"{{"text":"{long}"}}"#),
        ] {
            let g = w.post("/sessions/s-1/send", body);
            assert_eq!(g.0, 400, "{}", &body[..body.len().min(60)]);
        }
        let g = w.post("/refresh", r#"{"task":"a"}"#);
        assert_eq!(g.0, 400, "a task without its context");
        assert_eq!(w.get("/sessions/s-1/send").0, 404, "not by GET");
        assert_eq!(w.req("PUT /v1/state", &[], "").code(), "not_found");
        assert_eq!(w.rig.typed(), 0);
        let h = ["Content-Type: Application/JSON; charset=utf-8"];
        assert_eq!(w.req(path, &h, SEND).0, 200);
        assert_eq!(w.rig.typed(), 1);
    }

    #[test]
    fn a_chunked_body_is_capped_too() {
        let w = web("chunked", Config::default());
        let auth = w.auth();
        let mut s = TcpStream::connect(w.run.address).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let head = format!(
            "POST /v1/sessions/s-1/send HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}\r\n\
             Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n"
        );
        s.write_all(head.as_bytes()).unwrap();
        let chunk = format!("4000\r\n{}\r\n", " ".repeat(0x4000));
        for _ in 0..5 {
            let _ = s.write_all(chunk.as_bytes());
        }
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 400"));
        assert_eq!(w.rig.typed(), 0);
    }

    /// Half-sent requests are dropped, and while they last the server still answers.
    #[test]
    fn a_stalled_client_is_dropped() {
        let w = web("stall", Config::default());
        let auth = w.auth();
        let head = format!("GET /v1/state HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}");
        let body = format!(
            "POST /v1/sessions/s-1/send HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}\r\n\
             Content-Type: application/json\r\nContent-Length: 99\r\n\r\n{{"
        );
        let stalled = [head, body].map(|text| {
            let mut s = TcpStream::connect(w.run.address).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            s.write_all(text.as_bytes()).unwrap();
            s
        });
        assert_eq!(w.get("/state").0, 200);
        for mut s in stalled {
            let mut out = vec![];
            s.read_to_end(&mut out).expect("closed by the server");
            assert!(!String::from_utf8_lossy(&out).contains("HTTP/1.1 200"));
        }
        assert_eq!(w.rig.typed(), 0);
    }

    #[test]
    fn read_only_takes_refresh_and_nothing_else() {
        let cfg = Config {
            read_only: true,
            ..Default::default()
        };
        let w = web("ro", cfg);
        assert_eq!(w.get("/server").json()["read_only"], true);
        for (p, body) in COMMANDS {
            let g = w.post(p, body);
            match p {
                "/refresh" => assert_eq!(g.0, 200, "{}", g.2),
                _ => assert_eq!((g.0, g.code().as_str()), (403, "read_only"), "{p}"),
            }
            let g = w.post(p, r#"{"confirm":"x","request_id":"r"}"#);
            assert_ne!(g.0, 409, "{p}: no prompt either");
        }
        assert_eq!(w.rig.typed(), 0);
    }

    #[test]
    fn commands_reach_the_engine_and_repeat_once() {
        let w = web("commands", Config::default());
        let body = r#"{"text":"hello","request_id":"r1"}"#;
        let a = w.post("/sessions/s-1/send", body);
        let b = w.post("/sessions/s-1/send", body);
        assert_eq!((a.0, &a.2), (200, &b.2));
        assert_eq!(w.rig.typed(), 1, "one request id types once");
        let g = w.post("/tasks/c/a/answer", r#"{"text":"go","decision":"gone"}"#);
        assert_eq!((g.0, g.code().as_str()), (409, "gone"));
        let g = w.post("/tasks/c/a/answer", r#"{"text":"go"}"#);
        assert_eq!(g.0, 200, "{}", g.2);
        assert_eq!(g.json()["delivery"]["task"], "a");
        let g = w.post("/sessions/s-1/close", r#"{"keep_pane":true}"#);
        assert_eq!((g.0, g.code().as_str()), (409, "needs_confirm"));
        let e = &g.json()["error"];
        assert!(!e["message"].as_str().unwrap().is_empty());
        let t = e["confirm"].as_str().unwrap().to_string();
        let g = w.post(
            "/sessions/s-1/close",
            &format!(r#"{{"keep_pane":true,"confirm":"{t}"}}"#),
        );
        assert_eq!(g.0, 200, "{}", g.2);
    }

    #[test]
    fn ids_from_a_client_never_become_paths() {
        let w = web("ids", Config::default());
        std::fs::write(w.dir.0.join("secret.json"), "SECRET").unwrap();
        for id in [
            "..%2F..%2Fsecret",
            "%2e%2e%2fsecret",
            "..",
            "%2Fetc%2Fpasswd",
            "s-1%00",
            "s-1%2F..%2Fs-1",
            "*",
            "%FF",
            "~",
        ] {
            for p in [
                format!("/sessions/{id}/log"),
                format!("/sessions/{id}/pane"),
                format!("/transcripts/{id}"),
                format!("/tasks/c/{id}"),
                format!("/tasks/{id}/a/timeline"),
            ] {
                let g = w.get(&p);
                assert!(matches!(g.0, 400 | 404), "{p}: {} {}", g.0, g.2);
                assert!(!g.2.contains("SECRET"), "{p}");
            }
            for p in [
                format!("/deliveries/{id}/resend"),
                format!("/sessions/{id}/close"),
                format!("/sessions/{id}/reopen"),
                format!("/sessions/{id}/send"),
                format!("/sessions/{id}/describe"),
            ] {
                let text = p.ends_with("send") && !p.ends_with("resend") || p.ends_with("describe");
                let g = w.post(&p, if text { SEND } else { "{}" });
                assert!(matches!(g.0, 400 | 404), "{p}: {} {}", g.0, g.2);
            }
        }
        assert_eq!(w.rig.typed(), 0);
        for p in READS.iter().filter(|p| **p != "/events") {
            let g = w.get(p);
            assert!(g.0 < 500, "{p}: {}", g.2);
            assert_eq!(g.head("cache-control"), Some("no-store"), "{p}");
            assert_eq!(g.head("x-content-type-options"), Some("nosniff"), "{p}");
        }
    }

    #[test]
    fn a_seen_state_is_not_sent_again() {
        let w = web("etag", Config::default());
        let g = w.get("/state");
        let tag = g.head("etag").unwrap().to_string();
        let st = g.json();
        let want = format!("\"{}-{}\"", st["instance"].as_str().unwrap(), st["rev"]);
        assert_eq!(tag, want.to_ascii_lowercase());
        let state = |tag: &str| w.req("GET /v1/state", &[&format!("If-None-Match: {tag}")], "");
        for seen in [want.clone(), format!("W/{want}"), format!("\"x\", {want}")] {
            let g = state(&seen);
            assert_eq!((g.0, g.2.as_str()), (304, ""), "{seen}");
        }
        for other in ["\"x\"".to_string(), "*x".into(), want.replace('-', "")] {
            assert_eq!(state(&other).0, 200, "{other}");
        }
        w.rig.load(vec![a_task("a", "/wt/a"), a_task("b", "/wt/b")]);
        let g = state(&want);
        assert_eq!(g.0, 200, "a change is sent");
        assert_eq!(g.json()["tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn events_name_what_changed() {
        let w = web("events", Config::default());
        let mut s = TcpStream::connect(w.run.address).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = w.auth();
        let req = format!("GET /v1/events HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}\r\n\r\n");
        s.write_all(req.as_bytes()).unwrap();
        let mut seen = String::new();
        let mut next = |seen: &mut String| -> serde_json::Value {
            let mut buf = [0u8; 4096];
            loop {
                if let Some(i) = seen.find("data: ")
                    && let Some(n) = seen[i..].find('\n')
                {
                    let v = serde_json::from_str(&seen[i + 6..i + n]).unwrap();
                    seen.drain(..i + n);
                    return v;
                }
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "closed: {seen}");
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        };
        let first = next(&mut seen);
        assert_eq!(first["sections"].as_array().unwrap().len(), 8, "{first}");
        w.rig.load(vec![a_task("a", "/wt/a"), a_task("b", "/wt/b")]);
        let e = next(&mut seen);
        assert!(e["rev"].as_u64() > first["rev"].as_u64(), "{e}");
        assert_eq!(e["instance"], first["instance"]);
        assert!(e["sections"].as_array().unwrap().contains(&"tasks".into()));
        assert!(!e.to_string().contains("/wt/b"), "never the change itself");
    }

    #[test]
    fn one_server_per_directory() {
        let w = web("lock", Config::default());
        let json = w.dir.0.join("server.json");
        let info: Info = serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(info.address, w.run.address.to_string());
        assert_eq!(info.instance, w.rig.e.state().instance);
        let e = Claim::take(&w.dir.0).err().unwrap().to_string();
        assert!(
            e.contains(&info.address) && e.contains(&info.pid.to_string()),
            "{e}"
        );
        assert!(
            json.exists(),
            "a refused start leaves the running one's file"
        );
        assert!(w.run.address.ip().is_loopback());
        for f in ["server.json", "server.lock"] {
            assert_eq!(w.dir.0.join(f).metadata().unwrap().mode() & 0o777, 0o600);
        }
        drop(w.run);
        assert!(!json.exists());
        std::fs::write(&json, "{}").unwrap();
        Claim::take(&w.dir.0).unwrap();
        assert!(!json.exists(), "a dead server's file is not kept");
    }

    #[test]
    fn the_token_is_private_and_kept() {
        let dir = scratch("token-file");
        let p = dir.0.join("server.token");
        let t = token(&p).unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(p.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(token(&p).unwrap(), t, "kept across starts");
        for mode in [0o644, 0o640, 0o620, 0o602] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(token(&p).is_err(), "{mode:o}");
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        for bad in ["", "\n", "short", &format!("{t} x")] {
            std::fs::write(&p, bad).unwrap();
            assert!(token(&p).is_err(), "{bad:?}");
        }
        std::fs::remove_file(&p).unwrap();
        assert_ne!(token(&p).unwrap(), t);
    }

    #[test]
    fn a_log_line_is_one_line() {
        let l = log_line(&format!("send x\nERROR forged\r{}", "y".repeat(999)), "ok");
        assert!(!l.contains(['\n', '\r']) && l.len() < 300, "{l}");
    }
}
