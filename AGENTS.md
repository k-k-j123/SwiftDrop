# SwiftDrop — notes for agent sessions

Local-first AirDrop-style file transfer, as a Rust workspace. **The app is the web UI**: `swiftdrop serve` runs a hand-rolled HTTP server and serves one embedded HTML page — send to another computer, receive from one, and let a phone's browser upload. There is no Flutter client and no mDNS discovery; both were removed by owner decision on 2026-10-19 (status note in `spec.md` §7).

## Source of truth

- `spec.md` (root) is the product/technical spec. Code comments cite it by ID (`FR-XFER-020`, `NFR-SEC-007`, `§9.2`) — keep citing spec IDs when implementing requirements.
- Code also cites `ADR-0002/0003/0007`, but no `docs/` or ADR files exist in this repo. Treat the module-level docs in `crates/*/src/lib.rs` as the design record.
- This is a git repository (it was not, early on). No CI, no lint config; the working tree usually carries uncommitted work.

## Workspace layout and dependency rule

Strict downward dependencies (spec §7.4, mirrored in root `Cargo.toml`):

```
apps/cli  (binary `swiftdrop`)          — `serve` (the app) / `send` / `receive`, manual arg parse,
    ↓                                    `--yes` and `--open` flags, stdin TerminalPrompt
crates/core       orchestration facade — sessions.rs: full connection lifecycle (TLS accept/connect,
    ↓             exporter-bound auth, SAS pairing, offer/accept, streaming, integrity, completion);
                  web.rs: the app UI — HTTP server, embedded page, browser prompt registry;
                  progress.rs: lock-free live byte counters (send/receive) the page polls for its bar
crates/transfer   state machine, path sanitization, streaming, SHA-256 finalize
crates/security   TLS, device identity, SAS pairing, trust store
crates/protocol   wire format only — no networking, no filesystem, no UI
```

- Never add an edge upward; UI/CLI goes through `swiftdrop-core`'s public API only.
- Internal deps are declared once in root `[workspace.dependencies]` and pulled in as `dep = { workspace = true }`. Add new shared deps there, not per-crate versions.
- `core/src/web.rs` is a deliberate exception to protocol purity — it *is* the product UI (hand-rolled HTTP/1.1, **zero new dependencies**, embedded page). Two postures share the one port: `/api/send`, `/api/receive/*`, `/api/prompt` drive the real authenticated protocol, while `/upload` and the rest of the HTTP surface are unauthenticated by the owner's explicit speed-first choice. Read its module docs before "securing" it.

## Commands

- Full verification: `cargo fmt --check` → `cargo clippy --all-targets` → `cargo test --workspace` (order per spec §16.2; no CI enforces it).
- Gate status (2026-10-19): fmt clean; clippy 0 errors, warnings only (all pre-existing); **95 tests green** = 9 core unit + 3 end-to-end localhost + 9 web UI + 19 protocol + 13 security + 42 transfer. Tests are hermetic (tempfile, in-memory, `127.0.0.1:0`) — no services, network access, or fixtures required.
- `./install.sh` installs the release binary (`cargo install --path apps/cli`) plus an app-menu launcher and icon from `packaging/` (the launcher's `Exec` gets the binary's absolute path — desktop sessions don't inherit `~/.cargo/bin`). Then `swiftdrop serve --open` runs the app.
- Single crate: `cargo test -p swiftdrop-transfer`. Single test: `cargo test -p swiftdrop-transfer state::tests::<name>`. End-to-end P2P: `cargo test -p swiftdrop-core --test localhost_transfer` (two in-process peers over real TLS — the §21 first-slice proof). Web UI: `cargo test -p swiftdrop-core --test web_upload` (raw HTTP over `127.0.0.1:0` — page content, byte-identical 16 MiB upload, unicode names, short-body abort, name containment, status/receive controls, and a full send→receive driven through `/api/send` + `/api/prompt`).
- MSRV is 1.88 (workspace `rust-version`, required by rcgen); toolchain here is 1.99.

## Known breakage

- None as of 2026-10-19. The previous rustls `ServerSide`/`ClientSide` compile error in `crates/security/src/tls.rs` is fixed (`ServerConnectionData` / `ClientConnectionData`).

## Non-obvious constraints (don't "fix" these)

- `rustls`/`tokio-rustls` are deliberately `default-features = false, features = ["ring", ...]`: TLS 1.3-only, and the `ring` backend avoids the cmake requirement of the default `aws-lc-rs`. Restoring default features will break builds.
- `crates/security/src/tls.rs` accepts **any** server certificate at the TLS layer on purpose: peer identity is enforced one layer up by signing the TLS exporter (RFC 5705) with the device identity key, then checking the trust store/SAS code. Looks like a vulnerability; it's reviewed design — see the module docs there.
- Sender-supplied filenames are hostile input: all sanitization goes through `crates/transfer/src/paths.rs`; the receiver must temp-file → hash → fsync → atomic rename (`file_sink.rs`). Never write to a sender-supplied path directly. Same rule in the UI: peer names and filenames are rendered with `textContent`, never `innerHTML`.
- State changes must go through `TransferStateTracker::transition` (`crates/transfer/src/state.rs`); ad-hoc state mutation is forbidden by spec §9.2.
- The unauthenticated HTTP surface is a stated owner trade-off, not an oversight: anyone on the LAN can reach every endpoint on the port. Don't add a PIN/TLS "fix" without being asked; do add the `shortcut:` comment and the module doc if the trade-off grows.
- Send staging (`config_dir/staging`) is disposable: files are deleted after the send attempt and cleaned at startup. Don't "keep them for retry" without also revisiting that cleanup.

## Documentation style

The project owner requires heavy rationale docs: every public type/function carries `///` docs explaining *why* (invariants, spec references, security reasoning), and logs must never contain secrets or file contents (spec NFR-SEC-007). Match that density; a bare `/// Returns the thing.` is below the house standard.
