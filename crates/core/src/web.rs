//! Web UI — the app's interface, served to any browser on the LAN.
//!
//! `swiftdrop serve` binds one HTTP listener and serves one embedded HTML
//! page that drives everything the app does: **send** to another computer
//! (the real SwiftDrop protocol), **receive** from another computer, and
//! **upload** from a phone that has nothing installed. There is no second
//! process, no asset directory, no FFI, and no build step.
//!
//! ## Why hand-rolled HTTP and an embedded page
//!
//! * **It is the whole app in one binary.** The page is a `const &str`;
//!   `cargo install` is the entire deployment.
//! * **No new dependencies.** The parser is a request line, headers to
//!   `\r\n\r\n`, then `Content-Length` bytes of body. Page code is plain
//!   JavaScript; uploads use `XMLHttpRequest` because it is still the only
//!   browser API that reports upload progress.
//! * **The browser is the file picker and the prompt UI.** Pairing codes
//!   and offer summaries reach the user through a small pending-prompt
//!   registry (below) instead of a terminal.
//!
//! ## Two security postures share this port
//!
//! * **P2P controls** — `POST /api/send`, `/api/receive/start|stop`,
//!   `/api/prompt` — run the real authenticated protocol from
//!   [`crate::sessions`]: TLS 1.3, exporter-bound device authentication,
//!   SAS pairing with the trust store, and explicit offer approval
//!   (spec FR-PAIR-003/009, FR-XFER-003). Approvals are answered in the
//!   browser: a prompt registers a one-shot channel, `/api/status` polls
//!   it, and `POST /api/prompt` resolves it.
//! * **`POST /upload`** — the phone path — is deliberately unauthenticated
//!   (owner's explicit speed-first choice, spec §20's approval requirement
//!   is *not* met there). Any host that can reach the port can drop files
//!   into `dest_dir`. Nothing on that path speaks the wire protocol.
//!
//! Because they share the listener, the whole port (P2P controls included)
//! is exposed to the LAN. That is accepted for a trusted home/office
//! network — never point this at untrusted Wi-Fi.
//!
//! ## Known shortcuts (upgrade paths marked inline)
//!
//! * **No authentication on the HTTP surface** (see above). Upgrade: a
//!   one-time PIN checked in [`handle_connection`] — roughly ten lines —
//!   or terminate with TLS if this ever leaves the LAN.
//! * **`/api/receive/stop` aborts the listener task immediately.** If a
//!   transfer is mid-flight, its `.part` temp file is left behind
//!   ([`IncomingFile`] has no `Drop` cleanup — `sessions` only aborts on
//!   error). Upgrade: a flag the receive loop checks between sessions.
//! * **Staged send files are a copy.** A browser cannot reveal a picked
//!   file's local path, so "send" uploads the files into
//!   `config_dir/staging` first and the P2P sender reads them from there.
//!   The copies are deleted when the send attempt ends (success or
//!   failure); a crash before that leaves them, and startup clears the
//!   directory.
//! * **No live P2P byte counter.** `sessions` emits no progress events, so
//!   the page shows transfer *results*, not a moving bar. Uploads (which
//!   the browser can measure) do show progress and speed.
//! * **No size cap on uploads beyond the disk-space check** in
//!   `IncomingFile::create`; the name sanitizer (`paths.rs`) is what keeps
//!   a hostile filename inside the destination.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use swiftdrop_protocol::messages::TransferOffer;
use swiftdrop_transfer::paths::CollisionPolicy;
use swiftdrop_transfer::{IncomingFile, ReceiveError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use crate::sessions::{receive, send, Peer, ReceiveOptions, SendOptions, SessionPrompt};
use crate::OfferLimits;

/// Read buffer for an upload body. Sized to amortize syscall overhead on
/// fast links while keeping memory flat at one chunk regardless of file
/// size.
const READ_CHUNK: usize = 1024 * 1024;

/// Cap on request-header size. Browsers send a few hundred bytes; anything
/// near this is broken or hostile, and we stop before allocating on its
/// behalf (spec NFR-SEC-004's spirit).
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// How many activity lines the page keeps (newest first).
const MAX_EVENTS: usize = 30;

/// What [`serve`] needs. The caller resolves these (CLI flags and
/// `Settings`, or temp dirs in a test) so this module never reads
/// environment variables or user settings on its own.
pub struct WebOptions {
    /// `ip:port` to bind. Use `127.0.0.1:0` in tests for an ephemeral port.
    pub listen: String,
    /// Directory finalized files are renamed into. Created if missing.
    pub dest_dir: PathBuf,
    /// What to do when a received name collides on disk (spec FR-XFER-011).
    pub collision_policy: CollisionPolicy,
    /// Identity/trust-store directory (spec FR-PAIR-004). Also the parent of
    /// the staging directory used by the send control.
    pub config_dir: PathBuf,
    /// Display name this device presents in its `hello`.
    pub device_name: String,
    /// Port the receive control listens on when the page does not specify
    /// one. Fixed by default so a sender can type just `host`.
    pub receive_port: u16,
    /// Open the page in the local browser after binding (the app launcher
    /// passes this). Failure is ignored — the URL is printed anyway.
    pub open: bool,
}

/// Bind `opts.listen` and serve the app until killed.
///
/// Returns only if the initial bind fails; a failed request is logged and
/// the loop keeps serving (one bad client must not take the app down).
pub async fn serve(opts: WebOptions) -> std::io::Result<()> {
    std::fs::create_dir_all(&opts.dest_dir)?;
    let listener = TcpListener::bind(&opts.listen).await?;
    let local = listener.local_addr()?;
    // Print before serving: callers show this as the URL, and it must never
    // claim a port the bind did not actually take.
    eprintln!("SwiftDrop web UI on {local}");
    eprintln!("received files land in: {}", opts.dest_dir.display());
    eprintln!(
        "open http://127.0.0.1:{} on this machine, or http://<this-machine's-ip>:{} from a phone",
        local.port(),
        local.port()
    );
    if opts.open {
        open_browser(local.port());
    }
    serve_on(listener, opts).await
}

/// Best-effort launch of the local browser at the UI.
///
/// shortcut: Linux-only (`xdg-open`). Add `open`/`start` when other desktop
/// platforms are supported; until then those platforms just print the URL.
fn open_browser(port: u16) {
    let url = format!("http://127.0.0.1:{port}");
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The accept loop behind [`serve`], split out so tests can bind their own
/// ephemeral `127.0.0.1:0` listener and know its port (the same pattern
/// `sessions::receive` uses).
///
/// Never returns: the loop only exits if the listener itself errors.
pub async fn serve_on(listener: TcpListener, opts: WebOptions) -> std::io::Result<()> {
    serve_with(listener, AppState::new(opts)).await
}

async fn serve_with(listener: TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    loop {
        let (tcp, peer_addr) = listener.accept().await?;
        let state = state.clone();
        // One task per connection: the page polls status on an interval and
        // opens one connection per uploaded file, all at the same time.
        tokio::spawn(async move {
            if let Err(e) = handle_connection(tcp, state).await {
                // Local detail stays local (spec NFR-SEC-007): this names
                // the failure, never file contents.
                tracing::warn!(%peer_addr, error = %e, "http request failed");
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared state: the receive toggle and the pending-prompt registry
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the page can observe or change, shared by all connections.
///
/// Fields are copied out of [`WebOptions`] rather than borrowed so tasks
/// spawned by request handlers own everything they touch.
struct AppState {
    /// Where finalized files land (receive control and phone uploads).
    dest_dir: PathBuf,
    /// Where the send control stages browser-picked files.
    staging_dir: PathBuf,
    collision_policy: CollisionPolicy,
    config_dir: PathBuf,
    device_name: String,
    /// Default P2P listen port for the receive control.
    default_port: u16,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Monotonic prompt id; never reused, so a stale page tab cannot answer
    /// a newer prompt by guessing an id.
    next_id: u64,
    /// Prompts waiting for a browser answer, keyed by id.
    pending: HashMap<u64, Pending>,
    /// The running receive loop, if any.
    receive: Option<ReceiveHandle>,
    /// Recent activity, newest first (capped at [`MAX_EVENTS`]).
    events: VecDeque<String>,
}

struct ReceiveHandle {
    /// Bound address, shown on the page (`0.0.0.0:52915`).
    addr: String,
    task: tokio::task::JoinHandle<()>,
}

/// One unanswered question from an in-flight session.
///
/// The session task is parked on `tx`; the HTTP handler answers through it.
/// If the page never answers, `ask`'s timeout drops `tx` and the session
/// treats it as a rejection (spec FR-PAIR-006's default: silence is not
/// consent).
struct Pending {
    kind: &'static str,
    peer: String,
    detail: String,
    tx: oneshot::Sender<bool>,
}

impl AppState {
    fn new(opts: WebOptions) -> Arc<Self> {
        let staging_dir = opts.config_dir.join("staging");
        let _ = std::fs::create_dir_all(&staging_dir);
        // Staged copies exist only until the send they were staged for ends,
        // so anything still here is a crash leftover.
        if let Ok(entries) = std::fs::read_dir(&staging_dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Arc::new(Self {
            dest_dir: opts.dest_dir,
            staging_dir,
            collision_policy: opts.collision_policy,
            config_dir: opts.config_dir,
            device_name: opts.device_name,
            default_port: opts.receive_port,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Locks the shared state, ignoring poisoning: a panic elsewhere must
    /// not take the UI down with it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn event(&self, msg: String) {
        let mut inner = self.lock();
        inner.events.push_front(msg);
        inner.events.truncate(MAX_EVENTS);
    }

    /// Register a prompt and wait for the browser to answer it.
    ///
    /// `timeout_ms` matches the session's own prompt budget
    /// (`config::PAIRING_TIMEOUT_MS`); whichever fires first, the session
    /// sees a rejection — the safe default.
    async fn ask(&self, kind: &'static str, peer: &Peer, detail: String, timeout_ms: u64) -> bool {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            inner.pending.insert(
                id,
                Pending {
                    kind,
                    peer: peer.display_name.clone(),
                    detail,
                    tx,
                },
            );
            id
        };
        let answered = tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await;
        // Remove even on timeout: the page must not keep showing a question
        // whose session has already given up.
        self.lock().pending.remove(&id);
        matches!(answered, Ok(Ok(true)))
    }

    /// Answer a pending prompt. Returns false for an unknown id (already
    /// answered, timed out, or never existed).
    fn answer(&self, id: u64, accept: bool) -> bool {
        match self.lock().pending.remove(&id) {
            Some(p) => {
                let _ = p.tx.send(accept);
                true
            }
            None => false,
        }
    }

    fn receive_stop(&self) {
        // Take the handle in its own statement: a guard held through an
        // `if let` body would still be alive when `event` locks again.
        let handle = self.lock().receive.take();
        if let Some(h) = handle {
            // shortcut: immediate abort — mid-transfer leaves a .part file,
            // see the module docs. Upgrade: a stop flag checked between
            // sessions inside the receive loop.
            h.task.abort();
            self.event(format!("stopped receiving on {}", h.addr));
        }
    }

    /// The whole world the page needs, as JSON. Hand-assembled values via
    /// `serde_json` (already a dependency) so peer-supplied names are
    /// escaped correctly rather than interpolated into JSON text.
    fn status_json(&self) -> String {
        let inner = self.lock();
        let mut pending: Vec<(&u64, &Pending)> = inner.pending.iter().collect();
        pending.sort_by_key(|(id, _)| **id);
        let prompts: Vec<serde_json::Value> = pending
            .iter()
            .map(|(id, p)| {
                serde_json::json!({
                    "id": id,
                    "kind": p.kind,
                    "peer": p.peer,
                    "detail": p.detail,
                })
            })
            .collect();
        serde_json::json!({
            "receive": inner.receive.as_ref().map(|r| r.addr.clone()),
            "prompts": prompts,
            "events": inner.events.iter().collect::<Vec<_>>(),
        })
        .to_string()
    }
}

/// The [`SessionPrompt`] implementation that asks through the browser page.
///
/// Used by both the send and receive controls, so a user answering
/// "pair?" or "accept these files?" is doing exactly what the CLI's
/// `TerminalPrompt` would ask — the same session code drives both.
struct WebPrompt {
    state: Arc<AppState>,
}

#[async_trait::async_trait]
impl SessionPrompt for WebPrompt {
    async fn confirm_pairing(&self, peer: &Peer, sas: &str) -> bool {
        self.state
            .ask(
                "pairing",
                peer,
                format!("code {sas} — it must match on their screen"),
                crate::config::PAIRING_TIMEOUT_MS,
            )
            .await
    }

    async fn confirm_offer(&self, peer: &Peer, offer: &TransferOffer) -> bool {
        // Show the first few names so the user knows what is arriving; the
        // page renders this with `textContent`, so hostile names cannot
        // inject markup.
        let names: Vec<&str> = offer
            .items
            .iter()
            .take(3)
            .map(|i| i.name.as_str())
            .collect();
        let more = offer.items.len().saturating_sub(names.len());
        let mut detail = format!(
            "{} file(s), {}",
            offer.items.len(),
            bytes_text(offer.total_size_bytes)
        );
        if !names.is_empty() {
            detail.push_str(" — ");
            detail.push_str(&names.join(", "));
            if more > 0 {
                detail.push_str(&format!(" (+{more} more)"));
            }
        }
        self.state
            .ask("offer", peer, detail, crate::config::PAIRING_TIMEOUT_MS)
            .await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing
// ─────────────────────────────────────────────────────────────────────────────

/// Serve exactly one HTTP/1.1 connection.
async fn handle_connection(mut tcp: TcpStream, state: Arc<AppState>) -> std::io::Result<()> {
    let header = match read_headers(&mut tcp).await? {
        Some(h) => h,
        None => return Ok(()), // connection closed before a full request
    };

    let request_line = header.split("\r\n").next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    // Path only — drop any `?query` before routing (handlers pull the query
    // back out of `target` when they need it).
    let path = target.split('?').next().unwrap_or("");

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut tcp, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await
        }
        ("GET", "/api/status") => json(&mut tcp, 200, state.status_json()).await,
        ("POST", "/upload") => handle_upload(&mut tcp, &state, &header, target).await,
        ("POST", "/api/send") => api_send(&mut tcp, &state, target).await,
        ("POST", "/api/receive/start") => api_receive_start(&mut tcp, &state, target).await,
        ("POST", "/api/receive/stop") => {
            state.receive_stop();
            json(&mut tcp, 200, r#"{"ok":true}"#.to_string()).await
        }
        ("POST", "/api/prompt") => api_prompt(&mut tcp, &state, target).await,
        _ => respond(&mut tcp, 404, "text/plain; charset=utf-8", b"not found\n").await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// /upload — the unauthenticated phone path
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /upload?name=<enc>[&staging=1]` — one file body, streamed to disk.
///
/// `staging=1` diverts the file into the send staging directory instead of
/// the destination folder; the page uses it for files chosen to be sent on.
async fn handle_upload(
    tcp: &mut TcpStream,
    state: &AppState,
    header: &str,
    target: &str,
) -> std::io::Result<()> {
    let len = match content_length(header) {
        Some(l) => l,
        None => {
            return text(tcp, 400, "upload needs Content-Length\n").await;
        }
    };
    let name = query_param(target, "name").unwrap_or_else(|| "upload.bin".into());
    let dest = if query_param(target, "staging").as_deref() == Some("1") {
        state.staging_dir.clone()
    } else {
        state.dest_dir.clone()
    };
    match upload(tcp, &dest, state.collision_policy, &name, len).await {
        Ok(saved) => {
            // The page reads this as the per-file result line.
            text(tcp, 200, &format!("saved: {saved}\n")).await
        }
        Err(e) => text(tcp, 400, &format!("error: {e}\n")).await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// /api/* — the P2P controls
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /api/send?target=<host[:port]>&file=<staged name>…`
///
/// Resolves the target and the staged files **synchronously** so a typo or
/// a missing file comes back as a 400 the user sees immediately, then runs
/// the transfer on its own task and reports through the activity log (a
/// 10 GB send must not hold an HTTP response open for minutes).
async fn api_send(tcp: &mut TcpStream, state: &Arc<AppState>, target: &str) -> std::io::Result<()> {
    let target_arg = query_param(target, "target").unwrap_or_default();
    if target_arg.trim().is_empty() {
        return text(tcp, 400, "give the other computer's address\n").await;
    }
    // `host` or `host:port`; a bare host means the default port.
    let (host, port) = match target_arg.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h.to_string(), port),
            Err(_) => {
                return text(tcp, 400, "target port must be a number\n").await;
            }
        },
        None => (target_arg.clone(), state.default_port),
    };
    if host.is_empty() {
        return text(tcp, 400, "give the other computer's address\n").await;
    }

    let names = query_params(target, "file");
    if names.is_empty() {
        return text(tcp, 400, "no files to send\n").await;
    }
    let mut files = Vec::with_capacity(names.len());
    for name in &names {
        // Trust boundary: the page sends back names the server itself chose,
        // but any LAN host can call this endpoint, so refuse anything that
        // is not a plain staged filename.
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
        {
            return text(tcp, 400, "bad staged filename\n").await;
        }
        let path = state.staging_dir.join(name);
        if !path.is_file() {
            return text(tcp, 400, &format!("unknown staged file: {name}\n")).await;
        }
        files.push(path);
    }

    let addr = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => {
                return text(tcp, 400, &format!("could not resolve {host}:{port}\n")).await;
            }
        },
        Err(e) => {
            return text(tcp, 400, &format!("could not resolve {host}:{port}: {e}\n")).await;
        }
    };

    let count = files.len();
    let state = state.clone();
    let host_for_task = host.clone();
    tokio::spawn(async move {
        let opts = SendOptions {
            config_dir: state.config_dir.clone(),
            device_name: state.device_name.clone(),
            limits: OfferLimits::default(),
            // Never auto-accept: the whole point of the UI is that the user
            // answers (spec §20).
            auto_accept: false,
        };
        let prompt = WebPrompt {
            state: state.clone(),
        };
        match send(addr, &host_for_task, files.clone(), opts, prompt).await {
            Ok(outcome) => state.event(format!(
                "sent {} file(s), {} to {host_for_task}",
                outcome.saved_names.len(),
                bytes_text(outcome.total_bytes)
            )),
            Err(e) => state.event(format!("send to {host_for_task} failed: {e}")),
        }
        // Staged copies exist only to be sent; drop them either way. The
        // user's originals were never touched (the browser only handed us
        // bytes).
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
    });

    json(tcp, 200, format!(r#"{{"started":{count}}}"#)).await
}

/// `POST /api/receive/start[?port=<u16>]` — begin listening for peers.
///
/// The bind happens here rather than in the task so a port already in use
/// is a 400 the user sees, and the page learns the actual bound address.
async fn api_receive_start(
    tcp: &mut TcpStream,
    state: &Arc<AppState>,
    target: &str,
) -> std::io::Result<()> {
    if state.lock().receive.is_some() {
        return text(tcp, 400, "already listening\n").await;
    }
    let port = query_param(target, "port")
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(state.default_port);
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            return text(tcp, 400, &format!("cannot listen on port {port}: {e}\n")).await;
        }
    };
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| format!("0.0.0.0:{port}"));

    let opts = ReceiveOptions {
        config_dir: state.config_dir.clone(),
        device_name: state.device_name.clone(),
        dest_dir: state.dest_dir.clone(),
        collision_policy: state.collision_policy,
        limits: OfferLimits::default(),
        auto_accept: false,
    };
    let prompt = WebPrompt {
        state: Arc::clone(state),
    };
    let state2 = Arc::clone(state);
    let task = tokio::spawn(async move {
        if let Err(e) = receive(listener, opts, prompt).await {
            state2.event(format!("receiving stopped: {e}"));
        }
    });
    state.lock().receive = Some(ReceiveHandle {
        addr: local.clone(),
        task,
    });
    state.event(format!("receiving on {local}"));

    json(
        tcp,
        200,
        serde_json::json!({ "listening": local }).to_string(),
    )
    .await
}

/// `POST /api/prompt?id=<u64>&answer=accept|decline` — answer a pending
/// pairing or offer prompt (spec FR-PAIR-003, FR-XFER-003).
async fn api_prompt(
    tcp: &mut TcpStream,
    state: &Arc<AppState>,
    target: &str,
) -> std::io::Result<()> {
    let id = query_param(target, "id").and_then(|i| i.parse::<u64>().ok());
    let accept = query_param(target, "answer").as_deref() == Some("accept");
    match id {
        Some(id) if state.answer(id, accept) => json(tcp, 200, r#"{"ok":true}"#.to_string()).await,
        // Already answered, timed out, or invented by a hostile client.
        _ => text(tcp, 404, "no such prompt\n").await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request/response plumbing
// ─────────────────────────────────────────────────────────────────────────────

/// Read until the blank line that ends the header block, capped at
/// [`MAX_HEADER_BYTES`]. Returns `None` if the peer closed first. Body bytes
/// are deliberately NOT consumed here — the upload handler streams them.
async fn read_headers(tcp: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    while buf.len() < MAX_HEADER_BYTES {
        let n = tcp.read(&mut byte).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed mid-header",
                ))
            };
        }
        buf.push(byte[0]);
        // Fast path: the four-byte terminator ends the header block.
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "header too large",
    ))
}

/// `Content-Length` from a raw header block. Required for uploads:
/// `IncomingFile::create` needs the declared size up front for its
/// disk-space check, and `Content-Length` is exactly that declaration.
fn content_length(headers: &str) -> Option<u64> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse::<u64>().ok())
            .flatten()
    })
}

