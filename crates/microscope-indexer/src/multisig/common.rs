use std::collections::HashMap;

use carbon_core::{error::CarbonResult, instruction::InstructionProcessorInputType};
use serde::Serialize;
use solana_pubkey::Pubkey;

use super::VerifiedMultisig;
use crate::{config::MultisigVersion, instructions, telemetry};

#[derive(Clone, Copy)]
pub struct AddressMatch {
    pub configured_address: Pubkey,
    pub state_address: Pubkey,
    pub squads_version: &'static str,
}

pub struct AddressResolver {
    vaults_by_state: HashMap<Pubkey, Pubkey>,
    squads_version: &'static str,
}

impl AddressResolver {
    pub fn new(multisigs: &[VerifiedMultisig], version: MultisigVersion) -> Self {
        Self {
            vaults_by_state: multisigs
                .iter()
                .filter(|multisig| multisig.version == version)
                .map(|multisig| (multisig.state_address, multisig.vault_address))
                .collect(),
            squads_version: version.as_str(),
        }
    }

    pub fn match_state(&self, state_address: Pubkey) -> Option<AddressMatch> {
        let Some(&vault_address) = self.vaults_by_state.get(&state_address) else {
            telemetry::record_multisig_unmatched_state(self.squads_version);
            return None;
        };
        Some(AddressMatch {
            configured_address: vault_address,
            state_address,
            squads_version: self.squads_version,
        })
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
                "configured_address_kind": "default_vault",
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
    use crate::{config::MultisigVersion, multisig::VerifiedMultisig};

    fn multisig(version: MultisigVersion) -> VerifiedMultisig {
        VerifiedMultisig {
            vault_address: Pubkey::new_unique(),
            state_address: Pubkey::new_unique(),
            version,
        }
    }

    #[test]
    fn matches_only_the_resolved_state_account() {
        let state = Pubkey::new_unique();
        let vault = Pubkey::new_unique();
        let resolver = AddressResolver::new(
            &[VerifiedMultisig {
                vault_address: vault,
                state_address: state,
                version: MultisigVersion::V4,
            }],
            MultisigVersion::V4,
        );

        let matched = resolver.match_state(state).expect("state account matches");

        assert_eq!(matched.configured_address, vault);
        assert_eq!(matched.state_address, state);
        assert_eq!(matched.squads_version, "v4");
        assert!(resolver.match_state(Pubkey::new_unique()).is_none());
        assert!(resolver.match_state(vault).is_none());
    }

    #[test]
    fn maps_each_state_account_to_its_own_vault() {
        let first = multisig(MultisigVersion::V4);
        let second = multisig(MultisigVersion::V4);
        let resolver = AddressResolver::new(&[first, second], MultisigVersion::V4);

        for configured in [first, second] {
            let matched = resolver
                .match_state(configured.state_address)
                .expect("every configured state account matches");
            assert_eq!(matched.configured_address, configured.vault_address);
            assert_eq!(matched.state_address, configured.state_address);
        }
        assert!(resolver.match_state(Pubkey::new_unique()).is_none());
    }

    #[test]
    fn ignores_state_accounts_configured_for_another_version() {
        let v3 = multisig(MultisigVersion::V3);
        let v4 = multisig(MultisigVersion::V4);
        let resolver = AddressResolver::new(&[v3, v4], MultisigVersion::V4);
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            assert!(resolver.match_state(v4.state_address).is_some());
            assert!(resolver.match_state(v3.state_address).is_none());
        });

        let unmatched = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(key, _, _, _)| key.key().name() == "microscope_multisig_unmatched_state_total")
            .map(|(_, _, _, value)| value);
        assert_eq!(unmatched, Some(DebugValue::Counter(1)));
    }

    #[test]
    fn counts_activity_on_a_state_account_the_deployment_does_not_monitor() {
        let resolver = AddressResolver::new(&[multisig(MultisigVersion::V5)], MultisigVersion::V5);
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
