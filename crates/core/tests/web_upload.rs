//! End-to-end tests for the web UI (`swiftdrop_core::web`).
//!
//! These exercise the real socket path a browser hits: raw HTTP/1.1 over
//! `127.0.0.1:0` (ephemeral port, hermetic — no fixed ports, no network
//! access, matching the rest of the workspace's tests).
//!
//! Covered:
//! * `GET /` serves the page, with the send/receive controls and the
//!   upload target in it (a phone and a desktop both see useful content);
//! * a multi-chunk upload lands byte-identical (streams, never buffers);
//! * percent-encoded unicode filenames decode and sanitize correctly;
//! * a body shorter than its declared `Content-Length` fails cleanly and
//!   leaves **no temp file** — where half-received data would leak;
//! * hostile filenames never escape the destination;
//! * `/api/status` reports a truthful idle state;
//! * the receive control starts and stops a real P2P listener;
//! * `/api/send` refuses unknown staged files and bad targets;
//! * **the whole feature end to end**: two servers exchange a file through
//!   `/api/receive/start` + `/api/send` with the pairing and offer prompts
//!   answered over `/api/prompt` — the exact sequence the page performs.
//!
//! Not covered (needs a fault seam): concurrent multi-connection uploads —
//! the browser's parallelism is plain tokio task-per-connection.

use std::time::Duration;

use swiftdrop_core::web::{serve_on, WebOptions};
use swiftdrop_core::CollisionPolicy;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One server under test. Each gets its own dest and config dirs, so a
/// sender/receiver pair has two distinct device identities (which is what
/// makes the pairing prompt appear at all).
struct Server {
    addr: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    dest: TempDir,
    config: TempDir,
}

impl Server {
    async fn start() -> Self {
        let dest = TempDir::new().expect("dest dir");
        let config = TempDir::new().expect("config dir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let opts = WebOptions {
            listen: addr.to_string(),
            dest_dir: dest.path().to_path_buf(),
            collision_policy: CollisionPolicy::Rename,
            config_dir: config.path().to_path_buf(),
            device_name: "test-peer".into(),
            // 0 lets `/api/receive/start` pick an ephemeral P2P port.
            receive_port: 0,
            open: false,
        };
        // serve_on never returns on success; the JoinHandle exists only so
        // tests can abort the loop at teardown instead of leaking a task.
        let handle = tokio::spawn(async move {
            let _ = serve_on(listener, opts).await;
        });
        Self {
            addr,
            handle,
            dest,
            config,
        }
    }
}

/// Send one raw HTTP/1.1 request and return the full response bytes.
///
/// Deliberately hand-rolled (no client crate): the server under test is
/// itself hand-rolled HTTP, so a crate's request builder would only hide
/// the exact bytes we want to control — like declaring a `Content-Length`
/// we then under-deliver.
async fn request(addr: std::net::SocketAddr, raw: Vec<u8>) -> String {
    let mut tcp = TcpStream::connect(addr).await.expect("connect");
    tcp.write_all(&raw).await.expect("write");
    // Signal "no more request bytes" so the server's header/body reads
    // finish even when we intentionally under-send the body.
    tcp.shutdown().await.expect("shutdown write");
    let mut resp = Vec::new();
    tcp.read_to_end(&mut resp).await.expect("read response");
    String::from_utf8_lossy(&resp).into_owned()
}

/// Build a `POST /upload` request: headers + exactly `payload` bytes.
fn post_upload(name_encoded: &str, declared_len: u64, payload: &[u8]) -> Vec<u8> {
    let mut req = format!(
        "POST /upload?name={name_encoded} HTTP/1.1\r\nHost: test\r\nContent-Length: {declared_len}\r\n\r\n"
    )
    .into_bytes();
    req.extend_from_slice(payload);
    req
}

/// Build a bodyless request (the `/api/*` controls take everything in the
/// query string).
fn http(method: &str, path: &str) -> Vec<u8> {
    format!("{method} {path} HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\r\n").into_bytes()
}

fn get(path: &str) -> Vec<u8> {
    http("GET", path)
}

fn post(path: &str) -> Vec<u8> {
    http("POST", path)
}

fn status_of(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("HTTP status line")
}

/// Everything after the response headers.
fn body_of(resp: &str) -> String {
    resp.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default()
}

/// Fetch and parse `/api/status`.
async fn status(server: &Server) -> serde_json::Value {
    let resp = request(server.addr, get("/api/status")).await;
    assert_eq!(status_of(&resp), 200, "status must succeed: {resp}");
    serde_json::from_str(&body_of(&resp)).expect("status must be JSON")
}

/// Answer every pending prompt with "accept" — the page's Accept button.
async fn accept_prompts(server: &Server) {
    let st = status(server).await;
    for p in st["prompts"].as_array().into_iter().flatten() {
        if let Some(id) = p["id"].as_u64() {
            let resp = request(
                server.addr,
                post(&format!("/api/prompt?id={id}&answer=accept")),
            )
            .await;
            assert_eq!(status_of(&resp), 200, "answering must succeed: {resp}");
        }
    }
}

