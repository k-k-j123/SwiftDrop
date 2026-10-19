---
date: 2026-10-18
status: Draft for implementation
title: SwiftDrop --- Product and Technical Specification
version: 0.1.0
---

# SwiftDrop: Product and Technical Specification

**Document type:** Product requirements, system architecture, protocol
design, security requirements, implementation plan, and acceptance
criteria\
**Version:** 0.1.0\
**Status:** Draft --- decisions marked *Proposed* should be validated
during implementation\
**Initial platform:** Linux (Rust binary with a self-hosted browser web
UI)\
**Peer devices:** any device on the LAN with a browser can use the
phone-upload path\
**Future platforms:** macOS, Windows\
**Primary use case:** Fast, convenient, secure file transfer between a
user's nearby devices over the local network, without a cloud account or
mandatory cloud relay.

## 1. Executive summary

SwiftDrop is a local-first file transfer application intended to combine
high transfer throughput with an AirDrop-like user experience and broad
platform compatibility.

The first release targets Linux. The application is a Rust workspace
whose binary, `swiftdrop`, is driven through a browser page that the app
itself serves over a hand-rolled local HTTP server. Two computers on the
same LAN or Wi-Fi network connect by explicit `host:port` (default port
52915), establish an authenticated encrypted session, request user
approval when appropriate, and transfer files directly between peers.
File contents must not pass through a cloud server for ordinary local
transfers.

The application is designed around a shared Rust core, a versioned
protocol, and replaceable network transports. TCP over a local IP
network is the initial data transport. Direct USB connectivity is a
future capability and must not be assumed to work merely because a USB
cable is connected; it will require a supported USB networking mode or a
platform-specific transport adapter.

### 1.1 Product vision

A user should be able to open the SwiftDrop web page, enter a peer's
`host:port`, choose files, and send them. A receiving device should show
a clear incoming-transfer prompt unless the sender is trusted and the
user's settings explicitly permit automatic acceptance for the relevant
content and context.

The product should be: - **Fast:** stream files with low overhead and
avoid whole-file buffering. - **Convenient:** a browser interface,
device names, drag-and-drop, and understandable progress. -
**Cross-platform:** protocol and transfer engine are independent of any
UI toolkit. - **Private:** local transfers remain
local; no account or cloud service is required. - **Secure:** encrypted
sessions, authenticated pairing, safe file handling, and explicit
receiving policies. - **Reliable:** integrity verification,
cancellation, recovery, and eventual resumable transfers. -
**Maintainable:** clear module boundaries, automated tests, and a
documented protocol.

### 1.2 Product positioning

SwiftDrop is not initially a remote file-sharing service, cloud storage
product, network drive, or general-purpose device-management suite. Its
core value is fast peer-to-peer transfer on nearby networks. Clipboard
sync, link sharing, device integration, remote transfer, and direct USB
are staged features, not MVP blockers.

## 2. Goals and non-goals

### 2.1 MVP goals

1.  Transfer one or more files between two computers on the same IP
    network.
2.  Target Linux first, keeping the Rust core portable.
3.  Connect to a peer by explicit `host:port` (default port 52915).
4.  Encrypt traffic and authenticate paired peers.
5.  Ask the receiver to approve incoming transfers by default.
6.  Stream data with bounded memory usage.
7.  Show useful progress, speed, estimated remaining time, and errors.
8.  Verify completed file content with a cryptographic hash.
9.  Handle cancellation and partial failures safely.
10. Keep local file transfers independent of cloud services.
11. Establish a benchmark suite for LAN and Wi-Fi performance.

### 2.2 Post-MVP goals

-   Resume interrupted transfers across application restarts.
-   Concurrent transfer queue and multiple-file/folder transfer.
-   Drag-and-drop support in the web UI.
-   Trusted-device policies with granular auto-accept rules.
-   Clipboard text and URL sharing.
-   Background transfer behavior appropriate to each OS.
-   USB networking transport where supported.
-   macOS and Windows clients.
-   Optional remote transfer using a separately designed
    peer-to-peer/relay mode.

### 2.3 Non-goals for the MVP

-   Cloud upload/download or account-based identity.
-   Automatic device discovery, including mDNS/DNS-SD.
-   Guaranteed background receiving.
-   Custom encryption algorithms or custom TCP congestion control.
-   Network filesystem mounting.
-   Remote desktop, phone control, notification mirroring, or broad
    device-management functionality.
-   Guaranteed direct USB data transfer on all combinations of phones
    and computers.
-   Maximum possible throughput on every hardware combination before
    basic correctness and security are established.

## 3. Terminology

-   **Peer:** a SwiftDrop instance running on a device.
-   **Sender:** the peer offering a file or set of files.
-   **Receiver:** the peer receiving and writing the offered files.
-   **Device identity:** a stable cryptographic identity associated with
    an installation or device.
-   **Pairing:** an authenticated process that establishes trust between
    two peers.
-   **Transfer offer:** metadata describing proposed files before data
    is sent.
-   **Transfer session:** the lifecycle of one accepted transfer.
-   **Control channel:** the authenticated connection used for commands,
    status, and metadata.
-   **Data channel:** the connection or stream carrying binary file
    bytes.
-   **Transport adapter:** an implementation that carries protocol
    traffic over a particular network path.
-   **LAN:** a local IP network, including Ethernet and Wi-Fi.
-   **USB networking:** a mode in which a cable and device support an IP
    or other explicit data transport; a cable alone does not guarantee
    this capability.
-   **MVP:** minimum viable product.

## 4. Users and primary scenarios

### 4.1 Primary users

1.  A Linux user sending files to another computer on the same network.
2.  A phone user uploading files to the Linux host through the web page
    in the phone's browser.
