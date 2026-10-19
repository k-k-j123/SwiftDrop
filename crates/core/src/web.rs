//! Web upload bridge — a phone-side client with **no app to install**.
//!
//! A browser cannot open raw TCP or speak SwiftDrop's wire protocol (TLS
//! 1.3 + custom framing + exporter-bound auth), so a phone can never be a
//! true SwiftDrop peer through a web page. This module is the deliberate
//! pragmatic substitute: a tiny HTTP server the phone talks to over plain
//! HTTP on the LAN. It exists so "transfer from phone to PC" works today,
//! without waiting for the Flutter client (spec §7.3 / milestone M3).
//!
//! The design priority here is **throughput on large files**, explicitly
//! not security or protocol purity. What that means concretely:
//!
//! * **Streamed, not buffered.** The request body is read off the socket
//!   in [`READ_CHUNK`] chunks and written straight into
//!   [`swiftdrop_transfer::IncomingFile`], which persists via temp-file →
//!   hash → fsync → atomic rename (`file_sink.rs`). A 10 GB file never
//!   sits in memory, and every byte that reaches disk is durable before
//!   the final rename (spec NFR-REL-001's ordering, reused unchanged).
//! * **No new dependencies.** HTTP/1.1 here is a few dozen lines — request
//!   line, headers to `\r\n\r\n`, then `Content-Length` bytes of body.
//!   Pulling in `axum`/`hyper` to parse one POST would be far more code
//!   to carry than writing the parser.
//! * **The browser parallelizes for free.** Each file is its own `fetch`,
//!   so a phone uploading five files opens several connections at once and
//!   we serve each on its own task. No connection pooling, no tuning.
//!
//! ## Known shortcuts (upgrade paths marked inline)
//!
//! * **No authentication at all.** Any host that can reach the port can
//!   upload into `dest_dir`. The owner accepted this explicitly for speed
//!   (spec §20's approval requirement is *not* met by this path). Upgrade:
//!   gate `handle_upload` on a one-time PIN in a header — roughly ten
//!   lines — or terminate with TLS if it ever leaves the LAN.
//! * **No size cap.** `IncomingFile::create` still checks free disk space
//!   (`fs2::available_space`) and refuses with `InsufficientStorage`, so
//!   we cannot fill the volume, but a hostile client can pick any name the
//!   sanitizer allows. The sanitizer (`paths.rs`) is the only thing
//!   standing between a `../../` filename and the destination directory —
//!   and it is load-bearing, not decorative.
//! * **No resume, pause, cancel, or progress protocol.** The owner
//!   ruled these out of scope. The HTML page shows per-file progress
//!   computed from `Content-Length` vs bytes the browser has pushed, which
//!   is display-only and touches nothing server-side.
//! * **One request per connection** (`Connection: close`). Browsers open
//!   their own parallel connections, so keep-alive would buy nothing while
//!   adding a state machine.

