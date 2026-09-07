//! Windows production IdSource adapter for PALKA service runtime.

use std::collections::HashSet;

use palka_core::TimerId;
use palka_windows_platform::{WindowsIdGeneratorError, WindowsIdSequence};

use crate::persistence::{OutboxEntryId, PersistentState};
use crate::runtime::{IdSource, PlatformError};

impl From<WindowsIdGeneratorError> for PlatformError {
    fn from(err: WindowsIdGeneratorError) -> Self {
        match err {
            WindowsIdGeneratorError::UnsupportedPlatform => {
                Self::new("WindowsIdGeneratorError: UnsupportedPlatform")
            }
            WindowsIdGeneratorError::RandomSeedGenerationFailure { ntstatus } => {
                Self::new(format!(
                    "WindowsIdGeneratorError: RandomSeedGenerationFailure (ntstatus: {ntstatus})"
                ))
            }
        }
    }
}

/// Internal sequence source trait used to decouple the service adapter from the concrete sequence.
/// Allows deterministic unit tests without exposing a public test seam in production.
pub(crate) trait IdSequenceSource: Send + Sync {
    fn next_128(&self) -> [u8; 16];
}

impl IdSequenceSource for WindowsIdSequence {
    fn next_128(&self) -> [u8; 16] {
        self.next_128()
    }
}

/// Production adapter implementing the infallible `IdSource` runtime port.
pub struct WindowsIdSourceAdapter {
    sequence: Box<dyn IdSequenceSource>,
    pub(crate) reserved_ids: HashSet<[u8; 16]>,
}

impl WindowsIdSourceAdapter {
    /// Constructs a production adapter from validated persistent state.
    pub fn from_production(state: &PersistentState) -> Result<Self, PlatformError> {
        let sequence = WindowsIdSequence::from_production().map_err(PlatformError::from)?;
        Ok(Self::from_sequence_and_state(Box::new(sequence), state))
    }

    /// Internal constructor taking an abstract sequence source and persistent state.
    pub(crate) fn from_sequence_and_state(
        sequence: Box<dyn IdSequenceSource>,
        state: &PersistentState,
    ) -> Self {
        let mut reserved_ids = HashSet::new();
        for action in &state.active_actions {
            reserved_ids.insert(action.id.0);
        }
        for entry in &state.telegram_outbox {
            reserved_ids.insert(entry.entry_id.0);
        }
        Self {
            sequence,
            reserved_ids,
        }
    }

    /// Generates the next candidate 128-bit identifier, skipping any reserved IDs.
    fn next_unreserved(&self) -> [u8; 16] {
        loop {
            let candidate = self.sequence.next_128();
            if self.reserved_ids.contains(&candidate) {
                continue;
            }
            return candidate;
        }
    }
}

impl IdSource for WindowsIdSourceAdapter {
    fn next_timer_id(&self) -> TimerId {
        TimerId(self.next_unreserved())
    }

