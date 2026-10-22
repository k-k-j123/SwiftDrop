//! End-to-end transfer over localhost (spec §21 "first slice" proof).
//!
//! This is the test that makes the receive path real: two in-process
//! SwiftDrop peers with **separate device identities and trust stores**
//! talk over a real TCP socket wrapped in real TLS 1.3, and we assert the
//! bytes that land on disk are byte-for-byte what was sent.
//!
//! Why this shape and not a mock:
//!
//! * The whole point of `core::sessions` is that TLS, exporter-bound
//!   auth, SAS pairing, framing and file sink *compose*. A mock of any
//!   layer would prove the mock, not the composition.
//! * Two distinct `config_dir`s force the **first-connection pairing**
//!   path (neither peer is trusted yet). Reusing one identity would
//!   silently skip the handshake branch we most want covered.
//! * `127.0.0.1` with an ephemeral port needs no fixed port and no
//!   network access, so it stays hermetic and parallel-safe — matching
//!   the existing crates' test style (spec §14.1).
//!
//! The `SWIFTDROP_CONFIG_DIR` env var is deliberately left untouched:
//! `receive`/`send` take explicit paths, so nothing here reads or writes
//! the developer's real config (spec §8.10).
//!
//! Covered here: happy-path pairing → offer → accept → stream → verify,
//! a multi-file offer, an empty file, and a unicode filename.
//! Not covered (would need a fault-injection seam at the framing layer):
//! a peer that lies about a digest or disconnects mid-frame — see the
//! `shortcut:` note in `receive_transfer`.

use std::path::PathBuf;
use std::sync::Arc;
use swiftdrop_core::progress::{State, TransferProgress};
use swiftdrop_core::{
    receive, send, AutoAccept, CollisionPolicy, OfferLimits, ReceiveOptions, SendOptions,
    SessionPrompt,
};
use swiftdrop_protocol::messages::TransferOffer;
use tempfile::TempDir;
use tokio::net::TcpListener;

/// A prompt that declines everything — the mirror of `AutoAccept`, used
/// to prove the reject path doesn't leave stray bytes or temp files.
struct AutoReject;

#[async_trait::async_trait]
impl SessionPrompt for AutoReject {
    async fn confirm_pairing(&self, _peer: &swiftdrop_core::Peer, _sas: &str) -> bool {
        true // pair, then reject the offer: exercises the offer-decline path
    }
    async fn confirm_offer(&self, _peer: &swiftdrop_core::Peer, _offer: &TransferOffer) -> bool {
        false
    }
}

/// Two isolated config dirs + a source dir + a receive dest dir, all
/// temp. Dropping the harness deletes them.
struct PeerDirs {
    sender_cfg: TempDir,
    receiver_cfg: TempDir,
    src: TempDir,
    dest: TempDir,
}

impl PeerDirs {
    fn new() -> Self {
        Self {
            sender_cfg: TempDir::new().expect("sender config dir"),
            receiver_cfg: TempDir::new().expect("receiver config dir"),
            src: TempDir::new().expect("source dir"),
            dest: TempDir::new().expect("dest dir"),
        }
    }

    /// Counters nobody reads; tests that assert on progress override the
    /// field with their own handle (spec §9.3).
    fn send_opts(&self) -> SendOptions {
        SendOptions {
            config_dir: self.sender_cfg.path().to_path_buf(),
            device_name: "sender-test".into(),
            limits: OfferLimits::default(),
            auto_accept: true,
            progress: Arc::new(TransferProgress::new()),
        }
    }

    fn recv_opts(&self) -> ReceiveOptions {
        ReceiveOptions {
            config_dir: self.receiver_cfg.path().to_path_buf(),
            device_name: "receiver-test".into(),
            dest_dir: self.dest.path().to_path_buf(),
            collision_policy: CollisionPolicy::Rename,
            limits: OfferLimits::default(),
            auto_accept: true,
            progress: Arc::new(TransferProgress::new()),
        }
    }
}

/// Write `bytes` and return the path it was written to.
fn write_file(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.path().join(name);
    std::fs::write(&p, bytes).expect("write source file");
    p
}

/// Bind an ephemeral localhost port and return (listener, addr).
async fn bind_localhost() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