use std::path::{Path, PathBuf};
use swiftdrop_transfer::paths::CollisionPolicy;
use swiftdrop_transfer::{IncomingFile, ReceiveError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Read buffer for the upload body. Sized to amortize syscall overhead on
/// fast links: at LAN speeds (~100–800 Mbps) a 1 MiB chunk means a few
/// hundred `write_all` calls per gigabyte — negligible — while keeping
/// memory flat regardless of file size.
const READ_CHUNK: usize = 1024 * 1024;

/// Cap on request-header size. Browsers send a few hundred bytes; anything
/// near this is either broken or hostile, and we stop reading before
/// allocating on its behalf (spec NFR-SEC-004's spirit: never allocate
/// proportional to an untrusted length).
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// What [`serve`] needs. The caller resolves these (CLI flags, or an
/// ephemeral port + temp dir in a test) so this module never reads
/// environment variables or user settings on its own.
pub struct WebOptions {
    /// `ip:port` to bind. Use `0.0.0.0:0` in tests for an ephemeral port.
    pub listen: String,
    /// Directory finalized files are renamed into. Created if missing.
    pub dest_dir: PathBuf,
    /// What to do when a received name collides on disk (spec FR-XFER-013).
    pub collision_policy: CollisionPolicy,
}

/// Default destination: the same folder the P2P `receive` path uses, so
/// files from either route land in one place.
pub fn default_dest() -> PathBuf {
    crate::config::default_dest_dir()
}

/// Bind `opts.listen`, serve uploads until killed, saving into
/// `opts.dest_dir`.
///
/// Returns only if the initial bind fails; a failed single upload is
/// logged and the loop keeps serving (one bad phone must not take the
/// server down).
///
/// `dest_dir` is created up front so the very first upload never races
/// directory creation.
pub async fn serve(opts: WebOptions) -> std::io::Result<()> {
    std::fs::create_dir_all(&opts.dest_dir)?;
    let listener = TcpListener::bind(&opts.listen).await?;
    let local = listener.local_addr()?;
    // Print immediately: callers show this as the URL to open, and it must
    // never claim a port the bind didn't actually take.
    eprintln!("web upload listening on {local}");
    eprintln!("dest: {}", opts.dest_dir.display());
    eprintln!(
        "open http://<this-machine's-ip>:{} from your phone",
        local.port()
    );
    serve_on(listener, opts.dest_dir, opts.collision_policy).await
}

/// The accept loop behind [`serve`], split out so tests can bind their own
/// ephemeral `127.0.0.1:0` listener and know its port (the same pattern
/// `sessions::receive` uses).
///
/// Never returns: the loop only exits if the listener itself errors.
pub async fn serve_on(
    listener: TcpListener,
    dest_dir: PathBuf,
    collision_policy: CollisionPolicy,
) -> std::io::Result<()> {
    loop {
        let (tcp, peer_addr) = listener.accept().await?;
        let dest = dest_dir.clone();
        let policy = collision_policy;
        // One task per connection: the browser opens several at once for a
        // multi-file pick, and each must progress independently.
        tokio::spawn(async move {
            if let Err(e) = handle_connection(tcp, dest, policy).await {
                // Local detail stays local (spec NFR-SEC-007): this log
                // names the failure, never file contents or full paths of
                // the sender.
                tracing::warn!(%peer_addr, error = %e, "upload failed");
            }
        });
    }
}

/// Serve exactly one HTTP/1.1 connection: `GET /` (the upload page) or
/// `POST /upload` (one file body). Anything else gets a short error page.
async fn handle_connection(
    mut tcp: TcpStream,
    dest: PathBuf,
    policy: CollisionPolicy,
) -> std::io::Result<()> {
    let header = match read_headers(&mut tcp).await? {
        Some(h) => h,
        None => return Ok(()), // connection closed before a full request
    };

    let request_line = header.split("\r\n").next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    // Path only — drop any `?query` before routing (the upload handler
    // pulls its own query back out of `target` when it needs it).
    let path = target.split('?').next().unwrap_or("");

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut tcp, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await
        }
        ("POST", "/upload") => {
            let len = content_length(&header).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "upload needs Content-Length",
                )
            })?;
            let name = query_param(target, "name").unwrap_or_else(|| "upload.bin".into());
            match upload(&mut tcp, &dest, policy, &name, len).await {
                Ok(saved) => {
                    let body = format!("saved: {saved}\n");
                    respond(&mut tcp, 200, "text/plain; charset=utf-8", body.as_bytes()).await
                }
                Err(e) => {
                    // The page reads this as the per-file error message.
                    let body = format!("error: {e}\n");
                    respond(&mut tcp, 400, "text/plain; charset=utf-8", body.as_bytes()).await
                }
            }
        }
        _ => respond(&mut tcp, 404, "text/plain; charset=utf-8", b"not found\n").await,
    }
}

/// Read until the blank line that ends the header block, capped at
/// [`MAX_HEADER_BYTES`]. Returns `None` if the peer closed first.
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
        // Fast path: the four-byte terminator ends the header block. Body
        // bytes are deliberately NOT consumed here — the caller streams
        // them straight to disk, so nothing sits in this buffer.
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

/// Pull one `key=value` pair out of a `?a=1&b=2` query string, decoding
/// the `%XX` escapes a browser sends for non-ASCII filenames. Values are
/// NOT `+`-decoded: `+` is a legal filename character and only spaces
/// arrive as `%20` from `URLSearchParams`.
fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?').map(|(_, q)| q)?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
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
/// 3. `finalize(None)` — exact-size check, then fsync, then atomic
///    rename. `None` skips the digest comparison because a browser has no
///    pre-hash to offer; the fsync+rename ordering is unchanged.
///
/// Any error before `finalize` triggers an explicit `abort()` (see below)
/// — a half-written file is never visible under the final name (spec §10.4).
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
/// [`READ_CHUNK`] buffer at a time — memory stays flat at one chunk no
/// matter the file size.
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

/// Failures the upload page surfaces verbatim to the phone. These are
/// *local* diagnostics — the browser is on the same machine's network, so
/// unlike the P2P wire (spec NFR-SEC-007) there is no peer to shield.
#[derive(Debug, thiserror::Error)]
enum UploadError {
    #[error("declared {expected} bytes but only {got} arrived")]
    ShortBody { expected: u64, got: u64 },
    /// Socket failures mid-body (client vanished, reset). Surfaced like
    /// any other upload failure — the response write that follows will
    /// itself fail on a dead socket, which the caller logs and ignores.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Receive(#[from] ReceiveError),
}

/// Write one complete HTTP/1.1 response. `Connection: close` — see the
/// module docs for why keep-alive buys nothing here.
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
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(head.as_bytes()).await?;
    tcp.write_all(body).await?;
    tcp.shutdown().await
}

// ─────────────────────────────────────────────────────────────────────────────
// The page
// ─────────────────────────────────────────────────────────────────────────────

