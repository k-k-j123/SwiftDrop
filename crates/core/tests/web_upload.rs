//! End-to-end tests for the web upload bridge (`swiftdrop_core::web`).
//!
//! These exercise the real socket path a phone browser would hit: raw
//! HTTP/1.1 over `127.0.0.1:0` (ephemeral port, hermetic — no fixed
//! ports, no network access, matching the rest of the workspace's tests).
//!
//! Covered:
//! * `GET /` serves the upload page (the phone must see something usable);
//! * a multi-chunk upload lands byte-identical (streams, never buffers);
//! * percent-encoded unicode filenames decode and sanitize correctly;
//! * a body shorter than its declared `Content-Length` fails cleanly and
//!   leaves **no temp file** — the failure path is where half-received
//!   data would leak if the abort logic were wrong.
//!
//! Not covered (would need a fault seam in the server): concurrent
//! multi-connection uploads — the browser's parallelism is plain tokio
//! task-per-connection, the same shape the P2P accept loop already proves.

use std::path::PathBuf;
use swiftdrop_core::web::serve_on;
use swiftdrop_core::CollisionPolicy;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Start a server on an ephemeral port. Returns the bound address, a
/// handle to abort when done, and the temp dest dir (kept alive for the
/// caller to inspect).
async fn start_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, TempDir) {
    let dest = TempDir::new().expect("dest dir");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let dest_path: PathBuf = dest.path().to_path_buf();
    // serve_on never returns on success; the JoinHandle exists only so
    // tests can abort the loop at teardown instead of leaking a task.
    let handle = tokio::spawn(async move {
        let _ = serve_on(listener, dest_path, CollisionPolicy::Rename).await;
    });
    (addr, handle, dest)
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
fn post(name_encoded: &str, declared_len: u64, payload: &[u8]) -> Vec<u8> {
    let mut req = format!(
        "POST /upload?name={name_encoded} HTTP/1.1\r\nHost: test\r\nContent-Length: {declared_len}\r\n\r\n"
    )
    .into_bytes();
    req.extend_from_slice(payload);
    req
}

fn status_of(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("HTTP status line")
}

/// `GET /` must return the upload page — this is the only thing a phone
/// sees first, so a broken response here breaks the whole phone path.
#[tokio::test]
async fn serves_upload_page() {
    let (addr, handle, _dest) = start_server().await;
    let resp = request(addr, b"GET / HTTP/1.1\r\nHost: test\r\n\r\n".to_vec()).await;

    assert_eq!(status_of(&resp), 200);
    assert!(resp.contains("SwiftDrop"), "page must identify the app");
    assert!(
        resp.contains("/upload"),
        "page must contain the upload target"
    );
    handle.abort();
}

/// The core speed claim: a file many `READ_CHUNK`s long is streamed to
/// disk byte-for-byte. 16 MiB crosses 16 chunk boundaries — enough to
/// catch off-by-one reads, lost tail bytes, or partial-chunk writes.
#[tokio::test]
async fn large_upload_lands_byte_identical() {
    let (addr, handle, dest) = start_server().await;

    // Deterministic pattern larger than one 1 MiB read chunk.
    let payload: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let resp = request(addr, post("big.bin", payload.len() as u64, &payload)).await;

    assert_eq!(status_of(&resp), 200, "upload must succeed: {resp}");
    assert!(
        resp.contains("saved: big.bin"),
        "must report the saved name"
    );

    let landed = std::fs::read(dest.path().join("big.bin")).expect("landed file");
    assert_eq!(landed.len(), payload.len(), "size must match exactly");
    assert_eq!(landed, payload, "content must be byte-identical");

    handle.abort();
}

/// Filenames arrive percent-encoded from `encodeURIComponent`; unicode
/// must survive the round trip (same class of input the P2P path proves
/// with its unicode-filename test).
#[tokio::test]
async fn percent_encoded_unicode_name_is_decoded() {
    let (addr, handle, dest) = start_server().await;

    // "héllo ✅.txt" as a browser would encode it.
    let resp = request(addr, post("h%C3%A9llo%20%E2%9C%85.txt", 7, b"payload")).await;

    assert_eq!(status_of(&resp), 200, "upload must succeed: {resp}");
    assert!(
        resp.contains("saved: héllo ✅.txt"),
        "saved name must be decoded unicode, got: {resp}"
    );

    let landed = std::fs::read(dest.path().join("héllo ✅.txt")).expect("landed file");
    assert_eq!(landed, b"payload");

    handle.abort();
}

/// Declaring 1000 bytes but sending 100 then closing must fail with 400
/// and leave the dest dir **empty** — proving the abort path removed the
/// temp file (`IncomingFile` has no Drop cleanup; `upload()` must call
/// `abort()` explicitly, and this is the test that would catch it not).
#[tokio::test]
async fn short_body_fails_cleanly_and_leaves_no_temp_file() {
    let (addr, handle, dest) = start_server().await;

    let resp = request(addr, post("partial.bin", 1000, &[0u8; 100])).await;

    assert_eq!(status_of(&resp), 400, "short body must be rejected: {resp}");
    assert!(
        resp.contains("declared 1000"),
        "error must name the shortfall, got: {resp}"
    );

    let leftovers: Vec<_> = std::fs::read_dir(dest.path())
        .expect("read dest dir")
        .collect();
    assert!(
        leftovers.is_empty(),
        "no file — temp or final — may survive a failed upload"
    );

    handle.abort();
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
    let (addr, handle, dest) = start_server().await;

    // a) ".." alone → refused before the body is touched.
    let resp = request(addr, post("%2E%2E", 4, b"body")).await;
    assert_eq!(status_of(&resp), 400, "bare .. must be rejected: {resp}");
    assert!(
        std::fs::read_dir(dest.path()).unwrap().next().is_none(),
        "nothing may be written for a rejected name"
    );

    // b) a traversal attempt → stripped to the final component.
    let resp = request(addr, post("..%2F..%2Fetc%2Fpasswd", 4, b"body")).await;
    assert_eq!(status_of(&resp), 200, "stripped name must upload: {resp}");
    assert!(
        resp.contains("saved: passwd"),
        "only the final component may survive: {resp}"
    );

    // It landed inside dest (a temp dir) — containment, byte-checked.
    let landed = std::fs::read(dest.path().join("passwd")).expect("contained file");
    assert_eq!(landed, b"body");

    handle.abort();
}
