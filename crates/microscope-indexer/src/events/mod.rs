use std::io::Read;

use carbon_core::{
    borsh::BorshDeserialize, deserialize::CarbonDeserialize, instruction::InstructionMetadata,
};
use carbon_program_decoder::instructions::CpiEvent;
use heck::ToSnakeCase;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Log,
    Cpi,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Cpi => "cpi",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct DecodedEvent {
    pub name: String,
    pub data: Value,
    pub source: EventSource,
}

#[derive(Debug, Default, PartialEq)]
pub struct DecodedLogs {
    pub events: Vec<DecodedEvent>,
    pub rejected: usize,
}

pub fn decode_logs(metadata: &InstructionMetadata) -> DecodedLogs {
    let mut decoded = DecodedLogs::default();
    for raw in runtime_logs_only(metadata).decode_log_events::<RawEventData>() {
        const EVENT_CPI_PREFIX: &[u8] = &[228, 69, 165, 46, 81, 203, 154, 29];
        let mut instruction_data = Vec::with_capacity(EVENT_CPI_PREFIX.len() + raw.0.len());
        instruction_data.extend_from_slice(EVENT_CPI_PREFIX);
        instruction_data.extend_from_slice(&raw.0);

        match CpiEvent::decode(&instruction_data)
            .and_then(|event| to_log_data(&event, EventSource::Log))
        {
            Some(event) => decoded.events.push(event),
            None => decoded.rejected += 1,
        }
    }
    decoded
}

/// Program-emitted text is shaped like the runtime's own invoke/success lines
/// closely enough to shift instruction attribution, so a hostile program can
/// forge or displace another program's events. After dropping program text,
/// only the runtime emits the structural line kinds (`invoke`,
/// `success`/`failed`, `consumed`), so path equality means the event came from
/// this instruction's own frame; `Program data:` lines survive because they
/// carry the payload.
fn runtime_logs_only(metadata: &InstructionMetadata) -> InstructionMetadata {
    let Some(logs) = &metadata.transaction_metadata.meta.log_messages else {
        return metadata.clone();
    };
    if !logs.iter().any(|line| is_program_emitted(line)) {
        return metadata.clone();
    }

    let mut transaction = (*metadata.transaction_metadata).clone();
    transaction.meta.log_messages = Some(
        logs.iter()
            .filter(|line| !is_program_emitted(line))
            .cloned()
            .collect(),
    );
    InstructionMetadata {
        transaction_metadata: std::sync::Arc::new(transaction),
        ..metadata.clone()
    }
}

fn is_program_emitted(line: &str) -> bool {
    line.starts_with("Program log: ") || line.starts_with("Program return: ")
}

pub fn cpi_to_log_data(event: &CpiEvent) -> Option<DecodedEvent> {
    to_log_data(event, EventSource::Cpi)
}

/// Event self-CPIs are separate instructions, so processing them on their own
/// would attribute the event to the synthetic `cpi_event` name. Collect them
/// from the emitting instruction's children instead, where the operation name
/// is the one the operator configured alerts against.
pub fn decode_child_events(
    children: &carbon_core::instruction::NestedInstructions,
    program_id: &solana_pubkey::Pubkey,
) -> DecodedLogs {
    let mut decoded = DecodedLogs::default();
    for child in children.iter() {
        if child.instruction.program_id != *program_id {
            continue;
        }
        let Some(event) = CpiEvent::decode(&child.instruction.data) else {
            continue;
        };
        match cpi_to_log_data(&event) {
            Some(event) => decoded.events.push(event),
            None => decoded.rejected += 1,
        }
    }
    decoded
}

fn to_log_data(event: &CpiEvent, source: EventSource) -> Option<DecodedEvent> {
    let serialized = serde_json::to_value(event).ok()?;
    let (name, data) = serialized.as_object()?.iter().next()?;
    Some(DecodedEvent {
        name: name.to_snake_case(),
        data: data.clone(),
        source,
    })
}

struct RawEventData(Vec<u8>);

impl BorshDeserialize for RawEventData {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        Ok(Self(data))
    }
}

impl CarbonDeserialize for RawEventData {
    const DISCRIMINATOR: &'static [u8] = &[];