3.  A user transferring large files between two computers.
4.  A user on a network where peer addresses must be entered manually.

### 4.2 Core user journeys

#### Journey A: Send from one computer to another

1.  User starts the receiver (`swiftdrop serve` or `receive`) on the
    receiving computer and opens the web page in a browser.
2.  User starts `swiftdrop send` on the sending computer, pointing at
    the receiver's `host:port`.
3.  The receiver shows sender identity, file names, total size, and
    Accept/Reject in the web page.
4.  User accepts.
5.  Transfer progress appears on both peers.
6.  Receiver verifies content and saves to the chosen/default
    destination.
7.  Both peers show success or a clear actionable error.

#### Journey B: Upload from a phone browser

1.  The receiving computer runs `swiftdrop serve` and shows the URL of
    its web page.
2.  The user opens that URL in the phone's browser; no app is installed
    on the phone.
3.  The user selects files and uploads them over the LAN.
4.  Files land on the computer and the browser shows completion.

#### Journey C: Transfer is interrupted

MVP behavior: mark the transfer as interrupted or failed, preserve no
file as "complete," and clean up temporary files according to policy.
Post-MVP behavior: offer resume if the partial data and transfer
identity can be safely reconciled.

## 5. Product requirements

Priorities: - **P0:** required for MVP. - **P1:** important follow-up. -
**P2:** later enhancement.

### 5.1 Pairing and device trust

-   **FR-PAIR-001 (P0):** All control and file data traffic must be
    encrypted in transit.
-   **FR-PAIR-002 (P0):** A new peer must not silently gain
    trusted-device status merely by connecting.
-   **FR-PAIR-003 (P0):** Initial pairing must use an explicit
    user-confirmed process, such as matching a short authentication
    string.
-   **FR-PAIR-004 (P0):** A device identity must be stable across normal
    application restarts.
-   **FR-PAIR-005 (P0):** Users must be able to remove a trusted device.
-   **FR-PAIR-006 (P0):** The receiver must approve incoming transfers
    by default.
-   **FR-PAIR-007 (P1):** Users may configure auto-accept only for
    trusted devices and only with explicit policy choices.
-   **FR-PAIR-008 (P0):** Revoking trust must prevent future sessions
    from being treated as trusted.
-   **FR-PAIR-009 (P0):** A pairing flow must defend against accidental
    pairing with a nearby impersonator; the UI must compare a
    cryptographically derived authentication string.
-   **FR-PAIR-010 (P0):** Private keys and tokens must not be written to
    ordinary logs.

### 5.2 Transfer workflow

-   **FR-XFER-001 (P0):** Users must be able to select one or more files
    for sending.
-   **FR-XFER-002 (P0):** The sender must offer metadata before
    transmitting file bytes.
-   **FR-XFER-003 (P0):** The receiver must accept or reject the offer
    before bulk data is written.
-   **FR-XFER-004 (P0):** The receiver must display sender identity,
    file name(s), file size(s), and total size before acceptance.
-   **FR-XFER-005 (P0):** The transfer must stream bytes rather than
    load an entire file into memory.
-   **FR-XFER-006 (P0):** The UI must display total bytes, transferred
    bytes, progress, speed, and state.
-   **FR-XFER-007 (P0):** Users must be able to cancel an active
    transfer.
-   **FR-XFER-008 (P0):** A cancelled, failed, or interrupted transfer
    must never be reported as completed.
-   **FR-XFER-009 (P0):** The receiver must write to a safe temporary
    file and finalize only after validation.
-   **FR-XFER-010 (P0):** The receiver must prevent path traversal and
    must not trust sender-supplied absolute paths.
-   **FR-XFER-011 (P0):** The app must handle filename collisions
    through an explicit policy such as rename, skip, or ask.
-   **FR-XFER-012 (P0):** Completed files must pass integrity
    verification before being marked successful.
-   **FR-XFER-013 (P1):** Transfers should support pause/resume where
    the transport and platform lifecycle allow it.
-   **FR-XFER-014 (P1):** Interrupted transfers should be resumable
    after restart.
-   **FR-XFER-015 (P1):** The app should support folder transfer by
    creating a manifest of relative paths and files.
-   **FR-XFER-016 (P1):** The app should support bounded concurrent
    transfers.
-   **FR-XFER-017 (P0):** The app must handle empty files, large files,
    Unicode names, and files with spaces or punctuation.
-   **FR-XFER-018 (P0):** The app must not automatically open received
    files.
-   **FR-XFER-019 (P1):** The app should show a completion notification
    when allowed by the OS.
-   **FR-XFER-020 (P0):** The receiver must enforce configurable maximum
    file size, transfer count, and disk-space checks.

### 5.3 User interface

-   **FR-UI-001 (P0):** The web UI must allow connecting to a peer by
    explicit `host:port`.
-   **FR-UI-002 (P0):** The user must be able to send a file from a file
    picker.
-   **FR-UI-003 (P0):** Incoming transfers must have clear Accept and
    Reject actions.
-   **FR-UI-004 (P0):** Transfer progress must remain visible while the
    transfer is active.
-   **FR-UI-005 (P0):** Errors must be actionable and avoid exposing
    stack traces or secrets.
-   **FR-UI-006 (P0):** The web UI must show manual connection details.
-   **FR-UI-007 (P1):** The web UI should support drag-and-drop.
-   **FR-UI-008 (P1):** Any device with a browser on the LAN can upload
    to the web page served by the receiver (no app required).
-   **FR-UI-009 (P1):** The app should include a transfer history with
    user-controlled retention.
-   **FR-UI-011 (P0):** The app must not imply that the internet is
    required for a local transfer.
-   **FR-UI-012 (P1):** The UI should show the active transport when
    known, for example Wi-Fi or Ethernet.

