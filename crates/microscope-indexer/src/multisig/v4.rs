use carbon_core::{
    error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor,
};
use carbon_squads_v4_decoder::{instructions::SquadsMultisigProgramInstruction, PROGRAM_ID};
use solana_pubkey::Pubkey;

use super::common;

pub struct SquadsV4Processor {
    address_resolver: common::AddressResolver,
}

impl SquadsV4Processor {
    pub fn new(vault_address: Pubkey, state_address: Pubkey) -> Self {
        Self {
            address_resolver: common::AddressResolver::new(vault_address, state_address, "v4"),
        }
    }
}

impl Processor<InstructionProcessorInputType<'_, SquadsMultisigProgramInstruction>>
    for SquadsV4Processor
{
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, SquadsMultisigProgramInstruction>,
    ) -> CarbonResult<()> {
        let address_match = state_address(input.decoded_instruction)
            .and_then(|state_address| self.address_resolver.match_state(state_address));
        common::process(
            input,
            address_match,
            "2.1.0",
            actions(input.decoded_instruction),
        )
    }
}

pub(crate) fn default_vault(multisig_state: Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"multisig", multisig_state.as_ref(), b"vault", &[0]],
        &PROGRAM_ID,
    )
    .0
}

fn state_address(instruction: &SquadsMultisigProgramInstruction) -> Option<Pubkey> {
    use SquadsMultisigProgramInstruction::*;

    let state_address = match instruction {
        BatchAccountsClose { accounts, .. } => accounts.multisig,
        BatchAddTransaction { accounts, .. } => accounts.multisig,
        BatchCreate { accounts, .. } => accounts.multisig,
        BatchExecuteTransaction { accounts, .. } => accounts.multisig,
        ConfigTransactionAccountsClose { accounts, .. } => accounts.multisig,
        ConfigTransactionCreate { accounts, .. } => accounts.multisig,
        ConfigTransactionExecute { accounts, .. } => accounts.multisig,
        MultisigAddMember { accounts, .. } => accounts.multisig,
        MultisigAddSpendingLimit { accounts, .. } => accounts.multisig,
        MultisigChangeThreshold { accounts, .. } => accounts.multisig,
        MultisigCreateV2 { accounts, .. } => accounts.multisig,
        MultisigRemoveMember { accounts, .. } => accounts.multisig,
        MultisigRemoveSpendingLimit { accounts, .. } => accounts.multisig,
        MultisigSetConfigAuthority { accounts, .. } => accounts.multisig,
        MultisigSetRentCollector { accounts, .. } => accounts.multisig,
        MultisigSetTimeLock { accounts, .. } => accounts.multisig,
        ProposalActivate { accounts, .. } => accounts.multisig,
        ProposalApprove { accounts, .. } => accounts.multisig,
        ProposalCancel { accounts, .. } => accounts.multisig,
        ProposalCancelV2 { accounts, .. } => accounts.multisig,
        ProposalCreate { accounts, .. } => accounts.multisig,
        ProposalReject { accounts, .. } => accounts.multisig,
        SpendingLimitUse { accounts, .. } => accounts.multisig,
        TransactionBufferClose { accounts, .. } => accounts.multisig,
        TransactionBufferCreate { accounts, .. } => accounts.multisig,
        TransactionBufferExtend { accounts, .. } => accounts.multisig,
        VaultBatchTransactionAccountClose { accounts, .. } => accounts.multisig,
        VaultTransactionAccountsClose { accounts, .. } => accounts.multisig,
        VaultTransactionCreate { accounts, .. } => accounts.multisig,
        VaultTransactionCreateFromBuffer { accounts, .. } => accounts.multisig,
        VaultTransactionExecute { accounts, .. } => accounts.multisig,
        MultisigCreate { .. }
        | ProgramConfigInit { .. }
        | ProgramConfigSetAuthority { .. }
        | ProgramConfigSetMultisigCreationFee { .. }
        | ProgramConfigSetTreasury { .. } => return None,
    };

    Some(state_address)
}