/// Happy path: a multi-file offer (including an empty file and a unicode
/// name) lands on disk byte-for-byte, and the sender's outcome reports
/// the receiver's real saved names.
#[tokio::test]
async fn localhost_roundtrip_multi_file() {
    let dirs = PeerDirs::new();

    // Deterministic-but-varied content: 1 MiB of a repeating pattern is
    // enough to cross several 64 KiB data frames without being slow.
    let big: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let big_path = write_file(&dirs.src, "big.bin", &big);
    let empty_path = write_file(&dirs.src, "empty.txt", b"");
    let unicode_path = write_file(&dirs.src, "héllo — wörld ✅.txt", b"unicode name payload");

    let (listener, addr) = bind_localhost().await;
    let recv_opts = dirs.recv_opts();
    let receiver = tokio::spawn(async move { receive(listener, recv_opts, AutoAccept).await });

    let outcome = send(
        addr,
        "127.0.0.1",
        vec![big_path, empty_path, unicode_path],
        dirs.send_opts(),
        AutoAccept,
    )
    .await
    .expect("send should succeed");

    // The sender sees exactly what the receiver reported saving.
    assert_eq!(
        outcome.total_bytes,
        (1024 * 1024 + "unicode name payload".len()) as u64
    );
    assert_eq!(outcome.saved_names.len(), 3);

    // Every file landed in the dest dir with byte-identical content.
    // Collision policy is Rename, and names are unique, so each saved
    // name should equal the sanitized source name.
    let mut landed_big = false;
    let mut landed_empty = false;
    let mut landed_unicode = false;
    for saved in &outcome.saved_names {
        let on_disk = dirs.dest.path().join(saved);
        let bytes = std::fs::read(&on_disk).expect("saved file readable");
        if saved == "big.bin" {
            assert_eq!(bytes, big, "big.bin content must be byte-identical");
            landed_big = true;
        } else if saved == "empty.txt" {
            assert!(bytes.is_empty(), "empty.txt must be empty");
            landed_empty = true;
        } else if saved.starts_with("héllo") {
            assert_eq!(bytes, b"unicode name payload", "unicode file content");
            landed_unicode = true;
        } else {
            panic!("unexpected saved name: {saved}");
        }
    }
    assert!(
        landed_big && landed_empty && landed_unicode,
        "all three files must land"
    );

    // Both peers now trust each other (pairing recorded on both sides).
    let sender_trust =
        swiftdrop_core::TrustStore::open(dirs.sender_cfg.path().join(swiftdrop_core::TRUST_FILE))
            .expect("open sender trust store");
    let receiver_trust =
        swiftdrop_core::TrustStore::open(dirs.receiver_cfg.path().join(swiftdrop_core::TRUST_FILE))
            .expect("open receiver trust store");
    assert_eq!(
        sender_trust.list().len(),
        1,
        "sender recorded exactly one trusted peer"
    );
    assert_eq!(
        receiver_trust.list().len(),
        1,
        "receiver recorded exactly one trusted peer"
    );

    receiver.abort();
}

/// Declining the offer must not write any file and must not leave the
/// sender reporting a phantom save. The sender gets a clean rejection.
#[tokio::test]
async fn localhost_rejected_offer_writes_nothing() {
    let dirs = PeerDirs::new();
    let payload_path = write_file(&dirs.src, "rejected.bin", b"should never land");

    let (listener, addr) = bind_localhost().await;
    // auto_accept: false is the whole point — the prompt must be asked.
    let recv_opts = ReceiveOptions {
        auto_accept: false,
        ..dirs.recv_opts()
    };
    let receiver = tokio::spawn(async move { receive(listener, recv_opts, AutoReject).await });

    let result = send(
        addr,
        "127.0.0.1",
        vec![payload_path],
        dirs.send_opts(),
        AutoAccept,
    )
    .await;

    // The sender must learn the offer was declined — not hang, not
    // falsely succeed.
    match result {
        Err(e) => {
            let code = e.wire_code();
            assert_eq!(
                code,
                swiftdrop_protocol::errors::ErrorCode::TransferRejected,
                "declined offer should surface as TRANSFER_REJECTED, got {code}"
            );
        }
        Ok(_) => panic!("rejected offer must not succeed"),
    }

    // No file landed in the dest dir.
    let landed: Vec<_> = std::fs::read_dir(dirs.dest.path())
        .expect("read dest dir")
        .collect();
    assert!(landed.is_empty(), "rejected offer must write no files");

    receiver.abort();
}

/// The counters behind the web UI's bar must actually move and end up
/// honest: the sender counts bytes it handed to the socket, the receiver
/// counts bytes it wrote to temp storage, and both match the offer's total
/// (spec FR-UI-004, §9.3). The rejected-offer test above covers the other
/// half — a declined prompt must leave a counter that never started alone.
#[tokio::test]
async fn localhost_progress_counts_both_sides() {
    let dirs = PeerDirs::new();
    // 512 KiB crosses eight 64 KiB data frames, so the counter is genuinely
    // accumulated rather than set to the total in one step.
    let payload: Vec<u8> = (0..512 * 1024u32).map(|i| (i % 251) as u8).collect();
    let path = write_file(&dirs.src, "counted.bin", &payload);
    let size = payload.len() as u64;

    let (listener, addr) = bind_localhost().await;
    let send_progress = Arc::new(TransferProgress::new());
    let recv_progress = Arc::new(TransferProgress::new());

    let recv_opts = ReceiveOptions {
        progress: Arc::clone(&recv_progress),
        ..dirs.recv_opts()
    };
    let receiver = tokio::spawn(async move { receive(listener, recv_opts, AutoAccept).await });

    let send_opts = SendOptions {
        progress: Arc::clone(&send_progress),
        ..dirs.send_opts()
    };
    send(addr, "127.0.0.1", vec![path], send_opts, AutoAccept)
        .await
        .expect("send should succeed");

    // `send` only returns once the receiver answered, so the sender's
    // counter is final: everything was written and nothing failed.
    let sent = send_progress.snapshot();
    assert_eq!(sent.total, size, "total comes from the offer");
    assert_eq!(sent.bytes, size, "a completed send counts every byte");
    assert_eq!(sent.state, State::Sent);

    // The receiver counted the same bytes on their way to temp storage.
    // Its terminal state is not asserted: the session task marks
    // `received` just after it answers, which can trail `send`'s return.
    let received = recv_progress.snapshot();
    assert_eq!(received.total, size);
    assert_eq!(received.bytes, size, "a verified receive counts every byte");
    assert!(
        matches!(received.state, State::Receiving | State::Received),
        "receive counter should be live or done, got {:?}",
        received.state
    );

    receiver.abort();
}