### 5.4 Platform integration

-   **FR-PLAT-001 (P0):** The Linux client must support foreground
    sending and receiving while the app is running.
-   **FR-PLAT-004 (P1):** The client should integrate with OS file
    pickers and notification systems where available.
-   **FR-PLAT-006 (P2):** macOS and Windows should be added after the
    protocol and core are stable.
-   **FR-PLAT-007 (P2):** Direct USB networking should be added behind a
    transport interface.

## 6. Quality attributes and acceptance targets

### 6.1 Performance

Targets are engineering goals, not guarantees across arbitrary hardware.

-   **NFR-PERF-001:** For a single large file on a healthy gigabit
    Ethernet link, target at least 90% of the measured usable TCP
    baseline on the same device pair, once the MVP is optimized.
-   **NFR-PERF-002:** For a healthy 2.5 GbE link, target at least 90% of
    measured usable baseline when disk and CPU permit.
-   **NFR-PERF-003:** Wi-Fi performance should be compared with a
    measured baseline for the same devices, band, channel, and location.
-   **NFR-PERF-004:** Memory usage must not scale linearly with total
    file size.
-   **NFR-PERF-005:** The transfer engine must use bounded buffers and
    backpressure.
-   **NFR-PERF-006:** Do not introduce parallel chunking until profiling
    shows that a single stream is insufficient.
-   **NFR-PERF-007:** Speed display should be smoothed to avoid jitter;
    ETA may be omitted when too little data exists to estimate it
    reliably.
-   **NFR-PERF-008:** Hashing and disk I/O should be measured separately
    from network throughput.

### 6.2 Reliability

-   **NFR-REL-001:** A successful transfer means the final file size and
    digest match the agreed metadata.
-   **NFR-REL-002:** Partial files must be distinguishable from
    completed files.
-   **NFR-REL-003:** The app must tolerate peer disconnection, process
    termination, disk-full conditions, and user cancellation without
    falsely reporting success.
-   **NFR-REL-004:** Transfer state transitions must be explicit and
    tested.
-   **NFR-REL-005:** A malformed or unsupported message must fail safely
    with a protocol error.
-   **NFR-REL-006:** Network retries must be bounded and must not
    duplicate committed data.

### 6.3 Security and privacy

-   **NFR-SEC-001:** TLS 1.3 is the preferred minimum for the initial
    secure transport; use a well-maintained TLS implementation.
-   **NFR-SEC-002:** Do not implement custom cryptographic primitives.
-   **NFR-SEC-004:** Validate all control messages, lengths, file names,
    identifiers, and state transitions.
-   **NFR-SEC-005:** Prevent path traversal, symlink/path substitution
    vulnerabilities, and unsafe overwrite behavior.
-   **NFR-SEC-006:** Apply rate limits and resource limits to
    unauthenticated connection attempts.
-   **NFR-SEC-007:** Do not log file contents, private keys, pairing
    secrets, authentication tokens, or sensitive absolute paths by
    default.
-   **NFR-SEC-008:** Local transfer traffic must not be routed through a
    third-party server.
-   **NFR-SEC-009:** Use secure OS storage for secrets where practical.
-   **NFR-SEC-010:** Keep dependencies updated and run
    dependency/security audits in CI.
-   **NFR-SEC-011:** The receiving device must not execute or preview
    received content automatically.
-   **NFR-SEC-012:** TLS encryption alone does not establish user trust;
    peer authentication and transfer approval remain necessary.

### 6.4 Usability and accessibility

-   **NFR-UX-001:** Core sending and receiving must not require terminal
    usage.
-   **NFR-UX-002:** The UI must support explicit `host:port` entry for
    connecting to peers.
-   **NFR-UX-003:** The app must clearly differentiate waiting for
    approval, transferring, verifying, completed, failed, and cancelled.
-   **NFR-UX-004:** Essential status must not rely on color alone.
-   **NFR-UX-005:** Keyboard navigation and screen-reader labels should
    be supported where the platform and framework permit.
-   **NFR-UX-006:** Units should be consistent: bytes internally,
    human-readable IEC units such as MiB/GiB or clearly labeled decimal
    MB/GB in the UI.
-   **NFR-UX-007:** Errors should include a suggested action where
    possible.

## 7. System architecture

**Status note:** By owner decision on 2026-10-19 the planned Flutter
client and the mDNS/DNS-SD discovery layer were removed. The interface
is now a browser page served by the app, and peers connect by explicit
`host:port` (default 52915) with no device discovery.

### 7.1 High-level components

1.  **Web UI:** a single embedded HTML/JS page served by the app and
    opened in any browser.
2.  **CLI (`apps/cli`):** the `swiftdrop` binary with `send`, `receive`,
    and `serve` verbs and a `--yes` flag for non-interactive
    auto-accept.
3.  **Core (`crates/core`):** orchestration (`sessions.rs`), the web UI
    server (`web.rs`), settings (`settings.rs`), and configuration
    (`config.rs`).
4.  **Protocol module (`crates/protocol`):** versioned messages and
    validation.
5.  **Security module (`crates/security`):** device identity, TLS 1.3
    configuration, SAS pairing, trusted-device storage.
6.  **Transfer module (`crates/transfer`):** file offers, acceptance,
    streaming, integrity, cancellation, and later resume.
7.  **Transport:** TCP over the local IP network, provided inside core;
    a USB network or other adapter may be added later behind the same
    session interface.

