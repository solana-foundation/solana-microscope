use carbon_core::{
    error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor,
};
use carbon_squads_smart_account_decoder::{
    instructions::SquadsSmartAccountProgramInstruction, types::SettingsAction, PROGRAM_ID,
};
use solana_pubkey::Pubkey;

use super::common;

pub struct SmartAccountProcessor {
    address_resolver: common::AddressResolver,
}

impl SmartAccountProcessor {
    pub fn new(vault_address: Pubkey, settings_address: Pubkey) -> Self {
        Self {
            address_resolver: common::AddressResolver::new(vault_address, settings_address, "v5"),
        }
    }
}

impl Processor<InstructionProcessorInputType<'_, SquadsSmartAccountProgramInstruction>>
    for SmartAccountProcessor
{
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, SquadsSmartAccountProgramInstruction>,
    ) -> CarbonResult<()> {
        let address_match = state_address(input.decoded_instruction)
            .and_then(|state_address| self.address_resolver.match_state(state_address));
        let mut actions = actions(input.decoded_instruction).to_vec();
        actions.extend(applied_settings_actions(input.decoded_instruction));
        common::process(input, address_match, "0.1.0", &actions)
    }
}

pub(crate) fn default_vault(settings_state: Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"smart_account",
            settings_state.as_ref(),
            b"smart_account",
            &[0],
        ],
        &PROGRAM_ID,
    )
    .0
}

fn state_address(instruction: &SquadsSmartAccountProgramInstruction) -> Option<Pubkey> {
    use SquadsSmartAccountProgramInstruction::*;

    let state_address = match instruction {
        ActivateProposal { accounts, .. } => accounts.settings,
        AddSignerAsAuthority { accounts, .. } => accounts.settings,
        AddSpendingLimitAsAuthority { accounts, .. } => accounts.settings,
        AddTransactionToBatch { accounts, .. } => accounts.settings,
        ApproveProposal { accounts, .. } => accounts.consensus_account,
        CancelProposal { accounts, .. } => accounts.consensus_account,
        ChangeThresholdAsAuthority { accounts, .. } => accounts.settings,
        CloseBatch { accounts, .. } => accounts.settings,
        CloseBatchTransaction { accounts, .. } => accounts.settings,
        CloseSettingsTransaction { accounts, .. } => accounts.settings,
        CloseTransaction { accounts, .. } => accounts.consensus_account,
        CloseTransactionBuffer { accounts, .. } => accounts.consensus_account,
        CreateBatch { accounts, .. } => accounts.settings,
        CreateProposal { accounts, .. } => accounts.consensus_account,
        CreateSettingsTransaction { accounts, .. } => accounts.settings,
        CreateTransaction { accounts, .. } => accounts.consensus_account,
        CreateTransactionBuffer { accounts, .. } => accounts.consensus_account,
        CreateTransactionFromBuffer { accounts, .. } => accounts.consensus_account,
        ExecuteBatchTransaction { accounts, .. } => accounts.settings,
        ExecuteSettingsTransaction { accounts, .. } => accounts.settings,
        ExecuteSettingsTransactionSync { accounts, .. } => accounts.consensus_account,
        ExecuteTransaction { accounts, .. } => accounts.consensus_account,
        ExecuteTransactionSync { accounts, .. } => accounts.consensus_account,
        ExecuteTransactionSyncV2 { accounts, .. } => accounts.consensus_account,
        ExtendTransactionBuffer { accounts, .. } => accounts.consensus_account,
        RejectProposal { accounts, .. } => accounts.consensus_account,
        RemoveSignerAsAuthority { accounts, .. } => accounts.settings,
        RemoveSpendingLimitAsAuthority { accounts, .. } => accounts.settings,
        SetArchivalAuthorityAsAuthority { accounts, .. } => accounts.settings,
        SetNewSettingsAuthorityAsAuthority { accounts, .. } => accounts.settings,
        SetTimeLockAsAuthority { accounts, .. } => accounts.settings,
        UseSpendingLimit { accounts, .. } => accounts.settings,
        CloseEmptyPolicyTransaction { .. }
        | CreateSmartAccount { .. }
        | InitializeProgramConfig { .. }
        | LogEvent { .. }
        | SetProgramConfigAuthority { .. }
        | SetProgramConfigSmartAccountCreationFee { .. }
        | SetProgramConfigTreasury { .. } => return None,
    };

    Some(state_address)
}