pub(super) const ACTIONS: &[&str] = &[
    "batch_closed",
    "batch_created",
    "batch_transaction_added",
    "batch_transaction_closed",
    "batch_transaction_executed",
    "configuration_authority_changed",
    "configuration_transaction_closed",
    "configuration_transaction_created",
    "configuration_transaction_executed",
    "member_added",
    "member_removed",
    "multisig_created",
    "program_configuration_authority_changed",
    "program_configuration_initialized",
    "program_multisig_creation_fee_changed",
    "program_treasury_changed",
    "proposal_activated",
    "proposal_approved",
    "proposal_cancelled",
    "proposal_created",
    "proposal_rejected",
    "rent_collector_changed",
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

fn actions(instruction: &SquadsMultisigProgramInstruction) -> &'static [&'static str] {
    use SquadsMultisigProgramInstruction::*;

    match instruction {
        BatchAccountsClose { .. } => &["batch_closed"],
        BatchAddTransaction { .. } => &["batch_transaction_added"],
        BatchCreate { .. } => &["batch_created"],
        BatchExecuteTransaction { .. } => &["batch_transaction_executed"],
        ConfigTransactionAccountsClose { .. } => &["configuration_transaction_closed"],
        ConfigTransactionCreate { .. } => &["configuration_transaction_created"],
        ConfigTransactionExecute { .. } => &["configuration_transaction_executed"],
        MultisigAddMember { .. } => &["member_added"],
        MultisigAddSpendingLimit { .. } => &["spending_limit_added"],
        MultisigChangeThreshold { .. } => &["threshold_changed"],
        MultisigCreate { .. } | MultisigCreateV2 { .. } => &["multisig_created"],
        MultisigRemoveMember { .. } => &["member_removed"],
        MultisigRemoveSpendingLimit { .. } => &["spending_limit_removed"],
        MultisigSetConfigAuthority { .. } => &["configuration_authority_changed"],
        MultisigSetRentCollector { .. } => &["rent_collector_changed"],
        MultisigSetTimeLock { .. } => &["time_lock_changed"],
        ProgramConfigInit { .. } => &["program_configuration_initialized"],
        ProgramConfigSetAuthority { .. } => &["program_configuration_authority_changed"],
        ProgramConfigSetMultisigCreationFee { .. } => &["program_multisig_creation_fee_changed"],
        ProgramConfigSetTreasury { .. } => &["program_treasury_changed"],
        ProposalActivate { .. } => &["proposal_activated"],
        ProposalApprove { .. } => &["proposal_approved"],
        ProposalCancel { .. } | ProposalCancelV2 { .. } => &["proposal_cancelled"],
        ProposalCreate { .. } => &["proposal_created"],
        ProposalReject { .. } => &["proposal_rejected"],
        SpendingLimitUse { .. } => &["spending_limit_used"],
        TransactionBufferClose { .. } => &["transaction_buffer_closed"],
        TransactionBufferCreate { .. } => &["transaction_buffer_created"],
        TransactionBufferExtend { .. } => &["transaction_buffer_extended"],
        VaultBatchTransactionAccountClose { .. } => &["batch_transaction_closed"],
        VaultTransactionAccountsClose { .. } => &["transaction_closed"],
        VaultTransactionCreate { .. } | VaultTransactionCreateFromBuffer { .. } => {
            &["transaction_created"]
        }
        VaultTransactionExecute { .. } => &["transaction_executed"],
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use carbon_core::instruction::InstructionDecoder;
    use carbon_squads_v4_decoder::{SquadsMultisigProgramDecoder, PROGRAM_ID};
    use solana_instruction::{AccountMeta, Instruction};
    use solana_pubkey::Pubkey;

    use super::{actions, default_vault, state_address};

    #[test]
    fn decodes_and_normalizes_a_v4_proposal_activation() {
        let multisig = Pubkey::new_unique();
        let unrelated_vault = Pubkey::new_unique();
        let instruction = Instruction {
            program_id: PROGRAM_ID,
            accounts: [
                multisig,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                unrelated_vault,
            ]
            .into_iter()
            .map(|address| AccountMeta::new(address, false))
            .collect(),
            data: vec![11, 34, 92, 248, 154, 27, 51, 106],
        };

        let decoded = SquadsMultisigProgramDecoder
            .decode_instruction(&instruction)
            .expect("v4 proposal activation decodes");
        assert_eq!(actions(&decoded), &["proposal_activated"]);
        assert_eq!(state_address(&decoded), Some(multisig));
        assert_ne!(state_address(&decoded), Some(unrelated_vault));
    }

    #[test]
    fn derives_a_known_mainnet_default_vault() {
        let multisig_state =
            Pubkey::from_str("4CxQs26DewQ1KaCHfyyjktkYjndNdUqCJvVdygtJFwcJ").unwrap();
        let expected_vault =
            Pubkey::from_str("DXtFpbPjcn2hxPnw79x1Pfoj35vXh5AsWBkS37YnXMVv").unwrap();

        assert_eq!(default_vault(multisig_state), expected_vault);
    }
}
