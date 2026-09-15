use carbon_core::{
    error::CarbonResult, instruction::InstructionProcessorInputType, processor::Processor,
};
use carbon_squads_v3_decoder::{instructions::SquadsMplInstruction, PROGRAM_ID};
use solana_pubkey::Pubkey;

use super::common;

pub struct SquadsV3Processor {
    address_resolver: common::AddressResolver,
}

impl SquadsV3Processor {
    pub fn new(vault_address: Pubkey, state_address: Pubkey) -> Self {
        Self {
            address_resolver: common::AddressResolver::new(vault_address, state_address, "v3"),
        }
    }
}

impl Processor<InstructionProcessorInputType<'_, SquadsMplInstruction>> for SquadsV3Processor {
    async fn process(
        &mut self,
        input: &InstructionProcessorInputType<'_, SquadsMplInstruction>,
    ) -> CarbonResult<()> {
        common::process(
            input,
            self.address_resolver
                .match_state(state_address(input.decoded_instruction)),
            "1.3.0",
            actions(input.decoded_instruction),
        )
    }
}

pub(crate) fn default_vault(multisig_state: Pubkey) -> Pubkey {
    let authority_index = 1_u32.to_le_bytes();
    Pubkey::find_program_address(
        &[
            b"squad",
            multisig_state.as_ref(),
            &authority_index,
            b"authority",
        ],
        &PROGRAM_ID,
    )
    .0
}

fn state_address(instruction: &SquadsMplInstruction) -> Pubkey {
    use SquadsMplInstruction::*;

    match instruction {
        ActivateTransaction { accounts, .. } => accounts.multisig,
        AddAuthority { accounts, .. } => accounts.multisig,
        AddInstruction { accounts, .. } => accounts.multisig,
        AddMember { accounts, .. } => accounts.multisig,
        AddMemberAndChangeThreshold { accounts, .. } => accounts.multisig,
        ApproveTransaction { accounts, .. } => accounts.multisig,
        CancelTransaction { accounts, .. } => accounts.multisig,
        ChangeThreshold { accounts, .. } => accounts.multisig,
        Create { accounts, .. } => accounts.multisig,
        CreateTransaction { accounts, .. } => accounts.multisig,
        ExecuteInstruction { accounts, .. } => accounts.multisig,
        ExecuteTransaction { accounts, .. } => accounts.multisig,
        RejectTransaction { accounts, .. } => accounts.multisig,
        RemoveMember { accounts, .. } => accounts.multisig,
        RemoveMemberAndChangeThreshold { accounts, .. } => accounts.multisig,
    }
}

pub(super) const ACTIONS: &[&str] = &[
    "member_added",
    "member_removed",
    "multisig_created",
    "proposal_activated",
    "proposal_approved",
    "proposal_cancelled",
    "proposal_rejected",
    "threshold_changed",
    "transaction_created",
    "transaction_executed",
    "transaction_instruction_added",
    "transaction_instruction_executed",
    "vault_authority_added",
];

fn actions(instruction: &SquadsMplInstruction) -> &'static [&'static str] {
    use SquadsMplInstruction::*;

    match instruction {
        ActivateTransaction { .. } => &["proposal_activated"],
        AddAuthority { .. } => &["vault_authority_added"],
        AddInstruction { .. } => &["transaction_instruction_added"],
        AddMember { .. } => &["member_added"],
        AddMemberAndChangeThreshold { .. } => &["member_added", "threshold_changed"],
        ApproveTransaction { .. } => &["proposal_approved"],
        CancelTransaction { .. } => &["proposal_cancelled"],
        ChangeThreshold { .. } => &["threshold_changed"],
        Create { .. } => &["multisig_created"],
        CreateTransaction { .. } => &["transaction_created"],
        ExecuteInstruction { .. } => &["transaction_instruction_executed"],
        ExecuteTransaction { .. } => &["transaction_executed"],
        RejectTransaction { .. } => &["proposal_rejected"],
        RemoveMember { .. } => &["member_removed"],
        RemoveMemberAndChangeThreshold { .. } => &["member_removed", "threshold_changed"],
    }
}

#[cfg(test)]
mod tests {
    use carbon_core::instruction::InstructionDecoder;
    use carbon_squads_v3_decoder::{SquadsMplDecoder, PROGRAM_ID};
    use solana_instruction::{AccountMeta, Instruction};
    use solana_pubkey::Pubkey;

    use super::{actions, state_address};

    #[test]
    fn decodes_and_normalizes_a_v3_approval() {
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
            data: vec![224, 39, 88, 181, 36, 59, 155, 122],
        };

        let decoded = SquadsMplDecoder
            .decode_instruction(&instruction)
            .expect("v3 approval decodes");
        assert_eq!(actions(&decoded), &["proposal_approved"]);
        assert_eq!(state_address(&decoded), multisig);
        assert_ne!(state_address(&decoded), unrelated_vault);
    }
}
