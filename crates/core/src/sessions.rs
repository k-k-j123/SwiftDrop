//! Session orchestration — where TLS, authentication, pairing, offers and
//! byte streaming finally meet (spec §9, §10.5, §21 "first slice").
//!
//! One encrypted TCP connection carries everything: the frame format is
//! `swiftdrop-protocol`'s, the pipe is TLS 1.3 via `swiftdrop-security`,
//! the bytes land through `swiftdrop-transfer`. This module is the only
//! place that knows the *order* things happen in:
//!
//! ```text
//! TLS handshake
//!   → hello ⇄ hello          (auth_sig: proof of the identity key,
//!                              bound to this session's TLS exporter)
//!   ⇄ pair.confirm           (a human compares the SAS on both screens
//!                              unless the peer is already trusted)
//!   → transfer.offer         (sender, after pre-hashing every file)
//!   ⇄ accept / reject        (receiver, after the user approves)
//!   → item_begin + DATA…     (sender pushes every declared byte)
//!   ⇄ item_done… + complete  (receiver verifies, finalizes, reports)
//! ```
//!
//! Both sides send their `hello` **before** reading the peer's: the write
//! is a few hundred bytes, so it always fits in a socket buffer and the
//! write-then-read order can never deadlock (the reverse, read-then-write,
//! *would* deadlock if both peers tried it).
//!
//! Everything a peer sends is hostile until checked, in layers: envelope
//! validation (`swiftdrop-protocol::validate`), identity proof
//! (`swiftdrop-security::auth`), user approval ([`SessionPrompt`]), and
//! finally size/digest verification in `swiftdrop-transfer`'s file sink.
//! See spec §10.3 for the control list this defends against.
//!
//! ## Known MVP limitations (upgrade paths marked inline)
//!
//! * **Cancellation is process-level**: Ctrl-C kills the process, the
//!   socket closes, and the peer fails cleanly with temp files deleted —
//!   never a false "complete" (spec NFR-REL-002). There is no in-app
//!   cancel button yet (spec FR-XFER-007). Upgrade: plumb a
//!   `tokio_util::sync::CancellationToken` through both loops.
//! * **No `transfer.progress` messages on the wire**: during the bulk
//!   phase the sender writes without reading, so a receiver pushing
//!   progress frames would eventually block on a full socket buffer while
//!   the sender blocks on a full *send* buffer — a shared-socket
//!   deadlock. Progress is counted **locally, one counter per side**,
//!   supplied by the caller through the options ([`TransferProgress`]) and
//!   polled by whoever displays it — the web UI's bar (spec §9.3).
//!   Upgrade: a second status connection, or have the sender read while
//!   it writes.
//! * **One connection is served at a time** (spec §8.2 permits this for
//!   the MVP). Upgrade: bounded `tokio::spawn` per accepted stream.
//! * **Pairing-prompt budget**: the prompt timeout reuses
//!   `PAIRING_TIMEOUT_MS` (30s default). A human walking to another
//!   device can exceed it — raise it in `config.rs` if pairing timeouts
//!   show up in the field.

use async_trait::async_trait;
use base64::Engine as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use swiftdrop_protocol::errors::{ErrorCode, ProtocolError};
use swiftdrop_protocol::framing::{read_message, write_frame, write_message};
use swiftdrop_protocol::messages::{
    Hello, Message, OfferItem, PairConfirm, TransferAccept, TransferComplete, TransferError,
    TransferItemBegin, TransferItemDone, TransferOffer, TransferReject,
};
use swiftdrop_protocol::validate;
use swiftdrop_protocol::{new_request_id, FRAME_KIND_DATA, MAX_DATA_FRAME_BYTES, PROTOCOL_MINOR};
use swiftdrop_security::identity::DeviceIdentity;
use swiftdrop_security::trust::TRUST_FILE;
use swiftdrop_security::{auth, tls, SecurityError, TrustStore};
use swiftdrop_transfer::file_sink::IncomingFile;
use swiftdrop_transfer::file_source::{sha256_file, OutgoingFile, PrehashError};
use swiftdrop_transfer::paths::CollisionPolicy;
use swiftdrop_transfer::state::{StateError, TransferState, TransferStateTracker};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

// Re-exported so callers building [`ReceiveOptions`]/[`SendOptions`]
// don't need a direct dependency on the protocol crate.
pub use swiftdrop_protocol::validate::OfferLimits;

use crate::progress::{Direction, TransferProgress};

/// Base64 engine for the opaque key/signature fields in a `hello`.
/// `STANDARD` (with padding) matches what `docs/protocol.md` documents.
const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

// ─────────────────────────────────────────────────────────────────────────────
// What the user is asked
// ─────────────────────────────────────────────────────────────────────────────

