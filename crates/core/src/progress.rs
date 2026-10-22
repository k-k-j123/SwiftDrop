//! Live byte counts for one transfer direction (spec FR-UI-004, §9.3).
//!
//! The streaming loops in [`crate::sessions`] know how far a transfer has
//! got; the UI needs to show it. This module is the narrow, dependency-free
//! bridge between them: a transfer owns a [`TransferProgress`], the session
//! updates it as bytes move, and whoever displays it takes a [`Snapshot`]
//! whenever it polls. Nothing is pushed anywhere, so neither side has to
//! know the other exists.
//!
//! ## Why atomics, and why no progress messages on the wire
//!
//! The bulk phase is the hot path (12 MB/s on WiFi, 500+ MB/s on
//! localhost), and progress is added once per data frame — thousands of
//! times per gigabyte. A relaxed atomic add costs a few nanoseconds and
//! takes no lock, so counting is invisible next to the syscalls it
//! accompanies. Sending progress frames to the peer is deliberately *not*
//! done: during the bulk phase the sender writes without reading, so
//! receiver-pushed progress would eventually deadlock two full socket
//! buffers against each other (see the module docs in `sessions`). Each
//! side counts locally instead, and no wire format changes.
//!
//! ## Two counters, never one
//!
//! Spec §9.3: "Do not equate bytes read by the sender with bytes durably
//! written by the receiver." So a sender's counter means *bytes handed to
//! the socket*, a receiver's means *bytes written to its temp file*, and
//! neither is used to claim the transfer succeeded — that still requires
//! the digest comparison in `swiftdrop-transfer`'s sink.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which way the bytes are moving for the transfer this counter belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Outbound: we are the sender.
    Send,
    /// Inbound: we are the receiver.
    Receive,
}

/// Where a transfer is right now, as the page renders it.
///
/// Only [`State::Sending`] and [`State::Receiving`] are live; the rest are
/// terminal and stay visible so the page can show how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    /// Nothing has run on this counter yet — the page shows nothing.
    Idle = 0,
    /// Bulk phase running, we are the sender.
    Sending = 1,
    /// Bulk phase running, we are the receiver.
    Receiving = 2,
    /// Our sender finished the bulk phase *and* the receiver verified it.
    Sent = 3,
    /// The received bytes were verified, fsynced, and renamed into place.
    Received = 4,
    /// A transfer that had started did not finish.
    Failed = 5,
}

impl State {
    /// Lowercase name for the JSON the page reads.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Sending => "sending",
            State::Receiving => "receiving",
            State::Sent => "sent",
            State::Received => "received",
            State::Failed => "failed",
        }
    }

    /// True while bytes are still moving — the page polls faster when this
    /// is set, so the bar looks live without wasting requests (FR-UI-004).
    pub fn is_active(self) -> bool {
        matches!(self, State::Sending | State::Receiving)
    }

    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => State::Sending,
            2 => State::Receiving,
            3 => State::Sent,
            4 => State::Received,
            5 => State::Failed,
            _ => State::Idle,
        }
    }
}

/// One transfer's numbers at the moment it was read.
///
/// Plain integers, so a poller can copy it and format at leisure without
/// holding anything or observing a torn read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// Where the transfer is.
    pub state: State,
    /// Bytes moved so far (sender: written to the socket; receiver: written
    /// to the temp file).
    pub bytes: u64,
    /// Bytes the transfer will move in total, known from the offer.
    pub total: u64,
    /// Milliseconds since [`TransferProgress::begin`]; 0 before the first
    /// one. The page divides by this to show a rate for a transfer it did
    /// not watch from the start (e.g. a reloaded tab).
    pub elapsed_ms: u64,
}

/// A shared, lock-free byte counter for one transfer.
///
/// Owned by whichever caller started the session (the web UI keeps one per
/// send and one per receive loop) and updated by [`crate::sessions`] from
/// the bulk loop. All methods take `&self`, so the owner reads it through
/// an `Arc` while the session writes.
#[derive(Debug, Default)]
pub struct TransferProgress {
    /// One of [`State`]'s discriminants.
    state: AtomicU8,
    bytes: AtomicU64,
    total: AtomicU64,
    /// Unix epoch milliseconds at [`Self::begin`]; 0 means "never started",
    /// which keeps an untouched counter distinguishable from a transfer
    /// that has merely moved no bytes yet.
    started_ms: AtomicU64,
}

