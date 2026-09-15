use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    time::{Duration, Instant},
};

use solana_signature::Signature;

use super::signatures::{AddressBatch, AddressCursor};

#[derive(Debug, Default)]
pub(super) struct PollingState {
    pub(super) cursors: BTreeMap<String, AddressCursor>,
    pub(super) recent_signatures: BTreeMap<Signature, u64>,
    poison_failures: HashMap<Signature, PoisonFailure>,
    last_history_boundary_check: Option<Instant>,
}

impl PollingState {
    pub(super) fn has_recent_signature(&self, signature: &Signature) -> bool {
        self.recent_signatures.contains_key(signature)
    }

    pub(super) fn record_recent_signature(&mut self, signature: Signature, slot: u64) {
        self.recent_signatures.insert(signature, slot);
    }

    pub(super) fn recent_signature_count(&self) -> usize {
        self.recent_signatures.len()
    }

    pub(super) fn retain_signatures_after(&mut self, minimum_slot: u64) {
        self.recent_signatures
            .retain(|_, slot| *slot > minimum_slot);
    }

    pub(super) fn history_boundary_check_due(&self, now: Instant, interval: Duration) -> bool {
        self.last_history_boundary_check
            .is_none_or(|last_check| now.saturating_duration_since(last_check) >= interval)
    }

    pub(super) fn record_history_boundary_check(&mut self, checked_at: Instant) {
        self.last_history_boundary_check = Some(checked_at);
    }

    pub(super) fn record_poison_failure(
        &mut self,
        signature: Signature,
        maximum_attempts: u32,
    ) -> FailureDisposition {
        let failure = self
            .poison_failures
            .entry(signature)
            .or_insert(PoisonFailure {
                attempts: 0,
                quarantined: false,
            });
        let newly_quarantined = if failure.quarantined {
            false
        } else {
            failure.attempts = failure.attempts.saturating_add(1);
            failure.quarantined = failure.attempts >= maximum_attempts;
            failure.quarantined
        };
        FailureDisposition {
            attempts: failure.attempts,
            quarantined: failure.quarantined,
            newly_quarantined,
        }
    }

    pub(super) fn clear_poison_failure(&mut self, signature: &Signature) {
        self.poison_failures.remove(signature);
    }

    pub(super) fn quarantined_transaction_count(&self) -> usize {
        self.quarantined_signatures().count()
    }

    pub(super) fn quarantined_signatures(&self) -> impl Iterator<Item = &Signature> {
        self.poison_failures
            .iter()
            .filter(|(_, failure)| failure.quarantined)
            .map(|(signature, _)| signature)
    }

    pub(super) fn restore_quarantined(&mut self, signatures: impl IntoIterator<Item = Signature>) {
        for signature in signatures {
            self.poison_failures.insert(
                signature,
                PoisonFailure {
                    attempts: u32::MAX,
                    quarantined: true,
                },
            );
        }
    }
}

#[derive(Debug)]
struct PoisonFailure {
    attempts: u32,
    quarantined: bool,
}

#[derive(Debug)]
pub(super) struct FailureDisposition {
    pub(super) attempts: u32,
    pub(super) quarantined: bool,
    pub(super) newly_quarantined: bool,
}

#[derive(Debug)]
pub(super) struct SignatureContext {
    pub(super) slot: u64,
    pub(super) addresses: BTreeSet<String>,
}

pub(super) fn signature_is_ready(
    signature: Signature,
    contexts: &HashMap<Signature, SignatureContext>,
    blocked_addresses: &BTreeSet<String>,
) -> bool {
    contexts
        .get(&signature)
        .is_some_and(|context| context.addresses.is_disjoint(blocked_addresses))
}