pub(super) const ACTIONS: &[&str] = &[
    "archival_authority_changed",
    "batch_closed",
    "batch_created",
    "batch_transaction_added",
    "batch_transaction_closed",
    "batch_transaction_executed",
    "empty_policy_transaction_closed",
    "event_logged",
    "member_added",
    "member_removed",
    "multisig_created",
    "policy_created",
    "policy_removed",
    "policy_updated",
    "program_configuration_authority_changed",
    "program_configuration_initialized",
    "program_smart_account_creation_fee_changed",
    "program_treasury_changed",
    "proposal_activated",
    "proposal_approved",
    "proposal_cancelled",
    "proposal_created",
    "proposal_rejected",
    "settings_authority_changed",
    "settings_transaction_closed",
    "settings_transaction_created",
    "settings_transaction_executed",
    "spending_limit_added",
    "spending_limit_removed",
    "spending_limit_used",
    "threshold_changed",
    "time_lock_changed",
    "transaction_buffer_closed",
    "transaction_buffer_created",
    "transaction_buffer_extended",
    "transaction_closed",
    "transaction_created",
    "transaction_executed",
];

fn actions(instruction: &SquadsSmartAccountProgramInstruction) -> &'static [&'static str] {
    use SquadsSmartAccountProgramInstruction::*;

    match instruction {
        ActivateProposal { .. } => &["proposal_activated"],
        AddSignerAsAuthority { .. } => &["member_added"],
        AddSpendingLimitAsAuthority { .. } => &["spending_limit_added"],
        AddTransactionToBatch { .. } => &["batch_transaction_added"],
        ApproveProposal { .. } => &["proposal_approved"],
        CancelProposal { .. } => &["proposal_cancelled"],
        ChangeThresholdAsAuthority { .. } => &["threshold_changed"],
        CloseBatch { .. } => &["batch_closed"],
        CloseBatchTransaction { .. } => &["batch_transaction_closed"],
        CloseEmptyPolicyTransaction { .. } => &["empty_policy_transaction_closed"],
        CloseSettingsTransaction { .. } => &["settings_transaction_closed"],
        CloseTransaction { .. } => &["transaction_closed"],
        CloseTransactionBuffer { .. } => &["transaction_buffer_closed"],
        CreateBatch { .. } => &["batch_created"],
        CreateProposal { .. } => &["proposal_created"],
        CreateSettingsTransaction { .. } => &["settings_transaction_created"],
        CreateSmartAccount { .. } => &["multisig_created"],
        CreateTransaction { .. } | CreateTransactionFromBuffer { .. } => &["transaction_created"],
        CreateTransactionBuffer { .. } => &["transaction_buffer_created"],
        ExecuteBatchTransaction { .. } => &["batch_transaction_executed"],
        ExecuteSettingsTransaction { .. } | ExecuteSettingsTransactionSync { .. } => {
            &["settings_transaction_executed"]
        }
        ExecuteTransaction { .. }
        | ExecuteTransactionSync { .. }
        | ExecuteTransactionSyncV2 { .. } => &["transaction_executed"],
        ExtendTransactionBuffer { .. } => &["transaction_buffer_extended"],
        InitializeProgramConfig { .. } => &["program_configuration_initialized"],
        LogEvent { .. } => &["event_logged"],
        RejectProposal { .. } => &["proposal_rejected"],
        RemoveSignerAsAuthority { .. } => &["member_removed"],
        RemoveSpendingLimitAsAuthority { .. } => &["spending_limit_removed"],
        SetArchivalAuthorityAsAuthority { .. } => &["archival_authority_changed"],
        SetNewSettingsAuthorityAsAuthority { .. } => &["settings_authority_changed"],
        SetProgramConfigAuthority { .. } => &["program_configuration_authority_changed"],
        SetProgramConfigSmartAccountCreationFee { .. } => {
            &["program_smart_account_creation_fee_changed"]
        }
        SetProgramConfigTreasury { .. } => &["program_treasury_changed"],
        SetTimeLockAsAuthority { .. } => &["time_lock_changed"],
        UseSpendingLimit { .. } => &["spending_limit_used"],
    }
}