### 7.2 Proposed stack

  -----------------------------------------------------------------------
  Area                    Proposed choice         Notes
  ----------------------- ----------------------- -----------------------
  UI                      Browser HTML/JS page    Served by the app; no
                                                  native client

  Core                    Rust                    Shared, testable
                                                  implementation

  Async runtime           Tokio                   Async tasks and network
                                                  I/O

  HTTP/control server     Hand-rolled HTTP/1.1    Zero new dependencies;
                                                  see core::web

  Initial bulk transport  TCP                     Mature, widely
                                                  supported, easy to
                                                  benchmark

  Encryption              TLS 1.3 (rustls, ring)  TLS 1.3-only; ring
                                                  avoids cmake

  Control encoding        JSON                    Simpler diagnostics;
                                                  enforce strict size
                                                  limits

  Hash                    SHA-256                 Integrity, not sender
                                                  authentication

  Persistence             Settings file           Settings and
                                                  trusted-device records

  CI                      GitHub Actions or       Linux builds and
                          equivalent              automated tests
  -----------------------------------------------------------------------

The precise crate selection must be reviewed at implementation time for
maintenance status, supported targets, license, and security history.

### 7.3 Repository structure

``` text
swiftdrop/
├── apps/
│   └── cli/                       # Binary `swiftdrop`: send, receive, serve, --yes
├── crates/
│   ├── core/                      # Orchestration (sessions.rs), web UI (web.rs),
│   │                              # settings.rs, config.rs
│   ├── security/                  # Device identity, TLS, SAS pairing, trust store
│   ├── transfer/                  # State machine, sanitization, streaming, hashing
│   └── protocol/                  # Message types and validation
├── Cargo.toml                     # Workspace manifest and shared dependencies
└── spec.md
```

This reflects the current workspace. Keep new modules behind the
dependency direction in §7.4, and split or merge crates only when a
boundary becomes clear.

### 7.4 Dependency direction

-   `apps/cli` and the browser web UI call the public API exposed by
    `core`.
-   `core` may depend on `protocol`, `security`, and `transfer`.
-   Lower crates never depend on `core`, on the CLI, or on the UI.
-   `protocol` contains data types and validation, not networking,
    filesystem access, or UI behavior.
-   The web UI server (`core::web`) is a deliberate exception to
    protocol purity: it speaks HTTP/1.1 to browsers that cannot speak
    the SwiftDrop wire protocol. Keep it isolated from the authenticated
    peer-session path.
-   No crate may depend on UI widgets or UI state.

## 8. Protocol design: SwiftDrop/1

### 8.1 Design principles

-   Versioned and documented.
-   Transport-independent where practical.
-   Secure by default.
-   Control metadata separated from bulk binary data.
-   Explicit state transitions.
-   Bounded message and resource sizes.
-   Extensible without silently changing semantics.
-   Compatible peers must reject unsupported critical features rather
    than guessing.

### 8.2 Initial network model

Each peer runs a receiver service while receiving is enabled. It listens
on a configured TCP port (default 52915), preferably bound only to
intended interfaces. Peers learn where to connect from an explicit
`host:port` supplied by the user; there is no automatic discovery.

For the MVP: - Use TCP/IP for the control and data path. - Protect the
connection with TLS. - Use a single accepted stream per file or transfer
initially. - Keep the listener closed when the user disables receiving.
- Do not expose the listener to all interfaces unintentionally. -
Configure OS firewall guidance and handle denied permissions gracefully.

**Important:** a listener on a local network is not inherently private.
Network segmentation and firewall rules may allow other local devices to
reach the service. Authentication and approval must still be enforced.

### 8.3 Message envelope

Control messages are UTF-8 JSON objects with a maximum size limit. Each
message includes a message type and protocol version. The implementation
must define exact schemas and reject unknown required fields or invalid
values.

Illustrative envelope:

``` json
{
  "protocol": "SwiftDrop/1",
  "type": "transfer.offer",
  "request_id": "opaque-random-id",
  "payload": {}
}
```

The `request_id` is an example correlation identifier. Production
identifiers must be generated using a cryptographically secure random
source and validated for length and syntax.

### 8.4 Initial control messages

  -----------------------------------------------------------------------
  Message                 Direction               Purpose
  ----------------------- ----------------------- -----------------------
  `hello`                 Both                    Negotiate protocol
                                                  compatibility and
                                                  capabilities

  `pair.request`          Either                  Begin pairing

  `pair.challenge`        Either                  Provide verification
                                                  challenge

  `pair.confirm`          Either                  Confirm the
                                                  user-verified pairing

  `transfer.offer`        Sender → receiver       Propose one or more
                                                  files

  `transfer.accept`       Receiver → sender       Approve offer and
                                                  return transfer
                                                  parameters

  `transfer.reject`       Receiver → sender       Reject offer with a
                                                  safe reason code

  `transfer.progress`     Either/status channel   Report current progress

  `transfer.cancel`       Either                  Request cancellation

  `transfer.complete`     Receiver → sender       Confirm successful
                                                  verification and
                                                  finalization

  `transfer.error`        Either                  Report a typed
                                                  protocol/transfer
                                                  failure

  `ping` / `pong`         Both                    Optional liveness check
  -----------------------------------------------------------------------

Message schemas, error codes, required/optional fields, and maximum
sizes must be documented in `docs/protocol.md` before the protocol is
considered stable.

### 8.5 Transfer offer schema

Illustrative JSON:

``` json
{
  "protocol": "SwiftDrop/1",
  "type": "transfer.offer",
  "request_id": "random-id",
  "payload": {
    "transfer_id": "random-transfer-id",
    "items": [
      {
        "item_id": "random-item-id",
        "name": "video.mp4",
        "size_bytes": 4294967296,
        "sha256": null
      }
    ],
    "total_size_bytes": 4294967296
  }
}
```

