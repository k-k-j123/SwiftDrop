# SwiftDrop — notes for agent sessions

Local-first AirDrop-style file transfer. Rust workspace today; the Flutter UI in `spec.md` §7.3 does **not** exist yet (`apps/` contains only `cli`).

## Source of truth

- `spec.md` (root) is the product/technical spec. Code comments cite it by ID (`FR-XFER-020`, `NFR-SEC-007`, `§9.2`) — keep citing spec IDs when implementing requirements.
- Code also cites `ADR-0002/0003/0007`, but no `docs/` or ADR files exist in this repo. Treat the module-level docs in `crates/*/src/lib.rs` as the design record.
- This directory is **not a git repository** (no history, branches, CI, README, or lint config as of 2026-10-19).

## Workspace layout and dependency rule

Strict downward dependencies (spec §7.4, mirrored in root `Cargo.toml`):

```
apps/cli  (binary `swiftdrop`)          — currently a 1-line placeholder with zero deps;
    ↓                                    `cargo run -p swiftdrop` does nothing yet
crates/core       orchestration facade (thin: most of it is stub/aspirational —
    ↓             `sessions.rs` is 27 lines; lib.rs describes intent, not code)
crates/discovery  mDNS `_swiftdrop._tcp.local.` + manual address fallback
crates/transfer   state machine, path sanitization, streaming, SHA-256 finalize
crates/security   TLS, device identity, SAS pairing, trust store
crates/protocol   wire format only — no networking, no filesystem, no UI
```

- Never add an edge upward; UI/CLI goes through `swiftdrop-core`'s public API only.
- Internal deps are declared once in root `[workspace.dependencies]` and pulled in as `dep = { workspace = true }`. Add new shared deps there, not per-crate versions.

## Commands

- Full verification: `cargo fmt --check` → `cargo clippy --all-targets` → `cargo test --workspace` (order per spec §16.2; no CI enforces it).
- Current status of those gates: `cargo fmt --check` **fails** (formatting drift, e.g. `crates/transfer/src/state.rs`); clippy passes with warnings; tests are inline `#[cfg(test)]` modules only — hermetic (tempfile, in-memory), no services, network, or fixtures required.
- Single crate: `cargo test -p swiftdrop-transfer`. Single test: `cargo test -p swiftdrop-transfer state::tests::<name>`.
- MSRV is 1.88 (workspace `rust-version`, required by rcgen); toolchain here is 1.99.

## Known breakage (as of 2026-10-19 — pre-existing, not caused by your edit)

- `cargo test --workspace` fails compiling `swiftdrop-security`: `crates/security/src/tls.rs` references `rustls::server::ServerSide` / `rustls::client::ClientSide`, which do not exist in the locked rustls 0.23.46 (they are `ServerConnectionData` / `ClientConnectionData` there).
- Until that's fixed, verify with: `cargo test -p swiftdrop-protocol -p swiftdrop-transfer -p swiftdrop-discovery` (passes).

## Non-obvious constraints (don't "fix" these)

- `rustls`/`tokio-rustls` are deliberately `default-features = false, features = ["ring", ...]`: TLS 1.3-only, and the `ring` backend avoids the cmake requirement of the default `aws-lc-rs`. Restoring default features will break builds.
- `crates/security/src/tls.rs` accepts **any** server certificate at the TLS layer on purpose: peer identity is enforced one layer up by signing the TLS exporter (RFC 5705) with the device identity key, then checking the trust store/SAS code. Looks like a vulnerability; it's reviewed design — see the module docs there.
- Sender-supplied filenames are hostile input: all sanitization goes through `crates/transfer/src/paths.rs`; the receiver must temp-file → hash → fsync → atomic rename (`file_sink.rs`). Never write to a sender-supplied path directly.
- State changes must go through `TransferStateTracker::transition` (`crates/transfer/src/state.rs`); ad-hoc state mutation is forbidden by spec §9.2.

## Documentation style

The project owner requires heavy rationale docs: every public type/function carries `///` docs explaining *why* (invariants, spec references, security reasoning), and logs must never contain secrets or file contents (spec NFR-SEC-007). Match that density; a bare `/// Returns the thing.` is below the house standard.