/// Pull one `key=value` pair out of a `?a=1&b=2` query string (first match).
fn query_param(target: &str, key: &str) -> Option<String> {
    query_params(target, key).into_iter().next()
}

/// Every value for `key` (uploads/prompts repeat keys in some callers).
///
/// Values are NOT `+`-decoded: `+` is a legal filename character and only
/// spaces arrive as `%20` from `URLSearchParams`/`encodeURIComponent`.
fn query_params(target: &str, key: &str) -> Vec<String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Byte-wise, never string-slicing: `&s[i+1..i+3]` would panic if
        // those bytes land mid-UTF-8-char (hostile input can be anything).
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Stream exactly `expected_len` body bytes into a fresh [`IncomingFile`]
/// and finalize it.
///
/// Order is what makes this safe for very large files:
///
/// 1. `IncomingFile::create` — sanitizes `name`, reserves the final
///    filename, checks free disk space, opens the temp file.
/// 2. Loop: read up to [`READ_CHUNK`] from the socket, `write_chunk`
///    (which also feeds the SHA-256 hasher). Memory stays at one chunk no
///    matter the file size.
/// 3. `finalize(None)` — exact-size check, then fsync, then atomic rename.
///    `None` skips the digest comparison because a browser has no pre-hash
///    to offer; the fsync+rename ordering is unchanged.
async fn upload(
    tcp: &mut TcpStream,
    dest: &Path,
    policy: CollisionPolicy,
    raw_name: &str,
    expected_len: u64,
) -> Result<String, UploadError> {
    let mut sink =
        IncomingFile::create(dest, raw_name, expected_len, policy).map_err(UploadError::Receive)?;

    // Abort on ANY failure before finalize: `IncomingFile` has no Drop
    // cleanup (only explicit `abort`), and a half-received upload must
    // never leave a `.part` temp behind (spec §10.4).
    let streamed = stream_body(tcp, &mut sink, expected_len).await;
    if let Err(e) = streamed {
        sink.abort();
        return Err(e);
    }

    let finalised = sink.finalize(None).map_err(UploadError::Receive)?;
    Ok(finalised.saved_name)
}

