/// The current transaction activity for a database connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i32)]
pub enum TransactionState {
    /// No transaction currently accesses the selected database.
    None = 0,
    /// A transaction has read from the selected database.
    Read = 1,
    /// A transaction has written to the selected database.
    Write = 2,
}

/// The locking and WAL-reuse behavior requested for a checkpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i32)]
pub enum CheckpointMode {
    /// Copy every currently available frame without waiting for readers or
    /// writers.
    Passive = 0,
    /// Wait for writers, then copy every frame available to current readers.
    Full = 1,
    /// Perform a full checkpoint and wait until the WAL can be reused.
    Restart = 2,
    /// Perform a restart checkpoint and truncate the WAL file to zero bytes.
    Truncate = 3,
}

/// Progress reported by one WAL checkpoint attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CheckpointResult {
    wal_frames: Option<u32>,
    checkpointed_frames: Option<u32>,
    busy: bool,
}

impl CheckpointResult {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(
        wal_frames: Option<u32>,
        checkpointed_frames: Option<u32>,
        busy: bool,
    ) -> Self {
        Self {
            wal_frames,
            checkpointed_frames,
            busy,
        }
    }

    /// Returns the number of frames observed in the WAL, when available.
    #[must_use]
    pub const fn wal_frames(self) -> Option<u32> {
        self.wal_frames
    }

    /// Returns the number of frames copied into the database, when available.
    #[must_use]
    pub const fn checkpointed_frames(self) -> Option<u32> {
        self.checkpointed_frames
    }

    /// Returns whether the checkpoint could not acquire a required lock.
    #[must_use]
    pub const fn is_busy(self) -> bool {
        self.busy
    }

    /// Returns the number of WAL frames left behind by this attempt.
    #[must_use]
    pub const fn remaining_frames(self) -> Option<u32> {
        match (self.wal_frames, self.checkpointed_frames) {
            (Some(wal), Some(checkpointed)) => Some(wal.saturating_sub(checkpointed)),
            _ => None,
        }
    }

    /// Returns whether the attempt copied every WAL frame it observed.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        !self.busy && matches!(self.remaining_frames(), Some(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_progress_distinguishes_busy_incomplete_and_no_wal() {
        let incomplete = CheckpointResult::new(Some(12), Some(7), false);
        assert_eq!(incomplete.remaining_frames(), Some(5));
        assert!(!incomplete.is_busy());
        assert!(!incomplete.is_complete());

        let busy = CheckpointResult::new(Some(12), Some(7), true);
        assert!(busy.is_busy());
        assert!(!busy.is_complete());

        let no_wal = CheckpointResult::new(None, None, false);
        assert_eq!(no_wal.remaining_frames(), None);
        assert!(!no_wal.is_complete());
    }
}