    fn deserialize(data: &[u8]) -> Option<Self> {
        Some(Self(data.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use carbon_core::{instruction::InstructionMetadata, transaction::TransactionMetadata};
    use carbon_program_decoder::PROGRAM_ID;
    use solana_pubkey::Pubkey;

    use carbon_program_decoder::instructions::CpiEvent;

    use super::{cpi_to_log_data, decode_logs, EventSource};

    const EVENT_CPI_PREFIX: [u8; 8] = [228, 69, 165, 46, 81, 203, 154, 29];

    fn subscription_created_payload() -> Vec<u8> {
        subscription_created_payload_at(1_700_000_000)
    }

    fn subscription_created_payload_at(created_ts: i64) -> Vec<u8> {
        [
            &[0][..],
            &[1; 32],
            &[2; 32],
            &[3; 32],
            &created_ts.to_le_bytes(),
            &[4; 32],
        ]
        .concat()
    }

    fn metadata(logs: Vec<String>) -> InstructionMetadata {
        metadata_at_path(logs, vec![0])
    }

    fn metadata_at_path(logs: Vec<String>, absolute_path: Vec<u8>) -> InstructionMetadata {
        let mut transaction = TransactionMetadata::default();
        transaction.meta.log_messages = Some(logs);
        InstructionMetadata {
            transaction_metadata: Arc::new(transaction),
            stack_height: 1,
            index: 1,
            absolute_path,
        }
    }

    #[test]
    fn decodes_generated_event_from_program_logs() {
        let payload = subscription_created_payload();
        let metadata = metadata(vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program data: {}", STANDARD.encode(payload)),
            format!("Program {PROGRAM_ID} success"),
        ]);

        let events = decode_logs(&metadata).events;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "subscription_created_event");
        assert_eq!(events[0].source, EventSource::Log);
        assert_eq!(events[0].data["created_ts"], 1_700_000_000_i64);
        assert_eq!(
            events[0].data["plan"],
            Pubkey::new_from_array([1; 32]).to_string()
        );
    }

    #[test]
    fn ignores_forged_success_lines_that_would_shift_attribution() {
        let forged = subscription_created_payload_at(9);
        let genuine = subscription_created_payload_at(1_700_000_000);
        let metadata = metadata(vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            "Program 11111111111111111111111111111111 invoke [2]".to_string(),
            "Program log: Program spoof success".to_string(),
            format!("Program log: forged {}", STANDARD.encode(&forged)),
            "Program 11111111111111111111111111111111 success".to_string(),
            format!("Program data: {}", STANDARD.encode(&genuine)),
            format!("Program {PROGRAM_ID} success"),
        ]);

        let events = decode_logs(&metadata).events;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["created_ts"], 1_700_000_000_i64);
    }

    #[test]
    fn ignores_event_payloads_emitted_on_program_log_lines() {
        let metadata = metadata(vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!(
                "Program log: {}",
                STANDARD.encode(subscription_created_payload())
            ),
            format!("Program {PROGRAM_ID} success"),
        ]);

        assert!(decode_logs(&metadata).events.is_empty());
    }

    #[test]
    fn ignores_event_payloads_emitted_as_return_data() {
        let metadata = metadata(vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!(
                "Program return: {PROGRAM_ID} {}",
                STANDARD.encode(subscription_created_payload())
            ),
            format!("Program {PROGRAM_ID} success"),
        ]);

        assert!(decode_logs(&metadata).events.is_empty());
    }

    #[test]
    fn ignores_forged_invoke_lines_from_a_sibling_instruction() {
        let forged = subscription_created_payload_at(9);
        let genuine = subscription_created_payload_at(1_700_000_000);
        let metadata = metadata_at_path(
            vec![
                "Program AttackerLoggerProgram11111111111111111111 invoke [1]".to_string(),
                "Program log: invoke [1]".to_string(),
                format!("Program log: forged {}", STANDARD.encode(&forged)),
                "Program AttackerLoggerProgram11111111111111111111 success".to_string(),
                format!("Program {PROGRAM_ID} invoke [1]"),
                format!("Program data: {}", STANDARD.encode(&genuine)),
                format!("Program {PROGRAM_ID} success"),
            ],
            vec![1],
        );

        let events = decode_logs(&metadata).events;

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["created_ts"], 1_700_000_000_i64);
    }

    #[test]
    fn counts_undecodable_event_payloads_as_rejected() {
        let unknown_discriminator = vec![9_u8; 16];
        let mut truncated = subscription_created_payload();
        truncated.truncate(12);
        let metadata = metadata(vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program data: {}", STANDARD.encode(&unknown_discriminator)),
            format!("Program data: {}", STANDARD.encode(&truncated)),
            format!("Program {PROGRAM_ID} success"),
        ]);

        let decoded = decode_logs(&metadata);

        assert!(decoded.events.is_empty());
        assert_eq!(decoded.rejected, 2);
    }

    fn child_instruction(
        program_id: Pubkey,
        data: Vec<u8>,
    ) -> carbon_core::instruction::NestedInstruction {
        carbon_core::instruction::NestedInstruction {
            metadata: metadata(Vec::new()),
            instruction: solana_instruction::Instruction {
                program_id,
                accounts: Vec::new(),
                data,
            },
            inner_instructions: carbon_core::instruction::NestedInstructions::default(),
        }
    }

    #[test]
    fn decodes_events_from_event_self_cpi_children() {
        let event_data = [
            EVENT_CPI_PREFIX.as_slice(),
            subscription_created_payload().as_slice(),
        ]
        .concat();
        let children = carbon_core::instruction::NestedInstructions(vec![
            child_instruction(Pubkey::new_unique(), event_data.clone()),
            child_instruction(PROGRAM_ID, event_data),
        ]);

        let decoded = super::decode_child_events(&children, &PROGRAM_ID);

        assert_eq!(decoded.events.len(), 1);
        assert_eq!(decoded.events[0].name, "subscription_created_event");
        assert_eq!(decoded.events[0].source, EventSource::Cpi);
    }

    #[test]
    fn ignores_children_that_are_not_events() {
        let children = carbon_core::instruction::NestedInstructions(vec![child_instruction(
            PROGRAM_ID,
            vec![7; 32],
        )]);

        let decoded = super::decode_child_events(&children, &PROGRAM_ID);

        assert!(decoded.events.is_empty());
        assert_eq!(decoded.rejected, 0);
    }

    #[test]
    fn decodes_generated_event_from_cpi_instruction_data() {
        let instruction_data = [
            EVENT_CPI_PREFIX.as_slice(),
            subscription_created_payload().as_slice(),
        ]
        .concat();

        let event = CpiEvent::decode(&instruction_data).expect("generated event decodes");
        let event = cpi_to_log_data(&event).expect("generated event serializes");

        assert_eq!(event.name, "subscription_created_event");
        assert_eq!(event.source, EventSource::Cpi);
        assert_eq!(event.data["created_ts"], 1_700_000_000_i64);
        assert_eq!(
            event.data["subscriber"],
            Pubkey::new_from_array([2; 32]).to_string()
        );
    }
}
