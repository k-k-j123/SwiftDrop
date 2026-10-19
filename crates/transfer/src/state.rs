//! The transfer lifecycle state machine — spec §9.
//!
//! ```text
//! CREATED -> OFFERED -> AWAITING_APPROVAL -> ACCEPTED -> TRANSFERRING
//!   -> VERIFYING -> COMPLETED
//!
//! OFFERED / AWAITING_APPROVAL -> REJECTED
//! any active state            -> CANCELLING -> CANCELLED
//! any nonterminal state       -> FAILED
//! TRANSFERRING                -> INTERRUPTED   (future resume support)
//! INTERRUPTED                 -> ACCEPTED / TRANSFERRING (future support)
//! ```
//!
//! Rules enforced here (spec §9.2):
//!
//! * Transitions are validated centrally — **never** mutated ad hoc.
//! * `COMPLETED` is reachable only from `VERIFYING` (i.e. only after
//!   integrity verification succeeded).
//! * `REJECTED`, `CANCELLED`, `FAILED` are terminal and are **never**
//!   represented as completed (FR-XFER-008).
//! * Duplicate/idempotent commands are handled by callers asking
//!   `can_transition` before attempting the same move twice.

use serde::{Deserialize, Serialize};
use std::time::Instant;

/// The lifecycle states from spec §9.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferState {
    Created,
    Offered,
    AwaitingApproval,
    Accepted,
    Transferring,
    Verifying,
    /// Terminal: user approved path finished and verified.
    Completed,
    /// Terminal: receiver's user declined the offer.
    Rejected,
    /// In-flight cancellation requested, cleanup running.
    Cancelling,
    /// Terminal: cancelled by a user or the OS.
    Cancelled,
    /// Terminal: any failure (network, disk, integrity, protocol…).
    Failed,
    /// Connection lost mid-transfer (post-MVP resume may continue from
    /// here; in the MVP it advances to `FAILED` with cleanup).
    Interrupted,
}

impl TransferState {
    /// Terminal states allow no further transitions.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Rejected | Self::Cancelled | Self::Failed
        )
    }

    /// Is the transfer still consuming resources / needing attention?
    pub fn is_active(self) -> bool {
        !self.is_terminal()
    }

    /// The exact transition table from spec §9.1.
    ///
    /// `from` → `to` is legal iff it appears here. Anything else —
    /// including *backwards* moves out of terminal states — is refused.
    pub fn can_transition(from: Self, to: Self) -> bool {
        use TransferState::*;
        if from.is_terminal() {
            return false; // terminal means terminal (spec §9.2)
        }
        match (from, to) {
            // The happy path.
            (Created, Offered)
            | (Offered, AwaitingApproval)
            | (AwaitingApproval, Accepted)
            | (Accepted, Transferring)
            | (Transferring, Verifying)
            | (Verifying, Completed) => true,

            // Explicit rejection while approval is pending.
            (Offered, Rejected) | (AwaitingApproval, Rejected) => true,

            // Cancellation funnel: any active state → Cancelling → Cancelled.
            // (Created is excluded: nothing was ever offered, so there is
            // nothing to cancel — callers simply drop it.)
            (
                Offered | AwaitingApproval | Accepted | Transferring | Verifying | Interrupted,
                Cancelling,
            ) => true,
            (Cancelling, Cancelled) => true,

            // Any nonterminal state may fail at any time.
            (Cancelling, Failed) => true,
            (_, Failed) => true,

            // Interruption + future resume paths (spec §9.1 tail).
            (Transferring, Interrupted) => true,
            (Interrupted, Accepted) | (Interrupted, Transferring) => true,

            _ => false,
        }
    }
}

/// Guarded holder of a transfer's current state + transition timestamps.
#[derive(Debug, Clone)]
pub struct TransferStateTracker {
    state: TransferState,
    /// When the current state was entered (drives timeout checks).
    since: Instant,
    /// Full history of transitions — surfaced in diagnostics and tests.
    history: Vec<(TransferState, Instant)>,
}

/// A refused transition. Carries enough context for a useful log line.
#[derive(Debug, thiserror::Error)]
#[error("illegal transfer transition {from:?} -> {to:?}")]
pub struct StateError {
    pub from: TransferState,
    pub to: TransferState,
}

impl TransferStateTracker {
    pub fn new() -> Self {
        Self {
            state: TransferState::Created,
            since: Instant::now(),
            history: vec![(TransferState::Created, Instant::now())],
        }
    }

    pub fn state(&self) -> TransferState {
        self.state
    }

    /// How long we have been in the current state.
    pub fn time_in_state(&self) -> std::time::Duration {
        self.since.elapsed()
    }

    pub fn history(&self) -> &[(TransferState, Instant)] {
        &self.history
    }

    /// Attempt a transition; refuse anything the table forbids.
    pub fn transition(&mut self, to: TransferState) -> Result<(), StateError> {
        if !TransferState::can_transition(self.state, to) {
            return Err(StateError {
                from: self.state,
                to,
            });
        }
        self.state = to;
        self.since = Instant::now();
        self.history.push((to, Instant::now()));
        Ok(())
    }

    /// Idempotent helper: `transition` if legal, `Ok(())` if it is a
    /// duplicate of the current state (spec §9.2: duplicate commands are
    /// handled idempotently where possible).
    pub fn transition_idempotent(&mut self, to: TransferState) -> Result<(), StateError> {
        if self.state == to {
            return Ok(());
        }
        self.transition(to)
    }

