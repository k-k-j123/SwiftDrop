# SwiftDrop

Local-first, AirDrop-style file transfer. No cloud, no accounts — files go directly between two machines on the same network, or from your phone's browser to your PC.

**Current state:** the Rust workspace is the working product today: a real CLI (`send` / `receive` / `serve`) with a complete, authenticated P2P transfer path and a speed-first web upload path. The Flutter UI from `spec.md` §7.3 does **not exist yet** (`apps/` contains only `cli`).

`spec.md` (repo root) is the source of truth for the product. Code comments cite spec IDs (`FR-XFER-020`, `NFR-SEC-007`, `§9.2`) — keep doing that when contributing.

---

## Quick start

```bash
cargo build --release      # binary: target/release/swiftdrop
```

> **Use the `release` binary, not `cargo run`.** Measured on a 2 GB localhost upload: **573 MB/s (release) vs 58 MB/s (debug)** — an unoptimized build costs ~10×.

### 1. Phone → PC (no app on the phone)

```bash
./target/release/swiftdrop serve
# or: swiftdrop serve --listen 0.0.0.0:9000 --dest ~/Downloads/inbox
```

On your phone (same WiFi), open the URL the server prints — `http://<pc-ip>:8080` — pick files, watch per-file progress and live MB/s. Files land byte-identical in the destination folder (platform Downloads by default).

> ### ⚠️ The web path is an open door on your LAN — by explicit design
>
> `serve` speaks **plain HTTP with no authentication, no PIN, and no TLS**. Anyone who can reach the port can write files into your destination folder (names are sanitized, so they can't escape that folder — but they can fill it). This was a deliberate priority call: raw speed for >10 GB files over hardening.
>
> **Only run it on a network you trust** (home WiFi). Don't forward the port, don't expose it to the internet. For authenticated transfer between your own machines, use `send`/`receive` below.

Why a web page at all: a browser cannot open raw TCP or speak SwiftDrop's TLS + custom wire protocol, so a phone can never be a true P2P peer through a web page. `serve` is the pragmatic bridge that works today, before the Flutter client exists. Design details and upgrade paths (PIN, TLS) are documented in `crates/core/src/web.rs`.

### 2. Machine ↔ machine (full P2P, authenticated)

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

Strict downward dependencies (spec §7.4 — never add an edge upward; UI goes through `swiftdrop-core`'s public API only):

```
apps/cli  (binary `swiftdrop`)   send / receive / serve verbs
    ↓
crates/core       orchestration facade — sessions.rs: full connection lifecycle;
    ↓             web.rs: plain-HTTP upload bridge (the deliberate exception below)
crates/discovery  mDNS `_swiftdrop._tcp.local.` + manual address fallback
crates/transfer   state machine, path sanitization, streaming, SHA-256 finalize
crates/security   TLS 1.3, device identity, SAS pairing, trust store
crates/protocol   wire format only — no networking, no filesystem, no UI
```

**Receiving is always the same four steps**, P2P or web: temp-file → hash → fsync → atomic rename (`crates/transfer/src/file_sink.rs`). A half-written file is never visible under its final name (spec §10.4).

### Security model in one paragraph

P2P sessions run TLS 1.3 (`rustls`, `ring` backend — deliberately `default-features = false` to avoid the cmake requirement of `aws-lc-rs`). Peer identity isn't taken from certificates: each side signs the RFC 5705 TLS exporter with its device identity key and checks the signature against the claimed device ID, the trust store, and the SAS code. Sender-supplied filenames are hostile input — everything passes through `sanitize_file_name` (`crates/transfer/src/paths.rs`), and logs never contain secrets or file contents (spec NFR-SEC-007). The **web path skips all of this** — see the warning above.

---

## Development

```bash
# full verification, in spec §16.2 order (no CI enforces it)
cargo fmt --check
cargo clippy --all-targets
cargo test --workspace        # 110 tests
```

Tests are hermetic: tempfiles, in-memory I/O, `127.0.0.1:0` — no services, network access, or fixtures required.

| Target | Command |
|---|---|
| End-to-end proof (two peers, real TLS) | `cargo test -p swiftdrop-core --test localhost_transfer` |
| Web upload path (raw HTTP) | `cargo test -p swiftdrop-core --test web_upload` |
| One crate | `cargo test -p swiftdrop-transfer` |
| One test | `cargo test -p swiftdrop-transfer state::tests::<name>` |

- **MSRV 1.88** (workspace `rust-version`, required by rcgen); developed on 1.99.
- Config/identity/trust store live in the platform config dir under `swiftdrop/`; override with `SWIFTDROP_CONFIG_DIR`. Destination folder is a `--dest` flag (no env override yet).
- `AGENTS.md` has the agent-facing detail: workspace rules, non-obvious constraints, and what not to "fix".