Rules: - `transfer_id` and item identifiers are opaque and unique within
their required scope. - Names are display names, not destination
paths. - File sizes are unsigned integers with explicit upper bounds. -
`sha256` may be absent or null before transfer if the sender has not
precomputed the digest. If provided, validate its encoding and length. -
The receiver must independently calculate the digest of the received
bytes. - The receiver must not trust the declared total size without
checking the sum of item sizes and configured limits. - The receiver
must show the user the offer before bulk transfer begins unless a
narrowly scoped auto-accept policy applies.

### 8.6 Binary transfer format

Do not send file bytes as Base64 inside JSON. Use a binary body over an
authenticated connection.

Two initial implementation choices are acceptable: 1. HTTP
request/response streaming, with a defined endpoint for each accepted
item. 2. A length-prefixed binary stream on a dedicated authenticated
TCP connection.

**Recommended MVP choice:** use HTTP streaming for clarity and
interoperability. The request must be tied to a transfer that the
receiver has already accepted. Enforce content-length or an explicit
length field, byte-count limits, request timeouts, and cancellation. Do
not trust a length header without enforcing it while streaming.

Avoid supporting arbitrary chunk offsets in the first release unless
resume is included. Add a documented chunk protocol later if performance
tests justify it.

### 8.7 Error codes

Define stable machine-readable codes, for example: -
`UNSUPPORTED_VERSION` - `AUTHENTICATION_REQUIRED` - `PAIRING_REJECTED` -
`TRANSFER_REJECTED` - `INVALID_METADATA` - `FILE_TOO_LARGE` -
`INSUFFICIENT_STORAGE` - `DESTINATION_UNAVAILABLE` -
`INTEGRITY_MISMATCH` - `TRANSFER_CANCELLED` - `RATE_LIMITED` -
`INTERNAL_ERROR`

Do not send stack traces, filesystem internals, or secrets to the remote
peer. User-facing messages can be more descriptive than protocol error
codes.

### 8.8 Protocol compatibility

-   Protocol versions use an explicit major/minor scheme or equivalent
    documented compatibility policy.
-   Breaking changes require a major version change.
-   Optional capabilities are negotiated explicitly.
-   A peer must not assume that a feature exists because a newer client
    may support it.
-   Unknown optional fields may be ignored only where the schema
    explicitly permits it.
-   Unknown message types must return a safe protocol error.
-   Maintain golden message fixtures and compatibility tests.

## 9. Transfer lifecycle and state machine

### 9.1 Proposed states

``` text
CREATED
  -> OFFERED
  -> AWAITING_APPROVAL
  -> ACCEPTED
  -> TRANSFERRING
  -> VERIFYING
  -> COMPLETED

OFFERED / AWAITING_APPROVAL -> REJECTED
Any active state -> CANCELLING -> CANCELLED
Any nonterminal state -> FAILED
TRANSFERRING -> INTERRUPTED (future resume support)
INTERRUPTED -> ACCEPTED / TRANSFERRING (future resume support)
```

### 9.2 State rules

-   State transitions must be validated centrally, not implemented ad
    hoc in UI callbacks.
-   `COMPLETED` is terminal and requires successful integrity
    verification and finalization.
-   `REJECTED`, `CANCELLED`, and `FAILED` must not be represented as
    completed.
-   A duplicate command must be handled idempotently where possible.
-   A receiver must reject content upload before the corresponding
    transfer is accepted.
-   Cancellation must propagate to network reads, writes, hash
    calculation, and temporary-file cleanup.
-   UI progress events must be derived from core transfer state rather
    than being the source of truth.

### 9.3 Progress calculations

Track: - Total bytes offered. - Bytes accepted for transfer. - Bytes
written to temporary storage. - Bytes verified. - Current state and
timestamps. - Rolling transfer rate. - Optional ETA once enough samples
exist.

Do not equate bytes read by the sender with bytes durably written by the
receiver. The receiver's progress should reflect bytes successfully
written; completion requires verification.

## 10. Security and threat model

### 10.1 Assets to protect

-   File contents during transit.
-   Files written to the receiver.
-   Device identity keys and pairing secrets.
-   Trusted-device records.
-   Local filesystem integrity.
-   User privacy and file names.
-   Network and disk resources.

### 10.2 Threats

-   Malicious peer on the same Wi-Fi.
-   Man-in-the-middle during pairing.
-   Unauthorized file push.
-   Malicious filenames and path traversal.
-   Oversized offers or streams intended to exhaust memory/disk.
-   Excessive connection attempts.
-   Replay or duplicate commands.
-   Partial-file confusion.
-   Compromised or untrusted received content.
-   Insecure debug logging.

### 10.3 Required controls

1.  TLS with authenticated peer identity.
2.  User-verified pairing for new peers.
3.  Receiver approval by default.
4.  Random, non-guessable transfer identifiers.
5.  Strict schema validation and request-size limits.
6.  Maximum concurrent sessions and bounded queues.
7.  Per-transfer byte limits and disk-space checks.
8.  Safe temporary files and safe finalization.
9.  No automatic execution/opening of received files.
10. Revocable trusted-device records.
11. Timeouts for incomplete handshakes and idle transfers.
12. Redacted logs.
13. Dependency auditing and security testing.
14. A documented update/revocation strategy for device identities.

### 10.4 File-system safety

-   Treat sender-supplied names as untrusted display names.
-   Never accept an arbitrary absolute path from a remote peer.
-   Strip or reject path separators and traversal sequences in a
    single-file name.
-   For folder transfers, validate every relative path component and
    reject absolute paths, `..`, drive prefixes, alternate data stream
    syntax where relevant, and platform-specific dangerous path forms.
-   Decide explicitly how symlinks are handled; the MVP should not
    recreate remote symlinks.
-   Avoid symlink-following races by using safe file APIs and creating
    files in a controlled destination directory.
-   Write to a randomly named temporary file in the destination
    filesystem.
