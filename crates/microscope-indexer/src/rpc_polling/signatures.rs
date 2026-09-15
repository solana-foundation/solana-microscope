use std::str::FromStr;

use anyhow::Context;
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_config::RpcSignaturesForAddressConfig,
    rpc_request::RpcRequest, rpc_response::RpcConfirmedTransactionStatusWithSignature,
};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;

const SIGNATURE_PAGE_LIMIT: usize = 1_000;
/// Roughly thirteen seconds of slots, enough for the replica skew a
/// load-balanced endpoint shows between two consecutive requests.
const REPLICA_SKEW_SLOTS: u64 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AddressCursor {
    pub(super) scanned_slot: u64,
}

#[derive(Debug)]
pub(super) struct AddressBatch {
    pub(super) address: Pubkey,
    pub(super) signatures: Vec<DiscoveredSignature>,
    pub(super) next_cursor: AddressCursor,
    pub(super) reached_replay_floor: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DiscoveredSignature {
    pub(super) signature: Signature,
    pub(super) slot: u64,
}

pub(super) async fn discover_signatures(
    rpc_client: &RpcClient,
    address: Pubkey,
    cursor: &AddressCursor,
    head_slot: u64,
    replay_window_slots: u64,
) -> anyhow::Result<AddressBatch> {
    let floor = cursor.scanned_slot.saturating_sub(replay_window_slots);
    let min_context_slot = min_context_slot(head_slot, replay_window_slots);
    let mut before = None;
    let mut signatures = Vec::new();
    let mut newest_slot = None;
    let mut reached_replay_floor = false;

    loop {
        let config = RpcSignaturesForAddressConfig {
            before: before.map(|signature: Signature| signature.to_string()),
            until: None,
            limit: Some(SIGNATURE_PAGE_LIMIT),
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: Some(min_context_slot),
        };
        let page: Vec<RpcConfirmedTransactionStatusWithSignature> = rpc_client
            .send(
                RpcRequest::GetSignaturesForAddress,
                serde_json::json!([address.to_string(), config]),
            )
            .await
            .with_context(|| format!("failed to fetch signatures for {address}"))?;
        if page.is_empty() {
            break;
        }

        if append_signature_page(
            page.iter().map(|info| (info.slot, info.signature.as_str())),
            floor,
            &mut signatures,
            &mut newest_slot,
        )? {
            reached_replay_floor = true;
            break;
        }

        if page.len() < SIGNATURE_PAGE_LIMIT {
            break;
        }
        let Some(page_end) = signatures.last() else {
            break;
        };
        before = Some(page_end.signature);
    }

    Ok(AddressBatch {
        address,
        signatures,
        next_cursor: AddressCursor {
            scanned_slot: head_slot.max(newest_slot.unwrap_or(cursor.scanned_slot)),
        },
        reached_replay_floor,
    })
}

/// Demanding `head_slot` itself fails against any replica trailing the separate
/// slot request. Discovery commits `head_slot` regardless, so the bound instead
/// has to keep whatever a replica omits above the next poll's floor, with room
/// left for the head to advance between polls: spending the whole replay window
/// on skew leaves none.
fn min_context_slot(head_slot: u64, replay_window_slots: u64) -> u64 {
    head_slot.saturating_sub(REPLICA_SKEW_SLOTS.min(replay_window_slots))
}

/// An address index can expose a signature after the poll that advanced the
/// cursor past its slot. The checkpoint's recent signatures drop the
/// duplicates this overlap re-lists.
fn append_signature_page<'a>(
    page: impl Iterator<Item = (u64, &'a str)>,
    floor: u64,
    signatures: &mut Vec<DiscoveredSignature>,
    newest_slot: &mut Option<u64>,
) -> anyhow::Result<bool> {
    for (slot, encoded_signature) in page {
        if slot <= floor {
            return Ok(true);
        }
        let signature = Signature::from_str(encoded_signature)
            .with_context(|| format!("RPC returned invalid signature {encoded_signature}"))?;
        if newest_slot.is_none() {
            *newest_slot = Some(slot);
        }
        signatures.push(DiscoveredSignature { signature, slot });
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{append_signature_page, min_context_slot, AddressCursor, REPLICA_SKEW_SLOTS};

    fn signature(value: u64) -> solana_signature::Signature {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        solana_signature::Signature::from(bytes)
    }

    #[test]
    fn leaves_a_lagging_replica_inside_the_window_the_next_poll_rescans() {
        let head_slot = 500_000;
        let replay_window_slots = 300;

        let covered_slot = min_context_slot(head_slot, replay_window_slots);
        let next_floor = head_slot.saturating_sub(replay_window_slots);

        assert!(
            covered_slot < head_slot,
            "a replica trailing the head must still answer"
        );
        assert!(
            next_floor < covered_slot,
            "the next poll must rescan every slot the replica could omit"
        );
        assert_eq!(
            covered_slot - next_floor,
            replay_window_slots - REPLICA_SKEW_SLOTS,
            "the rest of the window absorbs the head advancing between polls"
        );
    }

    #[test]
    fn never_demands_more_coverage_than_the_replay_window_allows() {
        assert_eq!(min_context_slot(500_000, 1), 499_999);
        assert_eq!(min_context_slot(10, 300), 0);
    }

    #[test]
    fn selects_signatures_down_to_the_replay_floor() {
        let newer = signature(2);
        let at_cursor = signature(1);
        let cursor = AddressCursor { scanned_slot: 100 };
        let floor = cursor.scanned_slot - 10;
        let mut signatures = Vec::new();
        let mut newest_slot = None;

        let reached_floor = append_signature_page(
            [
                (102, newer.to_string()),
                (100, at_cursor.to_string()),
                (90, signature(0).to_string()),
            ]
            .iter()
            .map(|(slot, encoded)| (*slot, encoded.as_str())),
            floor,
            &mut signatures,
            &mut newest_slot,
        )
        .unwrap();

        assert!(reached_floor);
        assert_eq!(signatures.len(), 2);
        assert_eq!(signatures[0].signature, newer);
        assert_eq!(signatures[1].signature, at_cursor);
        assert_eq!(newest_slot, Some(102));
    }

    #[test]
    fn rejects_a_page_holding_an_unparsable_signature() {
        let valid = signature(3).to_string();
        let mut signatures = Vec::new();
        let mut newest_slot = None;

        let error = append_signature_page(
            [(102, valid.as_str()), (101, "not-a-signature")].into_iter(),
            0,
            &mut signatures,
            &mut newest_slot,
        )
        .expect_err("an unparsable signature must not be skipped");

        assert!(error.to_string().contains("not-a-signature"), "{error}");
    }

    #[test]
    fn recovers_a_signature_exposed_after_its_slot_was_scanned() {
        let late = signature(7);
        let cursor = AddressCursor { scanned_slot: 100 };
        let mut signatures = Vec::new();
        let mut newest_slot = None;

        append_signature_page(
            [(95, late.to_string())]
                .iter()
                .map(|(slot, encoded)| (*slot, encoded.as_str())),
            cursor.scanned_slot.saturating_sub(300),
            &mut signatures,
            &mut newest_slot,
        )
        .unwrap();

        assert_eq!(signatures.len(), 1);
        assert_eq!(signatures[0].signature, late);
    }
}
