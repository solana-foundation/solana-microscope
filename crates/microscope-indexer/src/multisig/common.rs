use carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType};
use serde::Serialize;
use solana_pubkey::Pubkey;

use crate::{instructions, telemetry};

#[derive(Clone, Copy)]
pub struct AddressMatch {
    pub configured_address: Pubkey,
    pub state_address: Pubkey,
    pub configured_address_kind: &'static str,
    pub squads_version: &'static str,
}

pub struct AddressResolver {
    resolved: AddressMatch,
}

impl AddressResolver {
    pub fn new(vault_address: Pubkey, state_address: Pubkey, squads_version: &'static str) -> Self {
        Self {
            resolved: AddressMatch {
                configured_address: vault_address,
                state_address,
                configured_address_kind: "default_vault",
                squads_version,
            },
        }
    }

    pub fn match_state(&self, state_address: Pubkey) -> Option<AddressMatch> {
        if state_address != self.resolved.state_address {
            telemetry::record_multisig_unmatched_state(self.resolved.squads_version);
            return None;
        }
        Some(self.resolved)
    }
}

pub fn process<T: Serialize>(
    input: &InstructionProcessorInputType<'_, T>,
    address_match: Option<AddressMatch>,
    idl_version: &'static str,
    actions: &[&'static str],
) -> CarbonResult<()> {
    let Some(address_match) = address_match else {
        return Ok(());
    };
    let squads_version = address_match.squads_version;

    let instruction = instructions::to_log_data(input.decoded_instruction)?;
    let transaction = &input.metadata.transaction_metadata;
    let failed = transaction.meta.status.is_err();

    for action in actions {
        log::info!(
            target: "microscope::multisig",
            "{}",
            serde_json::json!({
                "kind": "multisig_activity",
                "provider": "squads",
                "squads_version": squads_version,
                "idl_version": idl_version,
                "action": action,
                "instruction": &instruction.name,
                "data": &instruction.data,
                "vault_address": address_match.configured_address.to_string(),
                "multisig_address": address_match.state_address.to_string(),
                "configured_address_kind": address_match.configured_address_kind,
                "program_id": input.raw_instruction.program_id.to_string(),
                "instruction_index": input.metadata.index,
                "instruction_path": instructions::occurrence_path(input.metadata),
                "stack_height": input.metadata.stack_height,
                "signature": transaction.signature.to_string(),
                "slot": transaction.slot,
                "block_time": transaction.block_time,
                "failed": failed,
            })
        );

        telemetry::record_multisig_activity(squads_version, action, failed);
    }
    telemetry::record_event();
    Ok(())
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use solana_pubkey::Pubkey;

    use super::AddressResolver;

    #[test]
    fn matches_only_the_resolved_state_account() {
        let state = Pubkey::new_unique();
        let vault = Pubkey::new_unique();
        let resolver = AddressResolver::new(vault, state, "v4");

        let matched = resolver.match_state(state).expect("state account matches");

        assert_eq!(matched.configured_address, vault);
        assert_eq!(matched.state_address, state);
        assert_eq!(matched.configured_address_kind, "default_vault");
        assert_eq!(matched.squads_version, "v4");
        assert!(resolver.match_state(Pubkey::new_unique()).is_none());
        assert!(resolver.match_state(vault).is_none());
    }

    #[test]
    fn counts_activity_on_a_state_account_the_deployment_does_not_monitor() {
        let resolver = AddressResolver::new(Pubkey::new_unique(), Pubkey::new_unique(), "v5");
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            assert!(resolver.match_state(Pubkey::new_unique()).is_none());
        });

        let (key, _, _, value) = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(key, _, _, _)| key.key().name() == "microscope_multisig_unmatched_state_total")
            .expect("the unmatched state account is counted");

        assert_eq!(value, DebugValue::Counter(1));
        assert!(key
            .key()
            .labels()
            .any(|label| label.key() == "version" && label.value() == "v5"));
    }
}