-   Ensure collision handling does not silently overwrite a user's
    existing file.
-   Finalize atomically where the platform/filesystem supports it.
-   On failure, remove temporary files or retain them only under a
    documented recoverable-transfer policy.
-   Do not preserve executable permissions from the sender by default.
-   Do not automatically launch the received file.

### 10.5 Pairing design

Pairing flow: 1. Peers establish a TLS connection using temporary or
persistent identity material. 2. The peers derive a short authentication
string (SAS) from the session transcript and identity information using
a reviewed construction (the TLS exporter). 3. Both peers show the same
verification string. 4. The user confirms the match on the intended
devices. 5. The peers persist the verified identity association. 6.
Later connections authenticate against the stored identity. 7. Users can
remove a peer from the trusted list.

The exact cryptographic construction must be selected from a reviewed
library/protocol design. Do not invent an authentication-string
algorithm.

### 10.6 Privacy

-   No mandatory account.
-   No cloud relay for local transfers.
-   No analytics by default in the MVP.
-   User-controlled transfer history.
-   Clear explanation of which interfaces are listening.
-   Explicit user control over receiving behavior.
-   Avoid retaining file content after completion.
-   Document any optional diagnostic data before collecting it.

## 11. Persistence and configuration

### 11.1 Settings

Potential settings: - Device display name. - Receiving enabled. - Default
destination directory. - Ask before receiving: enabled by default. -
Trusted devices. - Maximum concurrent transfers. - Maximum accepted file
size. - Collision behavior. - Notification preference. - Transfer
history retention. - Diagnostic logging level.

### 11.2 Persistence plan

MVP may use a small platform-appropriate settings store for preferences
and trusted-device metadata. SQLite is recommended when transfer history
and resumable state are introduced.

Never store private keys or sensitive tokens in plaintext application
logs. Use OS-provided secure storage where available. If a platform does
not provide a suitable facility, document the limitations and use a
carefully reviewed encrypted-storage design.

### 11.3 Transfer record

A persisted transfer record may include: - Transfer ID. - Peer identity
reference. - Direction (incoming/outgoing). - Item names and sizes. -
State. - Created, accepted, started, and finished timestamps. - Bytes
transferred. - Destination reference. - Expected and computed digest. -
Failure code if any.

Do not store file contents in the database. Make history retention and
deletion behavior explicit.

## 12. Transport design and future USB support

### 12.1 Transport abstraction

The transfer engine depends on a narrow interface for a connected peer
and byte stream, rather than on the web UI or a specific network
interface.

Conceptual responsibilities: - Establish a connection to a verified
peer. - Expose authenticated control/data streams. - Report connection
metadata. - Support cancellation and timeouts. - Close resources
reliably. - Report transport errors in a platform-neutral form.

Do not prematurely build a complex plugin architecture. A small
trait/interface with a TCP implementation is sufficient initially.

### 12.2 LAN/Wi-Fi transport

-   Use TCP over IPv4 and IPv6 where practical.
-   Bind only to interfaces where receiving is intended.
-   Support multiple interfaces without assuming Wi-Fi is always
    preferred.
-   Handle address changes and reconnection.
-   There is no multicast discovery; peers are addressed explicitly by
    `host:port`.
-   Avoid internet relay for peers on the same reachable LAN.

### 12.3 Connection selection

For MVP, connect to the address supplied by the user and attempt a
secure connection. If the host resolves to multiple addresses, try
candidates with timeouts and stop after a successful authenticated
connection.

Future improvements may: - Rank interfaces using observed throughput and
latency. - Detect whether a connection is routed through a VPN. - Offer
a visible active-transport indicator. - Re-evaluate a connection when
the network changes.

Do not infer a route's true throughput from interface labels alone.

### 12.4 Direct USB

USB is a future transport capability, not an MVP requirement.

Potential implementation paths: 1. **USB Ethernet/IP:** if the device
exposes an IP interface over the cable, use the existing TCP protocol.
2. **Platform-specific USB transport:** use native APIs where both
endpoints and the OS support a suitable direct data mode. 3. **USB
accessory or gadget mode:** investigate separately for supported
Android/hardware combinations.

Requirements: - Detect whether a usable USB data path actually exists. -
Never imply that a charging-only cable supports data transfer. -
Preserve the same pairing and security requirements. - Do not bypass
platform permissions or device security restrictions. - Keep USB code
isolated from the core protocol. - Test each supported OS/device
combination explicitly.

## 13. Performance engineering

### 13.1 Initial strategy

1.  Establish a baseline using a network benchmark such as `iperf3`.
2.  Build a simple single-stream sender/receiver.
3.  Measure network throughput, CPU use, disk read/write speed, hashing
    overhead, and memory.
4.  Use asynchronous I/O and bounded buffers.
5.  Avoid excessive copying and per-chunk allocations.
6.  Reuse buffers where profiling demonstrates value.
7.  Add concurrency for separate files with a bounded queue.
8.  Only consider parallel chunks for one large file if single-stream
    performance is below the usable baseline.
9.  Re-run benchmarks on supported platforms.
10. Record device hardware, network conditions, file size, storage type,
    and build version.

### 13.2 Throughput units

Use bytes per second internally. Be explicit in the UI: - MB/s =
megabytes per second, decimal. - MiB/s = mebibytes per second, binary. -
Mbps = megabits per second.

Do not compare Mbps and MB/s without converting units. 1 byte equals 8
bits, excluding protocol overhead.

### 13.3 Benchmark matrix

