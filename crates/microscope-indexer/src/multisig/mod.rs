pub(crate) mod common;
mod smart_account;
mod v3;
mod v4;
mod verification;

pub use smart_account::SmartAccountProcessor;
pub use v3::SquadsV3Processor;
pub use v4::SquadsV4Processor;
pub use verification::{verify, VerifiedMultisig};

use crate::config::MultisigVersion;

pub(crate) fn action_names(version: MultisigVersion) -> &'static [&'static str] {
    match version {
        MultisigVersion::V3 => v3::ACTIONS,
        MultisigVersion::V4 => v4::ACTIONS,
        MultisigVersion::V5 => smart_account::ACTIONS,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{smart_account, v3, v4};

    fn normalized_action_names(source: &str) -> BTreeSet<&str> {
        let from_lists = source
            .match_indices("&[\"")
            .filter_map(|(start, matched)| {
                let list = &source[start + matched.len()..];
                Some(&list[..list.find(']')?])
            })
            .flat_map(|list| list.split(','))
            .map(|entry| entry.trim().trim_matches('"'));
        let from_match_arms = source
            .match_indices("=> \"")
            .filter_map(|(start, matched)| {
                let arm = &source[start + matched.len()..];
                Some(&arm[..arm.find('"')?])
            })
            .filter(|action| !action.is_empty());
        from_lists.chain(from_match_arms).collect()
    }

    #[test]
    fn every_emitted_action_name_is_declared_for_config_validation() {
        for (source, declared) in [
            (include_str!("v3.rs"), v3::ACTIONS),
            (include_str!("v4.rs"), v4::ACTIONS),
            (include_str!("smart_account.rs"), smart_account::ACTIONS),
        ] {
            let declared = declared.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(normalized_action_names(source), declared);
        }
    }
}