impl TransferProgress {
    /// A counter that has never run ([`State::Idle`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the start of a transfer: reset the byte count and the clock.
    ///
    /// Called once per transfer, after the offer is accepted and before the
    /// first byte moves, so the elapsed time is the transfer's own and not
    /// the process's (spec §9.3 asks for timestamps per transfer).
    pub fn begin(&self, direction: Direction, total: u64) {
        self.bytes.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
        self.started_ms.store(now_ms(), Ordering::Relaxed);
        let state = match direction {
            Direction::Send => State::Sending,
            Direction::Receive => State::Receiving,
        };
        self.state.store(state as u8, Ordering::Relaxed);
    }

    /// Add one chunk of progress.
    ///
    /// Called once per data frame/chunk — keep it branch-free and
    /// lock-free. Deliberately does nothing else: no logging (a 10 GB
    /// transfer would emit tens of thousands of lines, and spec NFR-SEC-007
    /// keeps file detail out of logs anyway).
    pub fn add(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Mark the transfer finished, successfully or not.
    ///
    /// A no-op unless a transfer is *currently* running. That matters:
    /// every session calls this on its way out, including sessions that
    /// never transferred anything (a declined pairing, a rejected offer, a
    /// failed TLS handshake). Without the guard, declining a prompt would
    /// repaint the last successful transfer as `Failed`.
    pub fn finish(&self, ok: bool) {
        let current = State::from_u8(self.state.load(Ordering::Relaxed));
        let end = match current {
            State::Sending => {
                if ok {
                    State::Sent
                } else {
                    State::Failed
                }
            }
            State::Receiving => {
                if ok {
                    State::Received
                } else {
                    State::Failed
                }
            }
            _ => return,
        };
        self.state.store(end as u8, Ordering::Relaxed);
    }

    /// Read the current numbers. Cheap and non-blocking; safe to call on
    /// every status poll.
    pub fn snapshot(&self) -> Snapshot {
        let started = self.started_ms.load(Ordering::Relaxed);
        Snapshot {
            state: State::from_u8(self.state.load(Ordering::Relaxed)),
            bytes: self.bytes.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            elapsed_ms: if started == 0 {
                0
            } else {
                now_ms().saturating_sub(started)
            },
        }
    }
}

/// Milliseconds since the Unix epoch, or 0 if the clock predates it.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh counter shows nothing, and stays that way through `finish`
    /// — a session that declined a prompt must not paint a failure.
    #[test]
    fn idle_counter_survives_finish() {
        let p = TransferProgress::new();
        assert_eq!(p.snapshot().state, State::Idle);
        p.finish(false);
        assert_eq!(p.snapshot().state, State::Idle, "idle is not a failure");
    }

    /// begin → add → finish is the whole lifecycle the sessions follow.
    #[test]
    fn counts_bytes_between_begin_and_finish() {
        let p = TransferProgress::new();
        p.begin(Direction::Send, 1000);
        p.add(600);
        p.add(400);
        let mid = p.snapshot();
        assert_eq!((mid.bytes, mid.total), (1000, 1000));
        assert_eq!(mid.state, State::Sending);
        assert!(mid.state.is_active());
        p.finish(true);
        assert_eq!(p.snapshot().state, State::Sent);
        assert!(!p.snapshot().state.is_active());
    }

    /// The receiver's counter is a separate vocabulary: it ends `Received`,
    /// and a failure ends `Failed` whichever direction it was.
    #[test]
    fn receiving_ends_received_and_failures_end_failed() {
        let ok = TransferProgress::new();
        ok.begin(Direction::Receive, 10);
        ok.finish(true);
        assert_eq!(ok.snapshot().state, State::Received);

        let bad = TransferProgress::new();
        bad.begin(Direction::Receive, 10);
        bad.finish(false);
        assert_eq!(bad.snapshot().state, State::Failed);
    }

    /// A finished transfer keeps its result until the next one starts, and
    /// a new transfer resets both the bytes and the clock.
    #[test]
    fn begin_resets_a_finished_transfer() {
        let p = TransferProgress::new();
        p.begin(Direction::Send, 10);
        p.add(10);
        p.finish(true);
        p.finish(false); // late/completion-phase failure must not repaint it
        assert_eq!(p.snapshot().state, State::Sent);

        p.begin(Direction::Send, 3);
        let fresh = p.snapshot();
        assert_eq!(
            (fresh.state, fresh.bytes, fresh.total),
            (State::Sending, 0, 3)
        );
        assert!(
            fresh.elapsed_ms < 60_000,
            "clock restarts with the transfer"
        );
    }
}