/// A verified peer, for prompts and logs. `device_id` is already proven
/// to belong to this session's TLS peer before any prompt shows it.
#[derive(Debug, Clone)]
pub struct Peer {
    pub device_id: String,
    pub display_name: String,
}

/// What the user (or an automated harness) is asked during a session.
///
/// The trait is async because the eventual Flutter UI will answer over a
/// bridge, not synchronously on the runtime thread (spec §7.3).
#[async_trait]
pub trait SessionPrompt: Send + Sync {
    /// Show `sas` and wait for the user to confirm it matches the peer's
    /// screen (spec FR-PAIR-003 / FR-PAIR-009). `false` aborts the session.
    async fn confirm_pairing(&self, peer: &Peer, sas: &str) -> bool;

    /// Show the offer summary and wait for accept/reject (spec
    /// FR-XFER-003 / FR-XFER-004). `false` rejects the transfer.
    async fn confirm_offer(&self, peer: &Peer, offer: &TransferOffer) -> bool;
}

/// Accepts everything without asking. Used by tests **and** by the CLI's
/// explicit `--yes` flag. NEVER the default: spec FR-PAIR-006 requires
/// approval, and §20 forbids shipping an auto-accept mode as default.
pub struct AutoAccept;

#[async_trait]
impl SessionPrompt for AutoAccept {
    async fn confirm_pairing(&self, _peer: &Peer, _sas: &str) -> bool {
        true
    }
    async fn confirm_offer(&self, _peer: &Peer, _offer: &TransferOffer) -> bool {
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// Everything that can end a session.
///
/// [`SessionError::wire_code`] maps a failure to the stable code the
/// *peer* is told (spec §8.8). Local detail — paths, causes — never
/// crosses the wire (spec NFR-SEC-007).
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Security(#[from] SecurityError),
    #[error(transparent)]
    Receive(#[from] swiftdrop_transfer::file_sink::ReceiveError),
    #[error(transparent)]
    Prehash(#[from] PrehashError),
    /// An illegal move in the §9.2 state machine — a bug, not a peer
    /// problem; surfaces loudly in tests.
    #[error(transparent)]
    State(#[from] StateError),
    #[error("TLS handshake failed: {0}")]
    TlsHandshake(String),
    /// Neither side confirmed the SAS (ours declined, or the peer sent
    /// `pair.confirm{accepted:false}`).
    #[error("pairing was not confirmed")]
    PairingDeclined,
    /// The local user rejected the offer or pairing.
    #[error("user declined: {0}")]
    UserRejected(&'static str),
    /// The peer rejected the offer, or reported a transfer error.
    #[error("peer reported {0}")]
    PeerRejected(ErrorCode),
    /// A step exceeded its configured budget (spec §10.3 control 11):
    /// handshakes, prompts, or a single frame read.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
}

impl SessionError {
    /// The stable code our peer is told about this failure (spec §8.8).
    pub fn wire_code(&self) -> ErrorCode {
        match self {
            SessionError::Protocol(p) => p.code(),
            SessionError::Io(_) => ErrorCode::ProtocolError, // mirror ProtocolError::Io
            SessionError::Security(SecurityError::AuthSignatureInvalid)
            | SessionError::Security(SecurityError::IdentityMismatch)
            | SessionError::Security(SecurityError::DeviceKeyInvalid(_))
            | SessionError::Security(SecurityError::SignatureInvalid(_)) => {
                ErrorCode::AuthenticationRequired
            }
            SessionError::Security(_) => ErrorCode::InternalError,
            SessionError::Receive(r) => r.code(),
            SessionError::Prehash(p) => p.code(),
            SessionError::State(_) => ErrorCode::InternalError,
            SessionError::TlsHandshake(_) => ErrorCode::InternalError,
            SessionError::PairingDeclined => ErrorCode::PairingRejected,
            SessionError::UserRejected(_) => ErrorCode::TransferRejected,
            SessionError::PeerRejected(c) => *c,
            SessionError::Timeout(_) => ErrorCode::Timeout,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Options and outcomes
// ─────────────────────────────────────────────────────────────────────────────

/// Everything [`send`] needs besides the files themselves. The caller
/// resolves these (from `Settings`, or from temp dirs in a test) so the
/// session code never reads env vars or real user settings directly.
pub struct SendOptions {
    /// Directory holding this device's identity key and trust store.
    pub config_dir: PathBuf,
    /// Display name sent in our `hello`.
    pub device_name: String,
    /// Size/count caps applied to the offer we build.
    pub limits: OfferLimits,
    /// Skip prompts (CLI `--yes` / tests). Never the default (spec §20).
    pub auto_accept: bool,
    /// Where this send reports live byte counts (spec §9.3). Shared with the
    /// caller so it can display them while the send is still running; a
    /// caller that shows nothing passes a fresh one.
    pub progress: Arc<TransferProgress>,
}

/// Everything [`receive`] needs besides the listening socket.
pub struct ReceiveOptions {
    /// Directory holding this device's identity key and trust store.
    pub config_dir: PathBuf,
    /// Display name sent in our `hello`.
    pub device_name: String,
    /// Directory finalized files are renamed into.
    pub dest_dir: PathBuf,
    /// What to do when a received name collides on disk (spec FR-XFER-013).
    pub collision_policy: CollisionPolicy,
    /// Caps applied to incoming offers (defaults match spec §11.1).
    pub limits: OfferLimits,
    /// Skip prompts (CLI `--yes` / tests). Never the default (spec §20).
    pub auto_accept: bool,
    /// Where each received transfer reports live byte counts (spec §9.3).
    /// One counter serves every session this listener accepts; each session
    /// resets it and the last one's result stays until the next arrives.
    pub progress: Arc<TransferProgress>,
}

/// What one completed **outbound** transfer produced.
pub struct SendOutcome {
    /// File names the receiver reported as saved (spec FR-XFER-015).
    pub saved_names: Vec<String>,
    /// Total bytes of file content pushed over the wire.
    pub total_bytes: u64,
}

/// A file staged for sending: opened once, its size and SHA-256 digest
/// fixed at open time, before any peer exists to lie to us (ADR-0007:
/// the offer is what we *pre-hashed*, not what we "meant").
pub struct PreparedFile {
    /// Sanitized name shown to the receiver (derived from the local path).
    pub display_name: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the file's bytes at prepare time.
    pub sha256: String,
    file: OutgoingFile,
}

impl PreparedFile {
    /// Open and pre-hash one local file, capping it at `max_bytes` so a
    /// runaway file is refused before it reaches the offer (spec §11.1).
    pub async fn prepare(path: PathBuf, max_bytes: u64) -> Result<Self, PrehashError> {
        let display_name = OutgoingFile::display_name(&path);
        let file = OutgoingFile::open(&path, max_bytes).await?;
        let size = file.declared_size();
        let sha256 = sha256_file(path).await?;
        Ok(Self {
            display_name,
            size,
            sha256,
            file,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry points
// ─────────────────────────────────────────────────────────────────────────────

/// Accept connections on `listener` forever, one session at a time,
/// saving whatever is approved into `opts.dest_dir` (spec §21 slice 1).
///
/// Returns only if the listener itself fails; a failed or rejected peer
/// session is logged and the loop keeps listening.
pub async fn receive<P: SessionPrompt>(
    listener: TcpListener,
    opts: ReceiveOptions,
    prompt: P,
) -> Result<(), SessionError> {
    let identity = DeviceIdentity::load_or_create(&opts.config_dir, &opts.device_name)?;
    let mut trust = TrustStore::open(opts.config_dir.join(TRUST_FILE))?;
    let acceptor = TlsAcceptor::from(std::sync::Arc::new(tls::server_config(&identity)?));

    // shortcut: no per-address rate limit on handshakes yet (spec
    // NFR-SEC-006). upgrade: token-bucket per peer address before the
    // TLS handshake (each attempt costs both sides a P-256 keygen).
    loop {
        let (tcp, peer_addr) = listener.accept().await?;
        tracing::info!(%peer_addr, "incoming connection");

        let mut stream = bounded(
            "TLS handshake",
            crate::config::HANDSHAKE_TIMEOUT_MS,
            acceptor.accept(tcp),
        )
        .await
        .map_err(|e| match e {
            SessionError::Io(io) => SessionError::TlsHandshake(io.to_string()),
            other => other,
        })?;

        let outcome = run_session(
            &mut stream,
            |s: &tokio_rustls::server::TlsStream<TcpStream>| tls::server_exporter(s),
            &identity,
            &opts.device_name,
            &mut trust,
            &prompt,
            opts.auto_accept,
            opts.limits,
            Role::Receive {
                dest_dir: opts.dest_dir.clone(),
                collision_policy: opts.collision_policy,
                progress: Arc::clone(&opts.progress),
            },
        )
        .await;
        // Every exit path lands here, so the counter can never be left
        // claiming a transfer is still running (spec FR-UI-004). Sessions
        // that never transferred anything leave it untouched — see
        // `TransferProgress::finish`.
        opts.progress.finish(outcome.is_ok());

        match outcome {
            Ok(_) => tracing::info!(%peer_addr, "session finished"),
            Err(SessionError::UserRejected(_)) | Err(SessionError::PairingDeclined) => {
                tracing::info!(%peer_addr, "session not confirmed by user")
            }
            Err(e) => tracing::warn!(%peer_addr, error = %e, "session failed"),
        }
    }
}

/// Connect to `addr` (TLS SNI name = `host`) and send every file in
/// `files` as one offer (spec §21 slice 1).
///
/// Files are prepared (opened + pre-hashed) **before** connecting, so a
/// missing/unreadable file fails locally with nothing on the wire.
pub async fn send<P: SessionPrompt>(
    addr: std::net::SocketAddr,
    host: &str,
    files: Vec<PathBuf>,
    opts: SendOptions,
    prompt: P,
) -> Result<SendOutcome, SessionError> {
    let identity = DeviceIdentity::load_or_create(&opts.config_dir, &opts.device_name)?;
    let mut trust = TrustStore::open(opts.config_dir.join(TRUST_FILE))?;

    if files.is_empty() {
        return Err(ProtocolError::InvalidOffer("no files to send".into()).into());
    }
    let mut prepared = Vec::with_capacity(files.len());
    for path in files {
        prepared.push(PreparedFile::prepare(path, opts.limits.max_file_bytes).await?);
    }

    let tcp = bounded(
        "connect",
        crate::config::HANDSHAKE_TIMEOUT_MS,
        TcpStream::connect(addr),
    )
    .await?;
    let server_name: rustls::pki_types::ServerName<'static> = match host.parse::<std::net::IpAddr>()
    {
        Ok(ip) => rustls::pki_types::ServerName::IpAddress(ip.into()),
        Err(_) => rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| {
            ProtocolError::InvalidField {
                field: "host",
                reason: "not an IP address or DNS name".into(),
            }
        })?,
    };
    let connector = TlsConnector::from(std::sync::Arc::new(tls::client_config()?));
    let mut stream = bounded(
        "TLS handshake",
        crate::config::HANDSHAKE_TIMEOUT_MS,
        connector.connect(server_name, tcp),
    )
    .await
    .map_err(|e| match e {
        SessionError::Io(io) => SessionError::TlsHandshake(io.to_string()),
        other => other,
    })?;

    let result = run_session(
        &mut stream,
        |s: &tokio_rustls::client::TlsStream<TcpStream>| tls::client_exporter(s),
        &identity,
        &opts.device_name,
        &mut trust,
        &prompt,
        opts.auto_accept,
        opts.limits,
        Role::Send {
            files: prepared,
            progress: Arc::clone(&opts.progress),
        },
    )
    .await;
    // Every exit path lands here, so a failed send never leaves the bar
    // spinning (spec FR-UI-004). A failure before the bulk phase (bad
    // address, declined pairing) leaves an untouched counter alone — see
    // `TransferProgress::finish`.
    opts.progress.finish(result.is_ok());

    match result {
        // The send role's branch always produces an outcome; this is an
        // internal invariant, not a runtime condition.
        Ok(Some(outcome)) => Ok(outcome),
        Ok(None) => unreachable!("send role must produce a SendOutcome"),
        Err(e) => Err(e),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The shared session body
// ─────────────────────────────────────────────────────────────────────────────

/// Which side of the offer/transfer phase this session plays. The only
/// behavioral fork inside [`run_session`]; everything up to (and
/// including) pairing is byte-for-byte symmetric.
enum Role {
    Send {
        files: Vec<PreparedFile>,
        progress: Arc<TransferProgress>,
    },
    Receive {
        dest_dir: PathBuf,
        collision_policy: CollisionPolicy,
        progress: Arc<TransferProgress>,
    },
}

/// Run one authenticated session on an established TLS stream, identically
/// for sender and receiver until pairing; `exporter_of` is the only
/// difference (each role reaches the TLS exporter through its own stream
/// type). Returns the send outcome for the send role, `None` for receive.
async fn run_session<S, E>(
    stream: &mut S,
    exporter_of: E,
    identity: &DeviceIdentity,
    device_name: &str,
    trust: &mut TrustStore,
    prompt: &dyn SessionPrompt,
    auto_accept: bool,
    limits: OfferLimits,
    role: Role,
) -> Result<Option<SendOutcome>, SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    E: for<'a> Fn(&'a S) -> &'a dyn tls::ExporterSource,
{
    let mut tracker = TransferStateTracker::new();

    // ── hello (write-first; see module docs for why) ────────────────────
    let my_hello = build_hello(identity, device_name, exporter_of(stream))?;
    bounded(
        "hello write",
        crate::config::HANDSHAKE_TIMEOUT_MS,
        write_message(stream, &my_hello),
    )
    .await?;
    let peer_hello =
        match read_control(stream, "hello", crate::config::HANDSHAKE_TIMEOUT_MS).await? {
            Message::Hello(h) => h,
            other => return Err(out_of_order(&other, "hello")),
        };
    // All exporter-derived work happens under one immutable stream
    // borrow that ends here, so the `&mut stream` uses above and below
    // never overlap it (see `server_exporter` docs for why this split).
    let (peer, sas) = bind_peer(exporter_of(stream), &identity.device_id(), peer_hello)?;
    tracing::info!(peer = %peer.display_name, "peer authenticated");

    // ── pairing (spec FR-PAIR-003: trust only when BOTH confirm) ────────
    let trusted = trust.contains(&peer.device_id);
    // shortcut: the prompt budget reuses PAIRING_TIMEOUT_MS (30s); raise
    // it in config.rs if slow humans trip it in the field. A timeout
    // reads as "no answer" → the safe direction (nothing proceeds).
    let accepted = trusted
        || auto_accept
        || prompt_budget(
            crate::config::PAIRING_TIMEOUT_MS,
            prompt.confirm_pairing(&peer, &sas),
        )
        .await;
    pair_exchange(stream, accepted).await?;
    if !trusted {
        // Record trust only after the peer also confirmed — recording on
        // our side alone would persist a pairing a MITM could then ride.
        trust.add(&peer.device_id, &peer.display_name)?;
        tracing::info!(peer = %peer.display_name, "peer added to trust store");
    }

    // ── offer / transfer ────────────────────────────────────────────────
    let result = match role {
        Role::Send { files, progress } => {
            send_transfer(stream, &mut tracker, files, &progress).await
        }
        Role::Receive {
            dest_dir,
            collision_policy,
            progress,
        } => {
            receive_transfer(
                stream,
                &mut tracker,
                &peer,
                limits,
                dest_dir,
                collision_policy,
                prompt,
                auto_accept,
                &progress,
            )
            .await
        }
    };
    if let Err(e) = &result {
        // Spec §9.2: terminal failure goes through the state machine.
        // `transition_or` leaves an already-terminal state (e.g. the
        // REJECTED the reject path just recorded) untouched.
        tracker.transition_or(TransferState::Failed, TransferState::Failed);
        // Tell the peer why before the socket drops — but only for
        // failures not already communicated by a typed message (the
        // reject path sent `transfer.reject`, a decline sent nothing).
        if !matches!(
            e,
            SessionError::UserRejected(_) | SessionError::PeerRejected(_)
        ) {
            // Best effort: if the peer already left, this write fails and
            // we don't care — the local error is the one the user sees.
            let _ = write_message(
                stream,
                &Message::Error(TransferError {
                    transfer_id: None,
                    code: e.wire_code(),
                    message: None,
                }),
            )
            .await;
        }
    }
    result
}

/// Build our `hello`, signing the TLS exporter with the identity key so
/// the peer can prove we hold that key *in this session* (ADR-0003).
fn build_hello(
    identity: &DeviceIdentity,
    device_name: &str,
    exporter: &dyn tls::ExporterSource,
) -> Result<Message, SessionError> {
    let sig = auth::sign_auth(identity, exporter)?;
    Ok(Message::Hello(Hello {
        protocol_minor: PROTOCOL_MINOR,
        device_id: identity.device_id(),
        display_name: device_name.to_string(),
        device_key: BASE64.encode(identity.public_sec1()),
        auth_sig: BASE64.encode(sig),
        capabilities: Default::default(),
    }))
}

/// Verify a peer's `hello` against this session's exporter and derive the
/// pairing SAS. Fails if the claimed `device_id` isn't the hash of the
/// claimed key, or the signature doesn't verify — either way the peer is
/// lying or broken, and no further byte it sends is trusted.
fn bind_peer(
    exporter: &dyn tls::ExporterSource,
    my_id: &str,
    hello: Hello,
) -> Result<(Peer, String), SessionError> {
    validate::check_display_name(&hello.display_name)?;
    let key = BASE64
        .decode(&hello.device_key)
        .map_err(|_| ProtocolError::InvalidField {
            field: "device_key",
            reason: "not base64".into(),
        })?;
    let sig = BASE64
        .decode(&hello.auth_sig)
        .map_err(|_| ProtocolError::InvalidField {
            field: "auth_sig",
            reason: "not base64".into(),
        })?;
    // Verifies the signature AND that device_id == SHA-256(key)
    // (spec FR-PAIR-002); returns the *verified* id — we trust no claim.
    let device_id = auth::verify_auth(&key, &hello.device_id, &sig, exporter)?;
    let sas = auth::derive_sas(exporter, my_id, &device_id)?;
    Ok((
        Peer {
            device_id,
            display_name: hello.display_name,
        },
        sas,
    ))
}

/// Exchange `pair.confirm` — both sides write theirs before reading the
/// peer's, so neither can block the other. Declining writes a refusal and
/// stops immediately; the peer's read then fails fast (no full timeout).
async fn pair_exchange<S>(stream: &mut S, accepted: bool) -> Result<(), SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mine = Message::PairConfirm(PairConfirm {
        accepted,
        reason: if accepted {
            None
        } else {
            Some("declined by user".into())
        },
    });
    bounded(
        "pair.confirm write",
        crate::config::PAIRING_TIMEOUT_MS,
        write_message(stream, &mine),
    )
    .await?;
    if !accepted {
        return Err(SessionError::UserRejected("pairing declined"));
    }
    match read_control(
        stream,
        "peer pair.confirm",
        crate::config::PAIRING_TIMEOUT_MS,
    )
    .await?
    {
        Message::PairConfirm(PairConfirm { accepted: true, .. }) => Ok(()),
        Message::PairConfirm(PairConfirm {
            accepted: false, ..
        }) => Err(SessionError::PairingDeclined),
        other => Err(out_of_order(&other, "pair.confirm")),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Sender side
// ─────────────────────────────────────────────────────────────────────────────

/// Offer every prepared file, stream the accepted ones, and cross-check
/// the receiver's verification results (spec §9.2 flow). The receiver's
/// *user* approves the offer (spec FR-XFER-003) — the sender never
/// prompts after pairing.
async fn send_transfer<S>(
    stream: &mut S,
    tracker: &mut TransferStateTracker,
    mut files: Vec<PreparedFile>,
    progress: &TransferProgress,
) -> Result<Option<SendOutcome>, SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let transfer_id = new_request_id();
    let items: Vec<_> = files
        .iter()
        .map(|f| OfferItem {
            item_id: new_request_id(),
            name: f.display_name.clone(),
            size_bytes: f.size,
            // Pre-hashed (ADR-0007): the digest is what we can prove,
            // so receivers verify against it unconditionally.
            sha256: Some(f.sha256.clone()),
        })
        .collect();
    let total = items
        .iter()
        .fold(0u64, |a, b| a.saturating_add(b.size_bytes));
    let offer = TransferOffer {
        transfer_id: transfer_id.clone(),
        items,
        total_size_bytes: total,
    };
    tracker.transition(TransferState::Offered)?;
    write_message(stream, &Message::Offer(offer.clone())).await?;
    tracker.transition(TransferState::AwaitingApproval)?;

    match read_control(
        stream,
        "transfer.accept",
        crate::config::STREAM_IO_TIMEOUT_MS,
    )
    .await?
    {
        Message::Accept(TransferAccept { transfer_id: got }) if got == transfer_id => {}
        Message::Reject(TransferReject { code, .. }) => {
            tracker.transition(TransferState::Rejected)?;
            return Err(SessionError::PeerRejected(code));
        }
        // e.g. the receiver rejected our *offer contents* before showing
        // the user (size/count limits) — surface its stable code.
        Message::Error(TransferError { code, .. }) => {
            return Err(SessionError::PeerRejected(code));
        }
        other => return Err(out_of_order(&other, "transfer.accept")),
    }
    tracker.transition(TransferState::Accepted)?;
    tracker.transition(TransferState::Transferring)?;

    // ── bulk phase: write everything, read nothing (see module docs) ────
    // The clock and the byte count start here, not at the offer: "bytes
    // written to the socket" is what this side can honestly report, and
    // waiting for approval is not transfer time (spec §9.3).
    progress.begin(Direction::Send, total);
    let mut buf = vec![0u8; MAX_DATA_FRAME_BYTES];
    for (item, file) in offer.items.iter().zip(files.iter_mut()) {
        write_message(
            stream,
            &Message::ItemBegin(TransferItemBegin {
                transfer_id: transfer_id.clone(),
                item_id: item.item_id.clone(),
                size_bytes: file.size,
            }),
        )
        .await?;
        loop {
            let n = file.file.next_chunk(&mut buf).await?;
            if n == 0 {
                break;
            }
            write_frame(stream, FRAME_KIND_DATA, &buf[..n]).await?;
            // Counted after the write completes, so the number is bytes the
            // socket accepted, never bytes we merely read from the disk.
            progress.add(n as u64);
        }
        tracing::debug!(name = %item.name, "item streamed");
    }

    // ── verification phase: the receiver reports per-item results ───────
    tracker.transition(TransferState::Verifying)?;
    for item in &offer.items {
        match read_control(
            stream,
            "transfer.item_done",
            crate::config::STREAM_IO_TIMEOUT_MS,
        )
        .await?
        {
            Message::ItemDone(done)
                if done.transfer_id == transfer_id && done.item_id == item.item_id =>
            {
                if !done.ok {
                    return Err(SessionError::PeerRejected(
                        done.code.unwrap_or(ErrorCode::IntegrityMismatch),
                    ));
                }
                // Both sides hashed the same bytes on the same session;
                // a digest disagreement means one side saw different
                // bytes — refuse to call it complete (spec FR-XFER-012).
                if done.sha256.as_deref() != item.sha256.as_deref() {
                    return Err(ProtocolError::InvalidField {
                        field: "sha256",
                        reason: "receiver's digest differs from ours".into(),
                    }
                    .into());
                }
            }
            other => return Err(out_of_order(&other, "transfer.item_done")),
        }
    }
    let saved_names = match read_control(
        stream,
        "transfer.complete",
        crate::config::STREAM_IO_TIMEOUT_MS,
    )
    .await?
    {
        Message::Complete(TransferComplete {
            transfer_id: got,
            saved_names,
        }) if got == transfer_id => saved_names,
        other => return Err(out_of_order(&other, "transfer.complete")),
    };
    tracker.transition(TransferState::Completed)?;

    let total_bytes = offer.total_size_bytes;
    Ok(Some(SendOutcome {
        saved_names,
        total_bytes,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Receiver side
// ─────────────────────────────────────────────────────────────────────────────

/// Approve, receive, and verify an offer (spec §21 slice 1).
async fn receive_transfer<S>(
    stream: &mut S,
    tracker: &mut TransferStateTracker,
    peer: &Peer,
    limits: OfferLimits,
    dest_dir: PathBuf,
    collision_policy: CollisionPolicy,
    prompt: &dyn SessionPrompt,
    auto_accept: bool,
    progress: &TransferProgress,
) -> Result<Option<SendOutcome>, SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let offer = match read_control(
        stream,
        "transfer.offer",
        crate::config::STREAM_IO_TIMEOUT_MS,
    )
    .await?
    {
        Message::Offer(o) => o,
        other => return Err(out_of_order(&other, "transfer.offer")),
    };
    tracker.transition(TransferState::Offered)?;
    // Reject a bad offer *before* any user prompt or byte of file data;
    // the error frame goes out via `run_session`'s failure path.
    validate::check_offer(&offer, &limits)?;
    tracker.transition(TransferState::AwaitingApproval)?;

    let accepted = auto_accept
        || prompt_budget(
            crate::config::PAIRING_TIMEOUT_MS,
            prompt.confirm_offer(peer, &offer),
        )
        .await;
    if !accepted {
        tracker.transition(TransferState::Rejected)?;
        write_message(
            stream,
            &Message::Reject(TransferReject {
                transfer_id: offer.transfer_id.clone(),
                code: ErrorCode::TransferRejected,
                message: None,
            }),
        )
        .await?;
        return Err(SessionError::UserRejected("transfer offer declined"));
    }
    write_message(
        stream,
        &Message::Accept(TransferAccept {
            transfer_id: offer.transfer_id.clone(),
        }),
    )
    .await?;
    tracker.transition(TransferState::Accepted)?;
    tracker.transition(TransferState::Transferring)?;

    // ── bulk phase: temp files under construction ───────────────────────
    // Any failure mid-stream deletes every temp file: a partial receive
    // is never left looking like a finished one (spec NFR-REL-002).
    // The counter starts after approval for the same reason as the sender's
    // (spec §9.3), and it counts bytes written to temp storage — never
    // bytes "received", which only the digest check can upgrade to saved.
    progress.begin(Direction::Receive, offer.total_size_bytes);
    let mut sinks: Vec<IncomingFile> = Vec::with_capacity(offer.items.len());
    if let Err(e) = receive_items(
        stream,
        &offer,
        dest_dir,
        collision_policy,
        &mut sinks,
        progress,
    )
    .await
    {
        for sink in sinks {
            sink.abort();
        }
        return Err(e);
    }

    // ── verification phase: digest check, fsync, atomic rename ──────────
    tracker.transition(TransferState::Verifying)?;
    let mut saved_names = Vec::with_capacity(sinks.len());
    let mut total_received = 0u64;
    let mut it = sinks.into_iter();
    for item in &offer.items {
        let sink = it.next().expect("one sink per offer item");
        match sink.finalize(item.sha256.as_deref()) {
            Ok(f) => {
                total_received += f.size_bytes;
                saved_names.push(f.saved_name);
            }
            Err(e) => {
                // The failed file's temp is already cleaned up by
                // `finalize`; clean up the not-yet-finalized remainder.
                for rest in it {
                    rest.abort();
                }
                return Err(e.into());
            }
        }
    }

    // Item-by-item proof, in offer order, each only after that file is
    // fsync'd and renamed (spec FR-XFER-009); then the all-done message.
    for item in &offer.items {
        write_message(
            stream,
            &Message::ItemDone(TransferItemDone {
                transfer_id: offer.transfer_id.clone(),
                item_id: item.item_id.clone(),
                ok: true,
                sha256: item.sha256.clone(),
                code: None,
            }),
        )
        .await?;
    }
    write_message(
        stream,
        &Message::Complete(TransferComplete {
            transfer_id: offer.transfer_id.clone(),
            saved_names,
        }),
    )
    .await?;
    tracker.transition(TransferState::Completed)?;
    tracing::info!(bytes = total_received, "transfer complete");
    Ok(None)
}

/// Read exactly the bytes each offered item promised, in offer order,
/// writing them into temp files. Stops at the declared totals — extra
/// frames past a declared size are a protocol violation (except at an
/// item boundary, where the next control frame must be the next
/// `item_begin`; anything else is rejected).
async fn receive_items<S>(
    stream: &mut S,
    offer: &TransferOffer,
    dest_dir: PathBuf,
    collision_policy: CollisionPolicy,
    sinks: &mut Vec<IncomingFile>,
    progress: &TransferProgress,
) -> Result<(), SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for item in &offer.items {
        let begin = match read_control(
            stream,
            "transfer.item_begin",
            crate::config::STREAM_IO_TIMEOUT_MS,
        )
        .await?
        {
            Message::ItemBegin(b) => b,
            other => return Err(out_of_order(&other, "transfer.item_begin")),
        };
        // The sender must walk our offer in order — a mismatched id or
        // size means it is streaming something we never approved.
        if begin.transfer_id != offer.transfer_id
            || begin.item_id != item.item_id
            || begin.size_bytes != item.size_bytes
        {
            return Err(ProtocolError::InvalidField {
                field: "item_begin",
                reason: "does not match the offer".into(),
            }
            .into());
        }
        let mut sink =
            IncomingFile::create(&dest_dir, &item.name, item.size_bytes, collision_policy)?;
        let mut got = 0u64;
        while got < item.size_bytes {
            let (msg, data) = bounded(
                "data frame",
                crate::config::STREAM_IO_TIMEOUT_MS,
                read_message(stream),
            )
            .await?;
            match (msg, data) {
                (_, Some(bytes)) => {
                    sink.write_chunk(&bytes)?;
                    got += bytes.len() as u64;
                    // After the write, not before: the number means "bytes
                    // in temp storage", which is what this side can prove
                    // (spec §9.3).
                    progress.add(bytes.len() as u64);
                }
                (Some(Message::Cancel(_)), None) => {
                    return Err(SessionError::PeerRejected(ErrorCode::TransferCancelled));
                }
                (Some(other), None) => return Err(out_of_order(&other, "data frame")),
                // `read_message` always yields at least one of the two.
                (None, None) => unreachable!("read_message returns (msg, data)"),
            }
        }
        sinks.push(sink);
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Small shared helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Bound a step by its configured budget so a silent peer or an
/// unattended prompt can't hang a session forever (spec §10.3 control 11).
async fn bounded<T, E: Into<SessionError>>(
    what: &'static str,
    ms: u64,
    step: impl Future<Output = Result<T, E>>,
) -> Result<T, SessionError> {
    tokio::time::timeout(Duration::from_millis(ms), step)
        .await
        .map_err(|_| SessionError::Timeout(what))?
        .map_err(Into::into)
}

/// A prompt's answer budget: `false` on timeout, because an unattended
/// prompt must never proceed — the approval can only come from a human
/// actually answering (spec FR-PAIR-006).
async fn prompt_budget(ms: u64, answer: impl Future<Output = bool>) -> bool {
    tokio::time::timeout(Duration::from_millis(ms), answer)
        .await
        .unwrap_or(false)
}

/// Read the next control frame. A DATA frame here is a violation: bulk
/// bytes are only legal between an item's `item_begin` and its declared
/// end.
async fn read_control<S: AsyncRead + Unpin>(
    stream: &mut S,
    what: &'static str,
    ms: u64,
) -> Result<Message, SessionError> {
    let (msg, _data) = bounded(what, ms, read_message(stream)).await?;
    match msg {
        Some(m) => Ok(m),
        None => Err(ProtocolError::OutOfOrder {
            got: "data frame".to_string(),
            expected: what,
        }
        .into()),
    }
}

/// An unexpected (but well-formed) message — the single place that
/// formats the out-of-order error, so wording stays consistent.
fn out_of_order(msg: &Message, expected: &'static str) -> SessionError {
    ProtocolError::OutOfOrder {
        got: msg.type_name().to_string(),
        expected,
    }
    .into()
}