/// The single-page upload UI, embedded so `serve` needs no asset files or
/// build step. Kept intentionally small: pick files, show per-file
/// progress and speed, show what was saved.
///
/// The speed figure is computed in the browser from bytes pushed vs
/// elapsed — display only, never a server round-trip.
const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>SwiftDrop</title>
<style>
  :root { color-scheme: dark; }
  body { font: 16px/1.5 system-ui, sans-serif; background:#0e1116; color:#e6edf3;
         max-width: 640px; margin: 2rem auto; padding: 0 1rem; }
  h1 { font-size: 1.4rem; margin-bottom: .2rem; }
  .sub { color:#8b949e; font-size:.9rem; margin-bottom:1.4rem; }
  #drop { border:2px dashed #30363d; border-radius:12px; padding:2rem; text-align:center;
          cursor:pointer; transition:border-color .15s, background .15s; }
  #drop.hot { border-color:#58a6ff; background:#161b22; }
  #pick { display:none; }
  .file { background:#161b22; border:1px solid #21262d; border-radius:10px;
          padding:.7rem .9rem; margin:.6rem 0; }
  .row { display:flex; justify-content:space-between; gap:1rem; font-size:.92rem; }
  .name { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .meta { color:#8b949e; white-space:nowrap; }
  .bar { height:6px; background:#21262d; border-radius:4px; margin-top:.5rem; overflow:hidden; }
  .bar i { display:block; height:100%; width:0; background:#58a6ff; transition:width .1s; }
  .ok  .bar i { background:#3fb950; }
  .bad  .bar i { background:#f85149; }
  .bad  .meta { color:#f85149; }
  .ok   .meta { color:#3fb950; }
  #log { margin-top:1.4rem; font-size:.9rem; color:#8b949e; white-space:pre-wrap; }
</style>
</head>
<body>
<h1>SwiftDrop</h1>
<div class="sub">Send files to this computer. They land in the destination folder shown by the server.</div>
<div id="drop">Tap to choose files<span id="hot" style="display:none"> — or drop them here</span><br>
<small style="color:#8b949e">large files are fine</small></div>
<input id="pick" type="file" multiple>
<div id="list"></div>
<div id="log"></div>
<script>
const drop = document.getElementById('drop');
const pick = document.getElementById('pick');
const list = document.getElementById('list');
const log  = document.getElementById('log');
if (location.protocol.startsWith('http')) document.getElementById('hot').style.display='inline';

drop.onclick = () => pick.click();
pick.onchange = () => { for (const f of pick.files) send(f); pick.value=''; };
['dragover','dragenter'].forEach(e => drop.addEventListener(e, ev => {
  ev.preventDefault(); drop.classList.add('hot');
}));
['dragleave','drop'].forEach(e => drop.addEventListener(e, ev => {
  ev.preventDefault(); drop.classList.remove('hot');
}));
drop.addEventListener('drop', ev => {
  for (const f of ev.dataTransfer.files) send(f);
});

const fmt = b => b < 1024 ? b+' B'
          : b < 1048576 ? (b/1024).toFixed(1)+' KB'
          : b < 1073741824 ? (b/1048576).toFixed(1)+' MB'
          : (b/1073741824).toFixed(2)+' GB';
const rate = bps => bps < 1048576 ? (bps/1024).toFixed(0)+' KB/s'
            : (bps/1048576).toFixed(1)+' MB/s';

function send(file) {
  const row = document.createElement('div');
  row.className = 'file';
  row.innerHTML = '<div class="row"><span class="name"></span><span class="meta">0%</span></div>'
                + '<div class="bar"><i></i></div>';
  row.querySelector('.name').textContent = file.name;
  const meta = row.querySelector('.meta');
  const fill = row.querySelector('.bar i');
  list.appendChild(row);

  // XHR, not fetch: upload.onprogress is the one thing browsers still do
  // only on XHR, and live per-file speed is the point of this page.
  const xhr = new XMLHttpRequest();
  const t0 = performance.now();
  xhr.open('POST', '/upload?name=' + encodeURIComponent(file.name));
  xhr.upload.onprogress = e => {
    if (!e.lengthComputable) return;
    const pct = Math.floor(e.loaded / e.total * 100);
    fill.style.width = pct + '%';
    const secs = (performance.now() - t0) / 1000;
    const bps = secs > 0 ? e.loaded / secs : 0;
    meta.textContent = pct + '%  ' + rate(bps);
  };
  xhr.onload = () => {
    if (xhr.status === 200) {
      row.classList.add('ok');
      fill.style.width = '100%';
      const secs = Math.max((performance.now() - t0) / 1000, 0.001);
      meta.textContent = 'done  ' + rate(file.size / secs);
      log.textContent = xhr.responseText.trim() + '\n' + log.textContent;
    } else {
      row.classList.add('bad');
      meta.textContent = (xhr.responseText || 'failed').trim();
    }
  };
  xhr.onerror = () => { row.classList.add('bad'); meta.textContent = 'network error'; };
  xhr.send(file);
}
</script>
</body>
</html>
"#;

// Tests live in `crates/core/tests/web_upload.rs` — they exercise the real
// socket path end to end rather than these helpers in isolation.
