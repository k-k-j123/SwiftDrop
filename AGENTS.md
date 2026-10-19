# SwiftDrop — notes for agent sessions

Local-first AirDrop-style file transfer. Rust workspace today; the Flutter UI in `spec.md` §7.3 does **not** exist yet (`apps/` contains only `cli`).

## Source of truth

- `spec.md` (root) is the product/technical spec. Code comments cite it by ID (`FR-XFER-020`, `NFR-SEC-007`, `§9.2`) — keep citing spec IDs when implementing requirements.
- Code also cites `ADR-0002/0003/0007`, but no `docs/` or ADR files exist in this repo. Treat the module-level docs in `crates/*/src/lib.rs` as the design record.
- This directory is **not a git repository** (no history, branches, CI, README, or lint config as of 2026-10-19).

## Workspace layout and dependency rule

Strict downward dependencies (spec §7.4, mirrored in root `Cargo.toml`):

```
apps/cli  (binary `swiftdrop`)          — real CLI: `send`/`receive`/`serve` verbs, manual arg parse,
    ↓                                    `--yes` flag, stdin TerminalPrompt; throughput printed after send
crates/core       orchestration facade — sessions.rs implements the full connection lifecycle
    ↓             (TLS accept/connect, exporter-bound auth, SAS pairing, offer/accept, streaming,
crates/discovery  mDNS `_swiftdrop._tcp.local.` + manual address fallback   integrity, completion)
crates/transfer   state machine, path sanitization, streaming, SHA-256 finalize
crates/security   TLS, device identity, SAS pairing, trust store
crates/protocol   wire format only — no networking, no filesystem, no UI
```

- Never add an edge upward; UI/CLI goes through `swiftdrop-core`'s public API only.
- Internal deps are declared once in root `[workspace.dependencies]` and pulled in as `dep = { workspace = true }`. Add new shared deps there, not per-crate versions.
- `core/src/web.rs` is a deliberate exception to protocol purity: a hand-rolled HTTP/1.1 upload server for phone browsers (they can't speak the SwiftDrop wire protocol). **Zero new dependencies**, no auth/TLS on that path by the owner's explicit speed-first choice — read its module docs before "securing" it.

## Commands

- Full verification: `cargo fmt --check` → `cargo clippy --all-targets` → `cargo test --workspace` (order per spec §16.2; no CI enforces it).
- Current status of those gates (2026-10-19): `cargo fmt --check` passes; clippy passes with warnings only; all tests green (110 total: 21 protocol + 19 security + 13 discovery + 50 transfer + 2 end-to-end localhost + 5 web upload). Tests are hermetic (tempfile, in-memory, `127.0.0.1:0`) — no services, network access, or fixtures required.
- Single crate: `cargo test -p swiftdrop-transfer`. Single test: `cargo test -p swiftdrop-transfer state::tests::<name>`. End-to-end: `cargo test -p swiftdrop-core --test localhost_transfer` (two in-process peers over real TLS — the §21 first-slice proof). Web path: `cargo test -p swiftdrop-core --test web_upload` (raw HTTP over `127.0.0.1:0` — byte-identical 16 MiB landing, unicode names, short-body abort, name containment).
- MSRV is 1.88 (workspace `rust-version`, required by rcgen); toolchain here is 1.99.

## Known breakage

- None as of 2026-10-19. The previous rustls `ServerSide`/`ClientSide` compile error in `crates/security/src/tls.rs` is fixed (`ServerConnectionData` / `ClientConnectionData`).

## Non-obvious constraints (don't "fix" these)

- `rustls`/`tokio-rustls` are deliberately `default-features = false, features = ["ring", ...]`: TLS 1.3-only, and the `ring` backend avoids the cmake requirement of the default `aws-lc-rs`. Restoring default features will break builds.
- `crates/security/src/tls.rs` accepts **any** server certificate at the TLS layer on purpose: peer identity is enforced one layer up by signing the TLS exporter (RFC 5705) with the device identity key, then checking the trust store/SAS code. Looks like a vulnerability; it's reviewed design — see the module docs there.
- Sender-supplied filenames are hostile input: all sanitization goes through `crates/transfer/src/paths.rs`; the receiver must temp-file → hash → fsync → atomic rename (`file_sink.rs`). Never write to a sender-supplied path directly.
- State changes must go through `TransferStateTracker::transition` (`crates/transfer/src/state.rs`); ad-hoc state mutation is forbidden by spec §9.2.

## Documentation style

The project owner requires heavy rationale docs: every public type/function carries `///` docs explaining *why* (invariants, spec references, security reasoning), and logs must never contain secrets or file contents (spec NFR-SEC-007). Match that density; a bare `/// Returns the thing.` is below the house standard.
