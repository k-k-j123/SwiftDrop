# SwiftDrop

Local-first, AirDrop-style file transfer. No cloud, no accounts — files go directly between two machines on the same network, or from your phone's browser to your PC.

**Current state:** the app *is* the Rust workspace. `swiftdrop serve` runs a web UI in your browser with three things in it: **send** to another computer, **receive** from one, and **upload from a phone** that has nothing installed. There is no mobile app — the Flutter client and the mDNS discovery layer were removed by owner decision (see the status note in `spec.md` §7).

`spec.md` (repo root) is the source of truth for the product. Code comments cite spec IDs (`FR-XFER-020`, `NFR-SEC-007`, `§9.2`) — keep doing that when contributing.

---

## Install

```bash
./install.sh        # binary + app-menu launcher + icon, for the current user only
```

That runs `cargo install --path apps/cli` and drops a launcher into `~/.local/share/applications`. After that, **SwiftDrop appears in your app menu**; launching it starts the server and opens the page in your browser. From a terminal it is just:

```bash
swiftdrop serve --open
```

> **Use a release build.** Measured on a 2 GB localhost upload: **573 MB/s (release) vs 58 MB/s (debug)** — an unoptimized build costs ~10×. `install.sh` and `cargo install` both build release by default.

### Options

```bash
swiftdrop serve [--listen 0.0.0.0:8080] [--dest DIR] [--open]
```

- `--listen` — HTTP bind address for the UI (default `0.0.0.0:8080`, i.e. your LAN).
- `--dest` — where received files land (default: this platform's Downloads folder).
- `--open` — also launch the local browser at the page.

---

## What the page does

**Send to a computer.** Type the other computer's `host:port` (default port `52915`), pick files, send. The files are staged locally first (a browser cannot hand the server a local path), then sent over the real authenticated SwiftDrop protocol. A bar under the tabs tracks the transfer — percent, bytes, live MB/s and time left — on both machines.

**Receive.** Pick a port and press Start. The other computer sends to *your* address and port. Incoming pairings and offers appear on the page as Accept / Decline prompts.

**Upload from a phone.** A phone on the same WiFi opens `http://<pc-ip>:8080`, picks files, and watches per-file progress and live MB/s. Nothing to install on the phone.

> ### ⚠️ The whole port is an open door on your LAN — by explicit design
>
> `serve` speaks **plain HTTP with no authentication, no PIN, and no TLS** — that covers the phone upload *and* the send/receive controls on the same port. Anyone who can reach it can drop files into your destination folder (names are sanitized, so they cannot escape that folder — but they can fill it), start a listener, or make your machine send a file you previously staged. This was a deliberate priority call by the owner: raw speed for files beyond 10 GB over hardening.
>
> **Only run it on a network you trust** (home WiFi). Don't forward the port, don't expose it to the internet, don't run it on café or hotel WiFi. For authenticated transfer between your own machines without a browser, use `send` / `receive` below.

Why a web page at all: a browser cannot open raw TCP or speak SwiftDrop's TLS + custom wire protocol, so a phone can never be a true P2P peer through a web page — the upload path is the pragmatic bridge. Design details, the two security postures that share the port, and every upgrade path (PIN, TLS) are documented in `crates/core/src/web.rs`.

---

## Machine ↔ machine without a browser (scripts, tests, benchmarks)

```bash
# receiver
swiftdrop receive                          # default port 52915, dest = Downloads
swiftdrop receive --listen 0.0.0.0:52915 --dest ./inbox

# sender
swiftdrop send 192.168.1.20:52915 report.mp4 photo/
```

First contact shows a **6-digit SAS** — type `y` on both sides if the codes match. That pairing is recorded in the trust store; trusted peers connect without prompting thereafter.

`--yes` disables every approval prompt. It exists for scripts and tests only — never for normal use (spec §20 forbids shipping auto-accept as the default).

---

## How it works

Strict downward dependencies (spec §7.4 — never add an edge upward; the UI goes through `swiftdrop-core`'s public API only):

```
apps/cli  (binary `swiftdrop`)   serve (the app) / send / receive
    ↓
crates/core       orchestration facade — sessions.rs: full connection lifecycle
    ↓             web.rs: the web UI (deliberate exception to protocol purity)
crates/transfer   state machine, path sanitization, streaming, SHA-256 finalize
crates/security   TLS 1.3, device identity, SAS pairing, trust store
crates/protocol   wire format only — no networking, no filesystem, no UI
```

**Receiving is always the same four steps**, P2P or browser upload: temp-file → hash → fsync → atomic rename (`crates/transfer/src/file_sink.rs`). A half-written file is never visible under its final name (spec §10.4).

### Security model in one paragraph

P2P sessions run TLS 1.3 (`rustls`, `ring` backend — deliberately `default-features = false` to avoid the cmake requirement of `aws-lc-rs`). Peer identity isn't taken from certificates: each side signs the RFC 5705 TLS exporter with its device identity key and checks the signature against the claimed device ID, the trust store, and the SAS code. Sender-supplied filenames are hostile input — everything passes through `sanitize_file_name` (`crates/transfer/src/paths.rs`), and logs never contain secrets or file contents (spec NFR-SEC-007). The **HTTP surface skips all of this** — see the warning above.

---

## Development

```bash
# full verification, in spec §16.2 order (no CI enforces it)
cargo fmt --check
cargo clippy --all-targets
cargo test --workspace        # 90 tests
```

Tests are hermetic: tempfiles, in-memory I/O, `127.0.0.1:0` — no services, network access, or fixtures required.

| Target | Command |
|---|---|
| End-to-end P2P (two peers, real TLS) | `cargo test -p swiftdrop-core --test localhost_transfer` |
| Web UI: page, uploads, and a full send→receive through the API | `cargo test -p swiftdrop-core --test web_upload` |
| One crate | `cargo test -p swiftdrop-transfer` |
| One test | `cargo test -p swiftdrop-transfer state::tests::<name>` |

- **MSRV 1.88** (workspace `rust-version`, required by rcgen); developed on 1.99.
- Config/identity/trust store live in the platform config dir under `swiftdrop/`; override with `SWIFTDROP_CONFIG_DIR`. Destination folder is a `--dest` flag (no env override yet).
- `AGENTS.md` has the agent-facing detail: workspace rules, non-obvious constraints, and what not to "fix".