fn applied_settings_actions(
    instruction: &SquadsSmartAccountProgramInstruction,
) -> Vec<&'static str> {
    let SquadsSmartAccountProgramInstruction::ExecuteSettingsTransactionSync { data, .. } =
        instruction
    else {
        return Vec::new();
    };

    data.args
        .actions
        .iter()
        .map(|action| match action {
            SettingsAction::AddSigner { .. } => "member_added",
            SettingsAction::RemoveSigner { .. } => "member_removed",
            SettingsAction::ChangeThreshold { .. } => "threshold_changed",
            SettingsAction::SetTimeLock { .. } => "time_lock_changed",
            SettingsAction::AddSpendingLimit { .. } => "spending_limit_added",
            SettingsAction::RemoveSpendingLimit { .. } => "spending_limit_removed",
            SettingsAction::SetArchivalAuthority { .. } => "archival_authority_changed",
            SettingsAction::PolicyCreate { .. } => "policy_created",
            SettingsAction::PolicyUpdate { .. } => "policy_updated",
            SettingsAction::PolicyRemove { .. } => "policy_removed",
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use carbon_core::instruction::InstructionDecoder;
    use carbon_squads_smart_account_decoder::{
        types::{SettingsAction, SyncSettingsTransactionArgs},
        SquadsSmartAccountProgramDecoder, PROGRAM_ID,
    };
    use solana_instruction::{AccountMeta, Instruction};
    use solana_pubkey::Pubkey;

    use super::{actions, applied_settings_actions, state_address};

    #[test]
    fn decodes_and_normalizes_a_smart_account_proposal_activation() {
        let settings = Pubkey::new_unique();
        let unrelated_vault = Pubkey::new_unique();
        let instruction = Instruction {
            program_id: PROGRAM_ID,
            accounts: [
                settings,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                unrelated_vault,
            ]
            .into_iter()
            .map(|address| AccountMeta::new(address, false))
            .collect(),
            data: vec![90, 186, 203, 234, 70, 185, 191, 21],
        };

        let decoded = SquadsSmartAccountProgramDecoder
            .decode_instruction(&instruction)
            .expect("smart account proposal activation decodes");
        assert_eq!(actions(&decoded), &["proposal_activated"]);
        assert_eq!(state_address(&decoded), Some(settings));
        assert_ne!(state_address(&decoded), Some(unrelated_vault));
        assert!(applied_settings_actions(&decoded).is_empty());
    }

    #[test]
    fn reports_each_change_a_synchronous_settings_execution_applies() {
        let settings = Pubkey::new_unique();
        let mut data = vec![138, 209, 64, 163, 79, 67, 233, 76];
        borsh::to_writer(
            &mut data,
            &SyncSettingsTransactionArgs {
                num_signers: 2,
                actions: vec![
                    SettingsAction::RemoveSigner {
                        old_signer: Pubkey::new_unique(),
                    },
                    SettingsAction::ChangeThreshold { new_threshold: 3 },
                ],
                memo: None,
            },
        )
        .unwrap();
        let instruction = Instruction {
            program_id: PROGRAM_ID,
            accounts: [
                settings,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                PROGRAM_ID,
            ]
            .into_iter()
            .map(|address| AccountMeta::new(address, false))
            .collect(),
            data,
        };

        let decoded = SquadsSmartAccountProgramDecoder
            .decode_instruction(&instruction)
            .expect("synchronous settings execution decodes");
        assert_eq!(state_address(&decoded), Some(settings));
        assert_eq!(
            applied_settings_actions(&decoded),
            ["member_removed", "threshold_changed"]
        );
        assert_eq!(actions(&decoded), ["settings_transaction_executed"]);
    }
}
