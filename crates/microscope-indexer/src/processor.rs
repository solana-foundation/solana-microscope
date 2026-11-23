use std::{
    collections::{HashSet, VecDeque},
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use carbon_core::{
    error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor,
};
use solana_signature::Signature;

use crate::{events, instructions, programs::Program, telemetry};

const RECENT_SIGNATURE_CAPACITY: usize = 4_096;

/// Shared by every program's processor, so a transaction that touches several
/// monitored programs is counted once.
#[derive(Default)]
pub struct RecentTransactions {
    signatures: HashSet<Signature>,
    order: VecDeque<Signature>,
}

impl RecentTransactions {
    fn remember(&mut self, signature: Signature) -> bool {
        if !self.signatures.insert(signature) {
            return false;
        }

        self.order.push_back(signature);
        if self.order.len() > RECENT_SIGNATURE_CAPACITY {
            if let Some(expired) = self.order.pop_front() {
                self.signatures.remove(&expired);
            }
        }

        true
    }
}

pub struct EventProcessor<P> {
    recent_transactions: Arc<Mutex<RecentTransactions>>,
    program: PhantomData<P>,
}

impl<P> EventProcessor<P> {
    pub fn new(recent_transactions: Arc<Mutex<RecentTransactions>>) -> Self {
        Self {
            recent_transactions,
            program: PhantomData,
        }
    }
}

impl<P: Program> Processor<InstructionProcessorInputType<'_, P::Instruction>>
    for EventProcessor<P>
{
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, P::Instruction>,
    ) -> CarbonResult<()> {
        let metadata = input.metadata;
        let decoded = input.decoded_instruction;
        let raw = input.raw_instruction;
        let instruction = instructions::to_log_data(decoded)?;
        let instruction_name = instruction.name;
        let transaction = &metadata.transaction_metadata;
        let failed = transaction.meta.status.is_err();

        log::info!(
            target: "microscope::instructions",
            "{}",
            serde_json::json!({
                "kind": "program_instruction",
                "name": instruction_name,
                "data": instruction.data,
                "program_id": raw.program_id.to_string(),
                "instruction_index": metadata.index,
                "instruction_path": instructions::occurrence_path(metadata),
                "stack_height": metadata.stack_height,
                "signature": transaction.signature.to_string(),
                "slot": transaction.slot,
                "block_time": transaction.block_time,
                "failed": failed,
            })
        );

        telemetry::record_instruction(raw.program_id.to_string(), instruction_name.clone());
        if P::DECLARES_EVENTS {
            let (from_logs, from_children) = if P::is_event_wrapper(decoded) {
                (
                    events::DecodedLogs::default(),
                    events::DecodedLogs::default(),
                )
            } else {
                (
                    events::decode_logs::<P>(metadata),
                    events::decode_child_events::<P>(input.nested_instructions, &raw.program_id),
                )
            };
            for (source, rejected) in [
                (events::EventSource::Log, from_logs.rejected),
                (events::EventSource::Cpi, from_children.rejected),
            ] {
                if rejected == 0 {
                    continue;
                }
                log::warn!(
                    target: "microscope::events",
                    "{}",
                    serde_json::json!({
                        "kind": "event_decode_failure",
                        "rejected": rejected,
                        "source": source.as_str(),
                        "program_id": raw.program_id.to_string(),
                        "instruction": instruction_name,
                        "signature": transaction.signature.to_string(),
                        "slot": transaction.slot,
                        "block_time": transaction.block_time,
                    })
                );
                telemetry::record_event_decode_failures(source.as_str(), rejected as u64);
            }
            for event in from_logs.events.into_iter().chain(from_children.events) {
                let source = event.source.as_str();
                log::info!(
                    target: "microscope::events",
                    "{}",
                    serde_json::json!({
                        "kind": "program_event",
                        "name": event.name,
                        "data": event.data,
                        "source": source,
                        "program_id": raw.program_id.to_string(),
                        "instruction": instruction_name,
                        "instruction_index": metadata.index,
                        "instruction_path": instructions::occurrence_path(metadata),
                        "stack_height": metadata.stack_height,
                        "signature": transaction.signature.to_string(),
                        "slot": transaction.slot,
                        "block_time": transaction.block_time,
                        "failed": failed,
                    })
                );
                telemetry::record_program_event(
                    raw.program_id.to_string(),
                    event.name,
                    source,
                    failed,
                );
            }
        }

        if self
            .recent_transactions
            .lock()
            .expect("recent transactions lock is not poisoned")
            .remember(transaction.signature)
        {
            telemetry::record_transaction(failed);
        }
        telemetry::record_event();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{RecentTransactions, RECENT_SIGNATURE_CAPACITY};
    use crate::test_support::signature;

    #[test]
    fn suppresses_recent_duplicate_transactions() {
        let mut recent = RecentTransactions::default();
        let transaction = signature(1);

        assert!(recent.remember(transaction));
        assert!(!recent.remember(transaction));
        assert_eq!(recent.signatures.len(), 1);
        assert_eq!(recent.order.len(), 1);
    }

    #[test]
    fn evicts_the_oldest_transaction_at_capacity() {
        let mut recent = RecentTransactions::default();
        let oldest = signature(0);
        assert!(recent.remember(oldest));

        for value in 1..=RECENT_SIGNATURE_CAPACITY as u64 {
            assert!(recent.remember(signature(value)));
        }

        assert_eq!(recent.signatures.len(), RECENT_SIGNATURE_CAPACITY);
        assert_eq!(recent.order.len(), RECENT_SIGNATURE_CAPACITY);
        assert!(!recent.signatures.contains(&oldest));
        assert!(recent.remember(oldest));
    }
}