/// `GET /` must return the app page — the only thing either a phone or a
/// desktop user sees first, so a broken response here breaks everything.
#[tokio::test]
async fn serves_the_app_page() {
    let server = Server::start().await;
    let resp = request(server.addr, get("/")).await;

    assert_eq!(status_of(&resp), 200);
    assert!(resp.contains("SwiftDrop"), "page must identify the app");
    assert!(
        resp.contains("/upload"),
        "page must contain the upload target"
    );
    assert!(
        resp.contains("/api/send") && resp.contains("/api/receive/start"),
        "page must contain the P2P controls"
    );
    server.handle.abort();
}

/// The core speed claim: a file many `READ_CHUNK`s long is streamed to
/// disk byte-for-byte. 16 MiB crosses 16 chunk boundaries — enough to
/// catch off-by-one reads, lost tail bytes, or partial-chunk writes.
#[tokio::test]
async fn large_upload_lands_byte_identical() {
    let server = Server::start().await;

    // Deterministic pattern larger than one 1 MiB read chunk.
    let payload: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let resp = request(
        server.addr,
        post_upload("big.bin", payload.len() as u64, &payload),
    )
    .await;

    assert_eq!(status_of(&resp), 200, "upload must succeed: {resp}");
    assert!(
        resp.contains("saved: big.bin"),
        "must report the saved name"
    );

    let landed = std::fs::read(server.dest.path().join("big.bin")).expect("landed file");
    assert_eq!(landed.len(), payload.len(), "size must match exactly");
    assert_eq!(landed, payload, "content must be byte-identical");

    server.handle.abort();
}

/// Filenames arrive percent-encoded from `encodeURIComponent`; unicode
/// must survive the round trip (same class of input the P2P path proves).
#[tokio::test]
async fn percent_encoded_unicode_name_is_decoded() {
    let server = Server::start().await;

    // "héllo ✅.txt" as a browser would encode it.
    let resp = request(
        server.addr,
        post_upload("h%C3%A9llo%20%E2%9C%85.txt", 7, b"payload"),
    )
    .await;

    assert_eq!(status_of(&resp), 200, "upload must succeed: {resp}");
    assert!(
        resp.contains("saved: héllo ✅.txt"),
        "saved name must be decoded unicode, got: {resp}"
    );

    let landed = std::fs::read(server.dest.path().join("héllo ✅.txt")).expect("landed file");
    assert_eq!(landed, b"payload");

    server.handle.abort();
}

/// Declaring 1000 bytes but sending 100 then closing must fail with 400
/// and leave the dest dir **empty** — proving the abort path removed the
/// temp file (`IncomingFile` has no Drop cleanup; `upload()` must call
/// `abort()` explicitly, and this is the test that would catch it not).
#[tokio::test]
async fn short_body_fails_cleanly_and_leaves_no_temp_file() {
    let server = Server::start().await;

    let resp = request(server.addr, post_upload("partial.bin", 1000, &[0u8; 100])).await;

    assert_eq!(status_of(&resp), 400, "short body must be rejected: {resp}");
    assert!(
        resp.contains("declared 1000"),
        "error must name the shortfall, got: {resp}"
    );

    let leftovers: Vec<_> = std::fs::read_dir(server.dest.path())
        .expect("read dest dir")
        .collect();
    assert!(
        leftovers.is_empty(),
        "no file — temp or final — may survive a failed upload"
    );

    server.handle.abort();
}

/// Names that reach the sanitizer are hostile input (AGENTS.md; spec
/// §10.3). Two shapes, both proving containment in `dest`:
///
/// * a bare `".."` is refused outright (400, nothing written);
/// * a traversal path is **stripped to its final component** and lands
///   inside `dest` — `../../etc/passwd` becomes `passwd` *here*, never a
///   write to anyone's real `/etc`.
#[tokio::test]
async fn hostile_names_never_escape_dest_dir() {
    let server = Server::start().await;

    // a) ".." alone → refused before the body is touched.
    let resp = request(server.addr, post_upload("%2E%2E", 4, b"body")).await;
    assert_eq!(status_of(&resp), 400, "bare .. must be rejected: {resp}");
    assert!(
        std::fs::read_dir(server.dest.path())
            .unwrap()
            .next()
            .is_none(),
        "nothing may be written for a rejected name"
    );

    // b) a traversal attempt → stripped to the final component.
    let resp = request(
        server.addr,
        post_upload("..%2F..%2Fetc%2Fpasswd", 4, b"body"),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "stripped name must upload: {resp}");
    assert!(
        resp.contains("saved: passwd"),
        "only the final component may survive: {resp}"
    );

    // It landed inside dest (a temp dir) — containment, byte-checked.
    let landed = std::fs::read(server.dest.path().join("passwd")).expect("contained file");
    assert_eq!(landed, b"body");

    server.handle.abort();
}

