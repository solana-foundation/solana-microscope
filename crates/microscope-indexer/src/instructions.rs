use carbon_core::{
    error::{CarbonResult, Error as CarbonError},
    instruction::InstructionMetadata,
};
use heck::ToSnakeCase;
use serde::Serialize;
use serde_json::Value;

pub struct InstructionLogData {
    pub name: String,
    pub data: Value,
}

/// Disambiguates invocations that share `instruction_index` and
/// `stack_height`, the same sibling position at the same depth under different
/// parents, which would otherwise be byte-identical records that Loki
/// deduplicates away in backfill.
pub fn occurrence_path(metadata: &InstructionMetadata) -> String {
    metadata
        .absolute_path
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

pub fn to_log_data<T: Serialize>(instruction: &T) -> CarbonResult<InstructionLogData> {
    let Value::Object(mut serialized) = serde_json::to_value(instruction).map_err(|error| {
        CarbonError::Custom(format!("failed to serialize instruction: {error}"))
    })?
    else {
        return Err(CarbonError::Custom(
            "serialized instruction is not an object".to_string(),
        ));
    };
    let variant = serialized
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| {
            CarbonError::Custom("serialized instruction is missing its type".to_string())
        })?;
    let data = serialized.remove("data").ok_or_else(|| {
        CarbonError::Custom("serialized instruction is missing its data".to_string())
    })?;

    Ok(InstructionLogData {
        name: variant.to_snake_case(),
        data,
    })
}

#[cfg(test)]
mod tests {
    use carbon_squads_v4_decoder::{
        instructions::{
            ProposalActivate, ProposalActivateInstructionAccounts, SquadsMultisigProgramInstruction,
        },
        PROGRAM_ID,
    };

    use super::to_log_data;

    /// Uses a pinned Squads decoder rather than the deployment's own, which is
    /// generated from whichever IDL the operator configured and declares no
    /// instruction this test could name.
    #[test]
    fn serializes_generated_instruction_to_structured_data() {
        let instruction = SquadsMultisigProgramInstruction::ProposalActivate {
            program_id: PROGRAM_ID,
            data: ProposalActivate {},
            accounts: ProposalActivateInstructionAccounts {
                multisig: PROGRAM_ID,
                member: PROGRAM_ID,
                proposal: PROGRAM_ID,
                remaining: Vec::new(),
            },
        };

        let instruction = to_log_data(&instruction).expect("instruction serializes");

        let program_id = serde_json::to_value(PROGRAM_ID).expect("pubkey serializes");

        assert_eq!(instruction.name, "proposal_activate");
        assert!(instruction.data.get("type").is_none());
        assert_eq!(instruction.data["data"], serde_json::json!({}));
        assert_eq!(instruction.data["program_id"], program_id);
        assert_eq!(instruction.data["accounts"]["multisig"], program_id);
    }
}
