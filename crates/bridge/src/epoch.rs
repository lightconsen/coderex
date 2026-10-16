//! Worker epoch write fence.
//!
//! More than one worker can serve the same session across a restart, a
//! reconnect, or a hand-over: the gateway starts a replacement while the old
//! worker is still finishing its last request. The replacement is authoritative
//! from the moment it starts, so the old worker's write-back must not land on
//! top of it — otherwise a session's state can go backwards, and the last write
//! wins by arrival time rather than by who is actually serving the session.
//!
//! The gateway hands each worker a monotonically increasing `worker_epoch`. A
//! [`WriteFence`] remembers the newest epoch it has seen and refuses anything
//! older. Equal epochs are the same worker writing again, which is allowed;
//! a newer epoch takes over.

use serde::{Deserialize, Serialize};

/// A write from a worker whose epoch has already been superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("worker epoch {presented} is stale: epoch {current} has already written")]
pub struct StaleEpoch {
    /// The epoch the refused write presented.
    pub presented: u64,
    /// The newest epoch this fence has admitted.
    pub current: u64,
}

/// A monotonic fence over a worker's epoch.
///
/// One fence guards one session's state: a write is admitted only if it comes
/// from the newest worker to have written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFence {
    epoch: u64,
}

impl WriteFence {
    /// Create a fence that has admitted no write yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { epoch: 0 }
    }

    /// The newest epoch this fence has admitted.
    #[must_use]
    pub const fn current(&self) -> u64 {
        self.epoch
    }

    /// Admit a write from `epoch`.
    ///
    /// A newer epoch advances the fence and is admitted; the same epoch is the
    /// same worker writing again and is admitted; an older epoch is refused
    /// and leaves the fence where it was.
    pub const fn admit(&mut self, epoch: u64) -> Result<(), StaleEpoch> {
        if epoch < self.epoch {
            return Err(StaleEpoch {
                presented: epoch,
                current: self.epoch,
            });
        }
        self.epoch = epoch;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_epoch_takes_the_fence() {
        let mut fence = WriteFence::new();
        assert_eq!(fence.current(), 0);

        assert!(fence.admit(4).is_ok());
        assert_eq!(fence.current(), 4);
    }

    #[test]
    fn a_stale_epoch_is_refused_and_changes_nothing() {
        let mut fence = WriteFence::new();
        fence.admit(4).unwrap();

        let refused = fence.admit(3);
        assert_eq!(
            refused,
            Err(StaleEpoch {
                presented: 3,
                current: 4
            })
        );
        assert_eq!(
            fence.current(),
            4,
            "a refused write must not move the fence"
        );
    }

    #[test]
    fn the_same_worker_may_write_again() {
        let mut fence = WriteFence::new();
        fence.admit(4).unwrap();
        assert!(fence.admit(4).is_ok());
        assert_eq!(fence.current(), 4);
    }

    #[test]
    fn a_refusal_says_which_epoch_is_current() {
        let mut fence = WriteFence::new();
        fence.admit(7).unwrap();
        let refused = fence.admit(1).unwrap_err();
        assert_eq!(refused.presented, 1);
        assert_eq!(refused.current, 7);
        assert!(refused.to_string().contains("stale"));
    }
}
