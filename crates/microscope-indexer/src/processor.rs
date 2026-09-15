use std::collections::{HashSet, VecDeque};

use carbon_core::{
    error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor,
};
use carbon_program_decoder::instructions::ProgramInstruction;
use solana_signature::Signature;

#[cfg(program_events)]
use crate::events;
use crate::{instructions, telemetry};

const RECENT_SIGNATURE_CAPACITY: usize = 4_096;

#[derive(Default)]
pub struct EventProcessor {
    recent_signatures: HashSet<Signature>,
    signature_order: VecDeque<Signature>,
}

impl EventProcessor {
    fn remember_transaction(&mut self, signature: Signature) -> bool {
        if !self.recent_signatures.insert(signature) {
            return false;
        }

        self.signature_order.push_back(signature);
        if self.signature_order.len() > RECENT_SIGNATURE_CAPACITY {
            if let Some(expired) = self.signature_order.pop_front() {
                self.recent_signatures.remove(&expired);
            }
        }

        true
    }
}

impl Processor<InstructionProcessorInputType<'_, ProgramInstruction>> for EventProcessor {
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, ProgramInstruction>,
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

        telemetry::record_instruction(instruction_name.clone());
        #[cfg(program_events)]
        {
            let (from_logs, from_children) = match decoded {
                ProgramInstruction::CpiEvent { .. } => (
                    events::DecodedLogs::default(),
                    events::DecodedLogs::default(),
                ),
                _ => (
                    events::decode_logs(metadata),
                    events::decode_child_events(input.nested_instructions, &raw.program_id),
                ),
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
                telemetry::record_program_event(event.name, source, failed);
            }
        }

        if self.remember_transaction(transaction.signature) {
            telemetry::record_transaction(failed);
        }
        telemetry::record_event();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use solana_signature::Signature;

    use super::{EventProcessor, RECENT_SIGNATURE_CAPACITY};

    fn signature(value: u64) -> Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        Signature::from(bytes)
    }

    #[test]
    fn suppresses_recent_duplicate_transactions() {
        let mut processor = EventProcessor::default();
        let transaction = signature(1);

        assert!(processor.remember_transaction(transaction));
        assert!(!processor.remember_transaction(transaction));
        assert_eq!(processor.recent_signatures.len(), 1);
        assert_eq!(processor.signature_order.len(), 1);
    }

    #[test]
    fn evicts_the_oldest_transaction_at_capacity() {
        let mut processor = EventProcessor::default();
        let oldest = signature(0);
        assert!(processor.remember_transaction(oldest));

        for value in 1..=RECENT_SIGNATURE_CAPACITY as u64 {
            assert!(processor.remember_transaction(signature(value)));
        }

        assert_eq!(processor.recent_signatures.len(), RECENT_SIGNATURE_CAPACITY);
        assert_eq!(processor.signature_order.len(), RECENT_SIGNATURE_CAPACITY);
        assert!(!processor.recent_signatures.contains(&oldest));
        assert!(processor.remember_transaction(oldest));
    }
}