/// A freshly started app must report an honest idle state: not listening,
/// no prompts, no invented activity.
#[tokio::test]
async fn status_reports_idle_state() {
    let server = Server::start().await;
    let st = status(&server).await;

    assert!(st["receive"].is_null(), "not listening yet: {st}");
    assert_eq!(st["prompts"].as_array().map(Vec::len), Some(0));
    assert_eq!(st["events"].as_array().map(Vec::len), Some(0));

    server.handle.abort();
}

/// The receive control must actually bind a listener, report the real
/// bound address, and release it on stop.
#[tokio::test]
async fn receive_can_be_started_and_stopped() {
    let server = Server::start().await;

    let resp = request(server.addr, post("/api/receive/start?port=0")).await;
    assert_eq!(status_of(&resp), 200, "start must succeed: {resp}");
    let listening = serde_json::from_str::<serde_json::Value>(&body_of(&resp)).unwrap()
        ["listening"]
        .as_str()
        .expect("start must report the bound address")
        .to_string();
    assert!(listening.starts_with("0.0.0.0:"), "got {listening}");

    let st = status(&server).await;
    assert_eq!(st["receive"].as_str(), Some(listening.as_str()));

    // Starting twice must be refused rather than leaking a second loop.
    let resp = request(server.addr, post("/api/receive/start?port=0")).await;
    assert_eq!(
        status_of(&resp),
        400,
        "double start must be refused: {resp}"
    );

    let resp = request(server.addr, post("/api/receive/stop")).await;
    assert_eq!(status_of(&resp), 200, "stop must succeed: {resp}");
    assert!(status(&server).await["receive"].is_null());

    server.handle.abort();
}

/// The send control's trust boundary: a filename that was never staged (or
/// that tries to escape the staging directory) is refused before any
/// connection is attempted, and a bad target is refused too.
#[tokio::test]
async fn send_refuses_unknown_files_and_bad_targets() {
    let server = Server::start().await;

    let resp = request(
        server.addr,
        post("/api/send?target=127.0.0.1:1&file=nope.bin"),
    )
    .await;
    assert_eq!(
        status_of(&resp),
        400,
        "unknown file must be refused: {resp}"
    );
    assert!(resp.contains("unknown staged file"), "got: {resp}");

    let resp = request(
        server.addr,
        post("/api/send?target=127.0.0.1:1&file=..%2Fescape.bin"),
    )
    .await;
    assert_eq!(status_of(&resp), 400, "traversal must be refused: {resp}");

    let resp = request(server.addr, post("/api/send?file=x.bin")).await;
    assert_eq!(
        status_of(&resp),
        400,
        "missing target must be refused: {resp}"
    );

    // Answering a prompt that does not exist must not pretend to work.
    let resp = request(server.addr, post("/api/prompt?id=424242&answer=accept")).await;
    assert_eq!(status_of(&resp), 404, "unknown prompt must 404: {resp}");

    server.handle.abort();
}

/// **The whole feature**: two apps on localhost, one told to receive and
/// one told to send, with every pairing and offer prompt answered through
/// `/api/prompt` — exactly the calls the page makes. The file must arrive
/// byte-identical and the sender's staged copy must be cleaned up.
#[tokio::test(flavor = "multi_thread")]
async fn send_and_receive_through_the_web_api() {
    let recv = Server::start().await;
    let send = Server::start().await;

    // Receiver: listen for peers on an ephemeral P2P port.
    let resp = request(recv.addr, post("/api/receive/start?port=0")).await;
    assert_eq!(status_of(&resp), 200, "receive start must succeed: {resp}");
    let listening = serde_json::from_str::<serde_json::Value>(&body_of(&resp)).unwrap()
        ["listening"]
        .as_str()
        .expect("bound address")
        .to_string();
    let p2p_port = listening.rsplit(':').next().expect("port").to_string();

    // Sender: stage a file the way the page does, then ask for the send.
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let staged = send.config.path().join("staging").join("hello.bin");
    std::fs::write(&staged, &payload).expect("stage file");
    let resp = request(
        send.addr,
        post(&format!(
            "/api/send?target=127.0.0.1:{p2p_port}&file=hello.bin"
        )),
    )
    .await;
    assert_eq!(status_of(&resp), 200, "send must be accepted: {resp}");

    // Drive the prompts on both sides until the sender reports completion.
    let mut sent = false;
    for _ in 0..400 {
        accept_prompts(&send).await;
        accept_prompts(&recv).await;
        let st = status(&send).await;
        if st["events"]
            .as_array()
            .map(|a| {
                a.iter()
                    .any(|e| e.as_str().unwrap_or("").starts_with("sent "))
            })
            .unwrap_or(false)
        {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let events = status(&send).await["events"].clone();
    assert!(sent, "transfer never completed; sender events: {events}");

    let landed = std::fs::read(recv.dest.path().join("hello.bin")).expect("received file");
    assert_eq!(landed, payload, "content must survive the P2P path");

    // The staged copy exists only to be sent; it must go away afterwards.
    for _ in 0..40 {
        if !staged.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !staged.exists(),
        "staged copy must be cleaned up after the send attempt"
    );

    send.handle.abort();
    recv.handle.abort();
}