Test at minimum: - 100 MiB file. - 1 GiB file. - 10 GiB file where
storage permits. - Many small files. - Empty file. - Slow storage
destination. - Wi-Fi with strong signal. - Wi-Fi with weaker signal. -
Gigabit Ethernet. - 2.5 GbE if available. - Concurrent unrelated network
traffic. - Cancellation and reconnection. - Hashing enabled and disabled
only in a test harness, never as a silent production integrity
downgrade.

### 13.4 Performance targets

Targets are measured relative to the usable baseline on the same device
pair: - Gigabit Ethernet: at least 90% of measured baseline after
optimization. - 2.5 GbE: at least 90% of measured baseline where
disk/CPU support it. - Wi-Fi: compare with a measured same-device
baseline, not a universal MB/s figure. - Memory: remain bounded as file
size grows. - Small files: optimize queueing and metadata overhead after
large-file streaming is correct.

A target should not be considered met from a single run. Use repeated
runs and report median plus range.

## 14. Testing strategy

### 14.1 Unit tests

-   Protocol message parsing and validation.
-   Unsupported versions and unknown message types.
-   Transfer state machine transitions.
-   Filename sanitization.
-   File-size and aggregate-size limits.
-   Hash verification.
-   Cancellation behavior.
-   Collision policy.
-   Resume-offset validation when implemented.
-   Pairing state and trust revocation.
-   Error mapping.

### 14.2 Integration tests

-   Sender and receiver on localhost.
-   Sender and receiver on two LAN devices.
-   Linux ↔ Linux.
-   Explicit `host:port` connection setup.
-   TLS handshake and peer identity verification.
-   Accept, reject, cancel, and disconnect flows.
-   Disk full and permission denied.
-   Receiver process killed mid-transfer.
-   Sender process killed mid-transfer.
-   Existing destination filename collision.
-   IPv4 and IPv6 where supported.
-   Multiple simultaneous transfers.
-   Phone-browser HTTP upload.

### 14.3 Security tests

-   Malformed JSON.
-   Oversized request body.
-   Negative or overflowed size fields.
-   Invalid digest encoding.
-   Path traversal filenames.
-   Replayed or duplicate state-changing messages.
-   Unpaired sender attempting upload.
-   Expired or revoked trusted identity.
-   Excessive incoming connection attempts.
-   Symlink and destination race cases.
-   Temporary-file cleanup after failures.

### 14.4 UI tests

-   Peer connection states.
-   File selection.
-   Incoming offer prompt.
-   Accept/reject.
-   Progress update.
-   Cancel confirmation if needed.
-   Error presentation.
-   Keyboard navigation in the browser page.

### 14.5 Performance tests

Performance tests should be opt-in or run on controlled machines,
because hardware and network differences can make CI numbers noisy. CI
should catch gross regressions, while a documented local benchmark
process measures actual throughput.

## 15. Error handling

Errors should be classified into: - Connection. -
Authentication/pairing. - User rejection. - File access. - Disk
capacity. - Network interruption. - Integrity verification. - Protocol
compatibility. - Internal failure.

Examples of user-facing guidance: - "Could not reach the peer. Check the
host and port, that both devices are on the same local network, and that
local-network access is allowed." - "The receiver rejected this
transfer." - "The connection was interrupted. The file was not marked as
complete." - "There is not enough free space at the selected
destination." - "The received file failed integrity verification and was
not finalized." - "This device uses an incompatible protocol version.
Update both applications."

Do not show a raw stack trace as the normal UI error. Keep detailed
diagnostics in opt-in local logs with secrets and sensitive paths
redacted.

## 16. Build and release strategy

### 16.1 Development environments

-   Linux: native development and the primary supported platform.
-   Windows: a later build/test target once the core is stable.
-   macOS: a later stage, with native signing and platform-specific
    review.

### 16.2 CI pipeline

For each pull request: 1. Format Rust code. 2. Run Rust linting with
warnings treated seriously. 3. Run unit tests. 4. Run protocol fixture
tests. 5. Run dependency audit tooling. 6. Build supported targets where
runners permit. 7. Publish test logs and build artifacts. 8. Keep
performance benchmarks separate from ordinary correctness tests.

### 16.3 Release checklist

-   Version and protocol compatibility confirmed.
-   Dependencies audited.
-   Signing and packaging configured.
-   Firewall behavior documented.
-   Permissions minimized.
-   Privacy notice written.
-   Upgrade behavior tested.
-   Transfer completion and failure states tested.
-   Large-file tests completed.
-   No secrets or private paths in logs.
-   Install/uninstall and settings persistence tested.

## 17. MVP acceptance criteria

The MVP is ready for an initial developer/tester release when all P0
criteria below pass.

### Functional

-   [ ] Linux peers can send and receive files while the app is
    running.
-   [ ] A peer can be reached by explicit `host:port`.
-   [ ] Incoming offers show sender and file metadata.
-   [ ] Receiver can accept or reject.
-   [ ] Progress and cancellation work.
-   [ ] Empty files, large files, and Unicode filenames work.
-   [ ] Completed files are verified.
-   [ ] Existing files are not silently overwritten.
-   [ ] Failed transfers are not marked complete.
-   [ ] A phone browser can upload files over the LAN.

### Security

-   [ ] Control and data traffic are encrypted.
-   [ ] Peer identity is verified according to the pairing design.
-   [ ] Unpaired senders cannot silently write files.
-   [ ] Filename/path validation tests pass.
-   [ ] Request and resource limits are enforced.
-   [ ] No sensitive values appear in logs.
-   [ ] Temporary files are handled safely.

### Performance

-   [ ] Streaming uses bounded memory.
-   [ ] Large-file throughput is measured against a baseline.
-   [ ] Hashing, disk I/O, and network bottlenecks are documented.
-   [ ] Repeated benchmark results are recorded.
-   [ ] No unsupported universal throughput claim is made.

### Usability