    /// Try to move, falling back to `fallback` when the primary move is
    /// illegal (used when converting errors into terminal states: e.g.
    /// something already `Cancelling` that hits an I/O error still ends in
    /// `Cancelled`, not `Failed`).
    pub fn transition_or(&mut self, preferred: TransferState, fallback: TransferState) {
        if self.transition(preferred).is_err() && self.state != preferred {
            let _ = self.transition(fallback);
        }
    }
}

impl Default for TransferStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full happy path from spec §9.1 must work end to end.
    #[test]
    fn happy_path_reaches_completed() {
        let mut t = TransferStateTracker::new();
        for next in [
            TransferState::Offered,
            TransferState::AwaitingApproval,
            TransferState::Accepted,
            TransferState::Transferring,
            TransferState::Verifying,
            TransferState::Completed,
        ] {
            t.transition(next).unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(t.state(), TransferState::Completed);
        assert!(t.state().is_terminal());
    }

    /// COMPLETED must be reachable ONLY via VERIFYING — a transfer can
    /// never jump from transferring to completed without verification
    /// (spec FR-XFER-012 / NFR-REL-001).
    #[test]
    fn completed_requires_verification() {
        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition(TransferState::AwaitingApproval).unwrap();
        t.transition(TransferState::Accepted).unwrap();
        t.transition(TransferState::Transferring).unwrap();
        assert!(
            t.transition(TransferState::Completed).is_err(),
            "must not complete without verifying"
        );
    }

    /// Failures, rejections, and cancellations are terminal and can never
    /// become COMPLETED afterwards (spec FR-XFER-008).
    #[test]
    fn bad_ends_never_become_completed() {
        for bad in [
            TransferState::Failed,
            TransferState::Rejected,
            TransferState::Cancelled,
        ] {
            let mut t = TransferStateTracker::new();
            t.transition(TransferState::Offered).unwrap();
            // Move into the bad terminal state via a legal route.
            match bad {
                TransferState::Rejected => t.transition(TransferState::Rejected).unwrap(),
                TransferState::Cancelled => {
                    t.transition(TransferState::Cancelling).unwrap();
                    t.transition(TransferState::Cancelled).unwrap();
                }
                _ => t.transition(TransferState::Failed).unwrap(),
            }
            assert!(t.transition(TransferState::Completed).is_err(), "{bad:?} -> Completed must be refused");
            assert!(t.state().is_terminal());
        }
    }

    /// Rejection legal from OFFERED and AWAITING_APPROVAL only.
    #[test]
    fn rejection_only_before_acceptance() {
        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition(TransferState::Rejected).unwrap();

        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition(TransferState::AwaitingApproval).unwrap();
        t.transition(TransferState::Rejected).unwrap();

        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition(TransferState::AwaitingApproval).unwrap();
        t.transition(TransferState::Accepted).unwrap();
        assert!(t.transition(TransferState::Rejected).is_err());
    }

    /// "Any active state → CANCELLING → CANCELLED".
    #[test]
    fn cancellation_from_any_active_state() {
        for start in [
            TransferState::Offered,
            TransferState::AwaitingApproval,
            TransferState::Accepted,
            TransferState::Transferring,
            TransferState::Verifying,
        ] {
            let mut t = TransferStateTracker::new();
            t.transition(TransferState::Offered).unwrap();
            if start == TransferState::AwaitingApproval
                || start == TransferState::Accepted
                || start == TransferState::Transferring
                || start == TransferState::Verifying
            {
                t.transition(TransferState::AwaitingApproval).unwrap();
            }
            if matches!(
                start,
                TransferState::Accepted | TransferState::Transferring | TransferState::Verifying
            ) {
                t.transition(TransferState::Accepted).unwrap();
            }
            if matches!(start, TransferState::Transferring | TransferState::Verifying) {
                t.transition(TransferState::Transferring).unwrap();
            }
            if start == TransferState::Verifying {
                t.transition(TransferState::Verifying).unwrap();
            }
            assert_eq!(t.state(), start, "setup should land on {start:?}");

            t.transition(TransferState::Cancelling)
                .unwrap_or_else(|e| panic!("cancel from {start:?}: {e}"));
            t.transition(TransferState::Cancelled)
                .unwrap_or_else(|e| panic!("finish cancel from {start:?}: {e}"));
        }
    }

    /// Terminal states reject everything, including themselves.
    #[test]
    fn terminal_states_are_sticky() {
        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition(TransferState::Failed).unwrap();
        for to in [
            TransferState::Completed,
            TransferState::Failed,
            TransferState::Cancelling,
            TransferState::Transferring,
        ] {
            assert!(t.transition(to).is_err(), "Failed -> {to:?} must be refused");
        }
    }

    /// Duplicate commands are idempotent (spec §9.2).
    #[test]
    fn duplicate_transition_is_idempotent() {
        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition_idempotent(TransferState::Offered).unwrap();
        // But a different illegal move still fails.
        assert!(t.transition_idempotent(TransferState::Completed).is_err());
    }

    /// Resume path (future feature, spec §9.1) is wired but unused in MVP.
    #[test]
    fn interrupted_resume_paths_exist() {
        let mut t = TransferStateTracker::new();
        t.transition(TransferState::Offered).unwrap();
        t.transition(TransferState::AwaitingApproval).unwrap();
        t.transition(TransferState::Accepted).unwrap();
        t.transition(TransferState::Transferring).unwrap();
        t.transition(TransferState::Interrupted).unwrap();
        t.transition(TransferState::Transferring).unwrap();
    }
}