    fn next_outbox_id(&self) -> OutboxEntryId {
        OutboxEntryId(self.next_unreserved())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{
        OutboxEntryId, PersistentState, TelegramOutboxEntry, TelegramPayload,
    };
    use palka_core::{
        ActionExecutionState, ActionKind, Deadline, DesiredInternetState, Initiator,
        ScheduledAction, UtcDateTime,
    };
    use std::sync::Mutex;

    struct FakeDeterministicSequence {
        state: Mutex<u128>,
    }

    impl FakeDeterministicSequence {
        fn new(seed: u128) -> Self {
            Self {
                state: Mutex::new(seed),
            }
        }
    }

    impl IdSequenceSource for FakeDeterministicSequence {
        fn next_128(&self) -> [u8; 16] {
            let mut guard = self.state.lock().unwrap();
            let current = *guard;
            *guard = current.wrapping_add(1);
            current.to_be_bytes()
        }
    }

    fn make_empty_state() -> PersistentState {
        PersistentState {
            desired_internet_state: DesiredInternetState::Unrestricted,
            active_actions: Vec::new(),
            internet_retry: None,
            telegram_outbox: Vec::new(),
        }
    }

    fn make_scheduled_action(id_num: u128) -> ScheduledAction {
        ScheduledAction {
            id: TimerId(id_num.to_be_bytes()),
            action_kind: ActionKind::BlockInternet,
            deadline: Deadline(UtcDateTime(1700000000000)),
            created_at: UtcDateTime(1700000000000),
            created_by: Initiator::ParentTelegram { user_id: 123456789 },
            emitted_thresholds: HashSet::new(),
            execution_state: ActionExecutionState::Pending,
        }
    }

    fn make_outbox_entry(id_num: u128) -> TelegramOutboxEntry {
        TelegramOutboxEntry {
            entry_id: OutboxEntryId(id_num.to_be_bytes()),
            payload: TelegramPayload::ServiceNotification {
                text: "test".to_string(),
            },
            attempt_count: 0,
            last_error: None,
        }
    }

    #[test]
    fn test_assert_send_sync() {
        // IDS-27: Trait and adapter Send + Sync static assertion
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowsIdSourceAdapter>();
        assert_send_sync::<Box<dyn IdSource>>();
    }

    #[test]
    fn test_a_b_c_trait_delegation_and_shared_sequence() {
        // A. IdSource trait delegation produces TimerId (IDS-01)
        // B. IdSource trait delegation produces OutboxEntryId (IDS-02)
        // C. Timer and Outbox calls consume one shared sequence (IDS-15)
        let fake = Box::new(FakeDeterministicSequence::new(100));
        let state = make_empty_state();
        let adapter = WindowsIdSourceAdapter::from_sequence_and_state(fake, &state);

        let timer_1 = adapter.next_timer_id();
        assert_eq!(timer_1, TimerId(100u128.to_be_bytes()));

        let outbox_1 = adapter.next_outbox_id();
        assert_eq!(outbox_1, OutboxEntryId(101u128.to_be_bytes()));

        let timer_2 = adapter.next_timer_id();
        assert_eq!(timer_2, TimerId(102u128.to_be_bytes()));

        let outbox_2 = adapter.next_outbox_id();
        assert_eq!(outbox_2, OutboxEntryId(103u128.to_be_bytes()));
    }

    #[test]
    fn test_d_existing_timer_id_reservation_skipped() {
        // D. existing TimerId reservation is skipped (IDS-16)
        let fake = Box::new(FakeDeterministicSequence::new(10));
        let mut state = make_empty_state();
        state.active_actions.push(make_scheduled_action(10));

        let adapter = WindowsIdSourceAdapter::from_sequence_and_state(fake, &state);
        assert!(adapter.reserved_ids.contains(&10u128.to_be_bytes()));

        // 10 is reserved, so next_timer_id must return 11
        let timer_id = adapter.next_timer_id();
        assert_eq!(timer_id, TimerId(11u128.to_be_bytes()));
    }

    #[test]
    fn test_e_existing_outbox_entry_id_reservation_skipped() {
        // E. existing OutboxEntryId reservation is skipped (IDS-17)
        let fake = Box::new(FakeDeterministicSequence::new(10));
        let mut state = make_empty_state();
        state.telegram_outbox.push(make_outbox_entry(10));

        let adapter = WindowsIdSourceAdapter::from_sequence_and_state(fake, &state);
        assert!(adapter.reserved_ids.contains(&10u128.to_be_bytes()));

        // 10 is reserved, so next_outbox_id must return 11
        let outbox_id = adapter.next_outbox_id();
        assert_eq!(outbox_id, OutboxEntryId(11u128.to_be_bytes()));
    }

    #[test]
    fn test_f_multiple_contiguous_reserved_values_skipped() {
        // F. multiple contiguous reserved values are skipped (IDS-18)
        let fake = Box::new(FakeDeterministicSequence::new(10));
        let mut state = make_empty_state();
        state.active_actions.push(make_scheduled_action(10));
        state.active_actions.push(make_scheduled_action(11));
        state.telegram_outbox.push(make_outbox_entry(12));
        state.telegram_outbox.push(make_outbox_entry(13));

        let adapter = WindowsIdSourceAdapter::from_sequence_and_state(fake, &state);
        assert_eq!(adapter.reserved_ids.len(), 4);

        // 10, 11, 12, 13 are all reserved, so next_timer_id must skip all four and return 14
        let timer_id = adapter.next_timer_id();
        assert_eq!(timer_id, TimerId(14u128.to_be_bytes()));

        // Next call produces 15
        let outbox_id = adapter.next_outbox_id();
        assert_eq!(outbox_id, OutboxEntryId(15u128.to_be_bytes()));
    }

    #[test]
    fn test_g_collision_prevented_before_candidate_boundary() {
        // G. returned collision is prevented before candidate insertion boundary (IDS-19)
        let fake = Box::new(FakeDeterministicSequence::new(50));
        let mut state = make_empty_state();
        state.active_actions.push(make_scheduled_action(50));
        state.telegram_outbox.push(make_outbox_entry(51));

        let adapter = WindowsIdSourceAdapter::from_sequence_and_state(fake, &state);

        for _ in 0..10 {
            let t = adapter.next_timer_id();
            assert!(!adapter.reserved_ids.contains(&t.0));
            let o = adapter.next_outbox_id();
            assert!(!adapter.reserved_ids.contains(&o.0));
        }
    }

    #[test]
    fn test_h_reserved_set_remains_unchanged_after_generation() {
        // H. reserved set remains unchanged after generation
        let fake = Box::new(FakeDeterministicSequence::new(1));
        let mut state = make_empty_state();
        state.active_actions.push(make_scheduled_action(999));
        state.telegram_outbox.push(make_outbox_entry(888));

        let adapter = WindowsIdSourceAdapter::from_sequence_and_state(fake, &state);
        let initial_reserved = adapter.reserved_ids.clone();

        for _ in 0..20 {
            let _ = adapter.next_timer_id();
            let _ = adapter.next_outbox_id();
        }

        assert_eq!(adapter.reserved_ids, initial_reserved);
    }

    #[test]
    fn test_i_error_mapping_preserves_unsupported_platform() {
        // I. error mapping preserves UnsupportedPlatform identity
        let err: PlatformError = WindowsIdGeneratorError::UnsupportedPlatform.into();
        assert_eq!(err.reason, "WindowsIdGeneratorError: UnsupportedPlatform");
    }

    #[test]
    fn test_j_error_mapping_preserves_exact_signed_ntstatus() {
        // J. error mapping preserves exact signed NTSTATUS
        let failure_ntstatus: i32 = -1073741811;
        let err: PlatformError = WindowsIdGeneratorError::RandomSeedGenerationFailure {
            ntstatus: failure_ntstatus,
        }
        .into();
        assert_eq!(
            err.reason,
            format!(
                "WindowsIdGeneratorError: RandomSeedGenerationFailure (ntstatus: {failure_ntstatus})"
            )
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_real_windows_production_adapter_construction() {
        let state = make_empty_state();
        let adapter = WindowsIdSourceAdapter::from_production(&state)
            .expect("WindowsIdSourceAdapter::from_production must succeed on Windows host");

        let timer_id = adapter.next_timer_id();
        let outbox_id = adapter.next_outbox_id();

        assert_eq!(
            u128::from_be_bytes(outbox_id.0),
            u128::from_be_bytes(timer_id.0).wrapping_add(1)
        );
    }
}