pub(super) fn advance_ready_cursors(
    state: &mut PollingState,
    batches: &[AddressBatch],
    retrying_signatures: &HashSet<Signature>,
) -> bool {
    let mut advanced_with_activity = false;
    for batch in batches {
        if batch
            .signatures
            .iter()
            .any(|discovered| retrying_signatures.contains(&discovered.signature))
        {
            continue;
        }
        advanced_with_activity |= !batch.signatures.is_empty();
        state
            .cursors
            .insert(batch.address.to_string(), batch.next_cursor.clone());
    }
    advanced_with_activity
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap, HashSet},
        time::{Duration, Instant},
    };

    use solana_pubkey::Pubkey;

    use super::{advance_ready_cursors, signature_is_ready, PollingState, SignatureContext};
    use crate::rpc_polling::signatures::{AddressBatch, AddressCursor, DiscoveredSignature};

    fn signature(value: u64) -> solana_signature::Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        solana_signature::Signature::from(bytes)
    }

    #[test]
    fn limits_history_boundary_checks_by_time() {
        let mut state = PollingState::default();
        let started = Instant::now();
        let interval = Duration::from_secs(60);

        assert!(state.history_boundary_check_due(started, interval));
        state.record_history_boundary_check(started);
        assert!(!state.history_boundary_check_due(started + Duration::from_secs(59), interval));
        assert!(state.history_boundary_check_due(started + interval, interval));
    }

    #[test]
    fn retains_only_signatures_needed_by_the_replay_window() {
        let mut state = PollingState::default();
        let old = signature(1);
        let recent = signature(2);
        state.record_recent_signature(old, 90);
        state.record_recent_signature(recent, 101);

        assert!(state.has_recent_signature(&old));
        state.retain_signatures_after(100);

        assert!(!state.has_recent_signature(&old));
        assert!(state.has_recent_signature(&recent));
    }

    #[test]
    fn quarantines_poison_failures_after_bounded_attempts() {
        let transaction = signature(1);
        let mut state = PollingState::default();

        for expected_attempts in 1..5 {
            let disposition = state.record_poison_failure(transaction, 5);
            assert_eq!(disposition.attempts, expected_attempts);
            assert!(!disposition.quarantined);
        }
        let disposition = state.record_poison_failure(transaction, 5);
        assert_eq!(disposition.attempts, 5);
        assert!(disposition.quarantined);
        assert!(disposition.newly_quarantined);
        assert_eq!(state.quarantined_transaction_count(), 1);
    }

    #[test]
    fn advances_only_addresses_without_retrying_transactions() {
        let blocked_address = Pubkey::new_unique();
        let ready_address = Pubkey::new_unique();
        let blocked_signature = signature(1);
        let ready_signature = signature(2);
        let mut state = PollingState::default();
        state.cursors.insert(
            blocked_address.to_string(),
            AddressCursor { scanned_slot: 100 },
        );
        state.cursors.insert(
            ready_address.to_string(),
            AddressCursor { scanned_slot: 100 },
        );
        let batches = [
            AddressBatch {
                address: blocked_address,
                signatures: vec![DiscoveredSignature {
                    signature: blocked_signature,
                    slot: 101,
                }],
                next_cursor: AddressCursor { scanned_slot: 110 },
                reached_replay_floor: true,
            },
            AddressBatch {
                address: ready_address,
                signatures: vec![DiscoveredSignature {
                    signature: ready_signature,
                    slot: 102,
                }],
                next_cursor: AddressCursor { scanned_slot: 110 },
                reached_replay_floor: true,
            },
        ];

        let contexts = HashMap::from([
            (
                blocked_signature,
                SignatureContext {
                    slot: 101,
                    addresses: BTreeSet::from([blocked_address.to_string()]),
                },
            ),
            (
                ready_signature,
                SignatureContext {
                    slot: 102,
                    addresses: BTreeSet::from([ready_address.to_string()]),
                },
            ),
        ]);
        let blocked_addresses = BTreeSet::from([blocked_address.to_string()]);

        assert!(advance_ready_cursors(
            &mut state,
            &batches,
            &HashSet::from([blocked_signature])
        ));

        assert!(!signature_is_ready(
            blocked_signature,
            &contexts,
            &blocked_addresses
        ));
        assert!(signature_is_ready(
            ready_signature,
            &contexts,
            &blocked_addresses
        ));
        assert_eq!(
            state.cursors[&blocked_address.to_string()].scanned_slot,
            100
        );
        assert_eq!(state.cursors[&ready_address.to_string()].scanned_slot, 110);
    }

    #[test]
    fn a_restored_quarantine_keeps_reporting_its_transaction() {
        let quarantined = signature(4);
        let mut state = PollingState::default();

        state.restore_quarantined([quarantined]);

        assert_eq!(state.quarantined_transaction_count(), 1);
        assert_eq!(
            state.quarantined_signatures().copied().collect::<Vec<_>>(),
            vec![quarantined]
        );
    }
}