-   [ ] The browser UI is usable without installing a native app.
-   [ ] Connection failures explain likely next steps.
-   [ ] The active transfer state is obvious.
-   [ ] Receiver behavior settings are understandable.
-   [ ] Local transfers work without an account or cloud service.

## 18. Milestone plan

The durations below are rough planning estimates for one developer and
depend strongly on prior Rust and networking experience.

### Milestone 0: Toolchain and architecture spike

-   Install the Rust toolchain.
-   Create the workspace.
-   Confirm the TLS and pairing approach.
-   Write a one-page architecture decision record for the TLS and
    pairing design.

**Exit:** a minimal workspace builds and the security approach is
recorded.

### Milestone 1: CLI transfer prototype

-   Implement a receiver listener.
-   Implement a sender.
-   Stream one file.
-   Calculate and verify SHA-256.
-   Handle cancellation and disconnects.
-   Benchmark on localhost and LAN.

**Exit:** a large file transfers correctly with bounded memory.

### Milestone 2: Secure transfer and state machine

-   Add TLS.
-   Define peer identity and pairing design.
-   Implement offer/accept/reject.
-   Add resource limits and safe temporary files.
-   Add protocol fixtures and security tests.

**Exit:** a transfer is encrypted, approved, and safely finalized.

### Milestone 3: Browser web UI

-   Serve the embedded HTML/JS page from the app.
-   Implement peer connect by `host:port`.
-   Implement the send/receive interface and the phone-upload path.
-   Package the Linux build.

**Exit:** a user can transfer between peers from a browser without
typing commands by hand.

### Milestone 4: Reliability and performance

-   Add transfer queue.
-   Improve errors and logging.
-   Add persistent history.
-   Add resumability design and implementation.
-   Benchmark and profile bottlenecks.

**Exit:** repeatable performance and failure recovery are documented.

### Milestone 5: Post-MVP integrations

-   Folder transfers.
-   Clipboard and URL sharing.
-   USB networking proof of concept.
-   macOS/Windows planning.
-   Optional remote-transfer architecture only if the product needs it.

## 19. Risks and mitigations

  -----------------------------------------------------------------------
  Risk                    Impact                  Mitigation
  ----------------------- ----------------------- -----------------------
  Platform storage        Files fail to save      Isolate storage behind
  differences             consistently            platform adapters

  Security mistakes in    Unauthorized peer       Use reviewed
  pairing                 access                  cryptographic designs
                                                  and explicit
                                                  verification

  Disk or CPU bottlenecks Lower throughput than   Benchmark each layer
                          network capability      separately

  Protocol drift          Clients become          Versioned schema,
                          incompatible            golden fixtures,
                                                  compatibility tests

  Scope creep             MVP takes too long      Keep USB, remote
                                                  transfer, clipboard,
                                                  and advanced
                                                  parallelism out of P0

  Firewall rules          Connection fails        Clear diagnostics and
                          despite a correct       documented firewall
                                                  behavior

  Unsafe file paths       Files written outside   Strict path validation
                          intended destination    and safe filesystem
                                                  APIs
  -----------------------------------------------------------------------

## 20. Decisions to record before implementation

Create short Architecture Decision Records (ADRs) for: 1. TLS library
and device identity model. 2. Exact pairing and authentication-string
construction. 3. HTTP streaming versus a custom binary stream. 4.
Settings and secure key storage. 5. Transfer history schema. 6. License
and dependency policy. 7. Packaging and update strategy. 8. Whether
automatic acceptance will be supported and its exact restrictions.

Do not leave the pairing/security design as an informal TODO while
exposing a network listener. The first prototype may use a restricted
development-only mode on a trusted test network, but that mode must be
clearly marked and must not be shipped as a production default.

## 21. Recommended first implementation slice

The best first slice is deliberately small:

1.  A Rust receiver listens on a configurable local TCP port.
2.  A Rust sender connects to it on a known LAN address.
3.  The receiver accepts a single file offer.
4.  The receiver writes to a temporary file with bounded buffering.
5.  The sender streams the content.
6.  The receiver calculates SHA-256 and compares it with the sender's
    expected digest.
7.  The receiver atomically finalizes the file and reports success.
8.  An integration test verifies the received file byte-for-byte.
9.  A benchmark records throughput, CPU use, and peak memory.

Then add TLS and authenticated pairing before exposing the service for
general use. Next add the browser web UI, then the phone-upload path.
This sequence validates the difficult technical risks early and
minimizes the chance of building a polished interface around an
unreliable transfer engine.

## 22. Definition of done for each feature

A feature is not done merely because the happy path works. It is done
when: - Its expected behavior is documented. - Input validation is
implemented. - Failure and cancellation paths are tested. - Security and
privacy implications are reviewed. - Logs are useful but do not leak
secrets. - Platform-specific behavior is documented. - Tests pass on
relevant targets. - The UI gives clear feedback if the feature is
user-facing. - Protocol changes have fixtures and compatibility tests. -
Performance claims, if any, are supported by reproducible measurements.

## 23. Final engineering recommendation

Build SwiftDrop as a **secure, local-first transfer engine first, and a
polished sharing experience second**.

Use Rust for protocol, streaming, and the core, TCP/TLS for the initial
transport, a self-hosted browser page for the interface, and SHA-256 for
file integrity. Peers connect by explicit `host:port`. Keep direct USB
as a future adapter, beginning with USB networking where supported.

Do not start with cloud infrastructure, custom cryptography, a custom
congestion-control algorithm, or every platform-specific integration at
once. First prove a secure, high-throughput transfer between two
devices, measure it against the network baseline, and expand from there.

**The first deliverable is a working Rust sender/receiver prototype with
integration tests and benchmark results.** That is the foundation on
which the rest of SwiftDrop should be built.