/// Read exactly `expected_len` body bytes off the socket into `sink`, one
/// [`READ_CHUNK`] buffer at a time — memory stays flat at one chunk.
async fn stream_body(
    tcp: &mut TcpStream,
    sink: &mut IncomingFile,
    expected_len: u64,
) -> Result<(), UploadError> {
    let mut remaining = expected_len;
    let mut chunk = vec![0u8; READ_CHUNK.min(expected_len.max(1) as usize)];
    while remaining > 0 {
        let want = (READ_CHUNK as u64).min(remaining) as usize;
        let n = tcp.read(&mut chunk[..want]).await?;
        if n == 0 {
            // Peer vanished mid-body: `remaining > 0` proves it was short.
            return Err(UploadError::ShortBody {
                expected: expected_len,
                got: expected_len - remaining,
            });
        }
        sink.write_chunk(&chunk[..n])
            .map_err(UploadError::Receive)?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Failures the page surfaces verbatim. These are *local* diagnostics — the
/// browser is on the same network, so unlike the P2P wire (spec
/// NFR-SEC-007) there is no peer to shield.
#[derive(Debug, thiserror::Error)]
enum UploadError {
    #[error("declared {expected} bytes but only {got} arrived")]
    ShortBody { expected: u64, got: u64 },
    /// Socket failures mid-body (client vanished, reset). The response write
    /// that follows will itself fail on a dead socket, which the caller logs
    /// and ignores.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Receive(#[from] ReceiveError),
}

/// Write a plain-text response.
async fn text(tcp: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    respond(tcp, status, "text/plain; charset=utf-8", body.as_bytes()).await
}

/// Write a JSON response.
async fn json(tcp: &mut TcpStream, status: u16, body: String) -> std::io::Result<()> {
    respond(tcp, status, "application/json", body.as_bytes()).await
}

/// Write one complete HTTP/1.1 response. `Connection: close` — browsers open
/// their own parallel connections, so keep-alive would add a state machine
/// for nothing.
async fn respond(
    tcp: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(head.as_bytes()).await?;
    tcp.write_all(body).await?;
    tcp.shutdown().await
}

/// Human-readable byte count for the activity log (IEC units, spec
/// NFR-UX-006's byte-internal/display-at-the-edge rule).
fn bytes_text(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b < KB {
        format!("{b} B")
    } else if b < MB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else if b < GB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else {
        format!("{:.2} GB", b as f64 / GB as f64)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The page
// ─────────────────────────────────────────────────────────────────────────────

/// The single-page app: three sections (send, receive, phone upload), a
/// prompt area for live pairing/offer questions, and an activity log.
///
/// Embedded so `serve` needs no asset files or build step. All dynamic
/// values (peer names, filenames, errors) are written with `textContent`,
/// never `innerHTML` — they originate from other devices.
const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SwiftDrop</title>
<style>
  :root { color-scheme: dark; }
  body { font:16px/1.5 system-ui,sans-serif; background:#0e1116; color:#e6edf3;
         max-width:680px; margin:2rem auto; padding:0 1rem; }
  h1 { font-size:1.4rem; margin:0 0 .2rem; }
  .sub { color:#8b949e; font-size:.9rem; margin-bottom:1rem; }
  nav { display:flex; gap:.4rem; margin-bottom:1rem; }
  nav button { flex:1; background:#161b22; color:#e6edf3; border:1px solid #30363d;
               border-radius:8px; padding:.5rem; font:inherit; cursor:pointer; }
  nav button.on { border-color:#58a6ff; color:#58a6ff; }
  .drop { border:2px dashed #30363d; border-radius:12px; padding:1.6rem;
          text-align:center; cursor:pointer; }
  .drop.hot { border-color:#58a6ff; background:#161b22; }
  input { background:#0d1117; color:#e6edf3; border:1px solid #30363d;
          border-radius:8px; padding:.45rem .6rem; font:inherit; }
  .file { background:#161b22; border:1px solid #21262d; border-radius:10px;
          padding:.7rem .9rem; margin:.6rem 0; }
  .row { display:flex; justify-content:space-between; align-items:center; gap:1rem;
         font-size:.92rem; }
  .name { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .meta { color:#8b949e; white-space:nowrap; }
  .bar { height:6px; background:#21262d; border-radius:4px; margin-top:.5rem; overflow:hidden; }
  .bar i { display:block; height:100%; width:0; background:#58a6ff; transition:width .1s; }
  .ok .bar i { background:#3fb950; } .bad .bar i { background:#f85149; }
  .ok .meta { color:#3fb950; } .bad .meta { color:#f85149; }
  .prompt { border-color:#d29922; }
  .act { background:#21262d; color:#e6edf3; border:1px solid #30363d;
         border-radius:6px; padding:.25rem .6rem; font:inherit; cursor:pointer; }
  #log { margin-top:1.2rem; font-size:.85rem; color:#8b949e; white-space:pre-wrap; }
  small.muted { color:#8b949e; display:block; margin-top:.6rem; }
</style>
</head>
<body>
<h1>SwiftDrop</h1>
<div class="sub">Send files to another computer on this network. Received files land in the
destination folder the server printed.</div>
<nav>
  <button id="tab-send" class="on">Send to a computer</button>
  <button id="tab-recv">Receive</button>
  <button id="tab-phone">Upload from a phone</button>
</nav>

<section id="send">
  <input type="text" id="target" placeholder="other computer: host or host:port" style="width:100%;box-sizing:border-box">
  <div id="senddrop" class="drop" style="margin-top:.8rem">Tap to choose files to send</div>
  <input id="sendpick" type="file" multiple hidden>
  <div id="sendlist"></div>
</section>

<section id="recv" hidden>
  <div class="row">
    <span>Listening: <b id="recvstate">no</b></span>
    <span>
      <input type="number" id="port" min="1" max="65535" style="width:6rem" placeholder="port">
      <button class="act" id="recvstart">Start</button>
      <button class="act" id="recvstop">Stop</button>
    </span>
  </div>
  <small class="muted">Start listening, then the other computer sends to this machine's
  address and this port (default is shown after starting).</small>
</section>

<section id="phone" hidden>
  <div id="drop" class="drop">Tap to choose files to upload to this computer</div>
  <input id="pick" type="file" multiple hidden>
  <div id="list"></div>
</section>

<div id="prompts"></div>
<div id="log"></div>

<script>
const $ = id => document.getElementById(id);
const list = $('list'), sendlist = $('sendlist'), log = $('log'), prompts = $('prompts');
const target = $('target'), port = $('port');

const tabs = [['tab-send','send'],['tab-recv','recv'],['tab-phone','phone']];
for (const [btn, sec] of tabs) {
  $(btn).onclick = () => {
    for (const [b, s] of tabs) { $(b).classList.toggle('on', b === btn); $(s).hidden = s !== sec; }
  };
}

const fmt = b => b < 1024 ? b+' B'
          : b < 1048576 ? (b/1024).toFixed(1)+' KB'
          : b < 1073741824 ? (b/1048576).toFixed(1)+' MB'
          : (b/1073741824).toFixed(2)+' GB';
const rate = bps => bps < 1048576 ? Math.round(bps/1024)+' KB/s'
            : (bps/1048576).toFixed(1)+' MB/s';

// One file upload. Resolves with the saved name; rejects with the reason.
function upload(file, container, staging) {
  const row = document.createElement('div');
  row.className = 'file';
  row.innerHTML = '<div class="row"><span class="name"></span><span class="meta">0%</span></div>'
                + '<div class="bar"><i></i></div>';
  row.querySelector('.name').textContent = file.name;
  const meta = row.querySelector('.meta'), fill = row.querySelector('.bar i');
  container.appendChild(row);
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest(), t0 = performance.now();
    xhr.open('POST', '/upload?name=' + encodeURIComponent(file.name) + (staging ? '&staging=1' : ''));
    xhr.upload.onprogress = e => {
      if (!e.lengthComputable) return;
      const pct = Math.floor(e.loaded / e.total * 100);
      fill.style.width = pct + '%';
      const secs = (performance.now() - t0) / 1000;
      meta.textContent = pct + '%  ' + rate(secs > 0 ? e.loaded / secs : 0);
    };
    xhr.onload = () => {
      if (xhr.status === 200) {
        row.classList.add('ok'); fill.style.width = '100%';
        const secs = Math.max((performance.now() - t0) / 1000, .001);
        meta.textContent = 'done  ' + rate(file.size / secs);
        resolve(xhr.responseText.trim().replace(/^saved:\s*/, ''));
      } else {
        row.classList.add('bad');
        meta.textContent = (xhr.responseText || 'failed').trim();
        reject(meta.textContent);
      }
    };
    xhr.onerror = () => { row.classList.add('bad'); meta.textContent = 'network error'; reject('network error'); };
    xhr.send(file);
  });
}

function wireDrop(zone, pick, onFiles) {
  zone.onclick = () => pick.click();
  pick.onchange = () => { onFiles([...pick.files]); pick.value = ''; };
  zone.addEventListener('dragover', ev => { ev.preventDefault(); zone.classList.add('hot'); });
  zone.addEventListener('dragleave', ev => { ev.preventDefault(); zone.classList.remove('hot'); });
  zone.addEventListener('drop', ev => {
    ev.preventDefault(); zone.classList.remove('hot');
    onFiles([...ev.dataTransfer.files]);
  });
}

// Stage the picked files locally, then ask the server to send them P2P.
async function sendPicked(files) {
  if (!files.length) return;
  if (!target.value.trim()) { appendLog('enter the other computer\'s address first'); return; }
  const names = [];
  for (const f of files) {
    try { names.push(await upload(f, sendlist, true)); }
    catch (e) { return; }              // the row already shows why
  }
  const q = names.map(n => '&file=' + encodeURIComponent(n)).join('');
  const r = await fetch('/api/send?target=' + encodeURIComponent(target.value.trim()) + q, { method: 'POST' });
  if (!r.ok) appendLog('send refused: ' + (await r.text()).trim());
  poll();
}
wireDrop($('senddrop'), $('sendpick'), sendPicked);
wireDrop($('drop'), $('pick'), files => { for (const f of files) upload(f, list, false).catch(() => {}); });

$('recvstart').onclick = async () => {
  const r = await fetch('/api/receive/start?port=' + encodeURIComponent(port.value || ''), { method: 'POST' });
  if (!r.ok) appendLog('receive: ' + (await r.text()).trim());
  poll();
};
$('recvstop').onclick = async () => { await fetch('/api/receive/stop', { method: 'POST' }); poll(); };

// Live prompts (pairing / incoming offer). Values are untrusted text.
function showPrompts(ps) {
  prompts.textContent = '';
  for (const p of ps) {
    const box = document.createElement('div');
    box.className = 'file prompt';
    const row = document.createElement('div');
    row.className = 'row';
    const label = document.createElement('span');
    label.textContent = (p.kind === 'pairing' ? 'Pair with ' : 'Accept from ') + p.peer
                      + ' — ' + p.detail;
    const actions = document.createElement('span');
    for (const [txt, ans] of [['Accept', 'accept'], ['Decline', 'decline']]) {
      const b = document.createElement('button');
      b.className = 'act'; b.textContent = txt;
      b.onclick = () => fetch('/api/prompt?id=' + p.id + '&answer=' + ans, { method: 'POST' }).then(poll);
      actions.appendChild(b);
    }
    row.appendChild(label); row.appendChild(actions); box.appendChild(row);
    prompts.appendChild(box);
  }
}

let local = [];
function appendLog(msg) { local.unshift(msg); if (local.length > 10) local.pop(); }

async function poll() {
  let s;
  try { s = await (await fetch('/api/status', { cache: 'no-store' })).json(); }
  catch (e) { return; }
  $('recvstate').textContent = s.receive || 'no';
  showPrompts(s.prompts || []);
  log.textContent = (s.events || []).concat(local).join('\n');
}
setInterval(poll, 1000);
poll();
</script>
</body>
</html>
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests for the pieces the socket tests cannot reach cheaply
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Arc<AppState> {
        let cfg = tempfile::TempDir::new().expect("config dir");
        let dest = tempfile::TempDir::new().expect("dest dir");
        // Let the dirs outlive the test body without threading them through
        // every call: the test process exits moments later.
        let (config_dir, dest_dir) = (cfg.path().to_path_buf(), dest.path().to_path_buf());
        std::mem::forget(cfg);
        std::mem::forget(dest);
        AppState::new(WebOptions {
            listen: "127.0.0.1:0".into(),
            dest_dir,
            collision_policy: CollisionPolicy::Rename,
            config_dir,
            device_name: "test".into(),
            receive_port: 52915,
            open: false,
        })
    }

    fn peer() -> Peer {
        Peer {
            device_id: "0123456789abcdef".into(),
            display_name: "other".into(),
        }
    }

    async fn first_pending(state: &AppState) -> Option<u64> {
        for _ in 0..200 {
            if let Some(id) = state.lock().pending.keys().copied().next() {
                return Some(id);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        None
    }

    /// An answer of "accept" must resolve the waiting session question as
    /// `true` — the pairing/offer approval path.
    #[tokio::test]
    async fn accept_resolves_the_waiting_prompt() {
        let state = state();
        let asker = Arc::clone(&state);
        let task = tokio::spawn(async move {
            asker
                .ask("pairing", &peer(), "code 123456".into(), 5_000)
                .await
        });
        let id = first_pending(&state).await.expect("prompt must register");
        assert!(
            state.answer(id, true),
            "answering a live prompt must succeed"
        );
        assert!(task.await.unwrap(), "accept must reach the session as true");
    }

    /// "Decline" must reach the session as `false` (spec FR-PAIR-006).
    #[tokio::test]
    async fn decline_resolves_the_waiting_prompt_as_false() {
        let state = state();
        let asker = Arc::clone(&state);
        let task = tokio::spawn(async move {
            asker
                .ask("offer", &peer(), "1 file(s), 1.0 MB".into(), 5_000)
                .await
        });
        let id = first_pending(&state).await.expect("prompt must register");
        assert!(state.answer(id, false));
        assert!(!task.await.unwrap());
    }

    /// No browser answer must time out into a rejection, not hang: an
    /// unattended listener must never block a session forever.
    #[tokio::test]
    async fn unanswered_prompt_times_out_to_rejection() {
        let state = state();
        let accepted = state
            .ask("pairing", &peer(), "code 000000".into(), 30)
            .await;
        assert!(!accepted);
        assert!(
            state.lock().pending.is_empty(),
            "a timed-out prompt must be forgotten"
        );
    }

    /// Answering an id that no longer exists (or never did) must fail rather
    /// than resurrect anything.
    #[test]
    fn unknown_prompt_id_is_rejected() {
        let state = state();
        assert!(!state.answer(999, true));
    }

    #[test]
    fn bytes_are_formatted_in_iec_units() {
        assert_eq!(bytes_text(512), "512 B");
        assert_eq!(bytes_text(2048), "2.0 KB");
        assert_eq!(bytes_text(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(bytes_text(5 * 1024 * 1024 * 1024), "5.00 GB");
    }
}
