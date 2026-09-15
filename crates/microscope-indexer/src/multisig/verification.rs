use std::fmt;

use solana_pubkey::Pubkey;

use super::{smart_account, v3, v4};
use crate::config::MultisigVersion;

#[derive(Debug, Clone, Copy)]
pub struct VerifiedMultisig {
    pub vault_address: Pubkey,
    pub state_address: Pubkey,
    pub version: MultisigVersion,
}

#[derive(Debug)]
pub struct VerificationError {
    vault_address: Pubkey,
    state_address: Pubkey,
    version: MultisigVersion,
}

impl fmt::Display for VerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "multisig.state_address {} does not derive configured Squads {} vault_address {}; \
             state_address is the internal Squads account (v3 Ms, v4 Multisig, v5 Settings), \
             not the vault the Squads UI shows, and only one version verifies a given pair; \
             see docs/operations.md",
            self.state_address, self.version, self.vault_address
        )
    }
}

impl std::error::Error for VerificationError {}

pub fn verify(
    vault_address: Pubkey,
    state_address: Pubkey,
    version: MultisigVersion,
) -> Result<VerifiedMultisig, VerificationError> {
    let derived_vault = match version {
        MultisigVersion::V3 => v3::default_vault(state_address),
        MultisigVersion::V4 => v4::default_vault(state_address),
        MultisigVersion::V5 => smart_account::default_vault(state_address),
    };
    if derived_vault != vault_address {
        return Err(VerificationError {
            vault_address,
            state_address,
            version,
        });
    }
    Ok(VerifiedMultisig {
        vault_address,
        state_address,
        version,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use solana_pubkey::Pubkey;

    use super::verify;
    use crate::{
        config::MultisigVersion,
        multisig::{smart_account, v3, v4},
    };

    #[test]
    fn verifies_vault_derivation_for_every_squads_generation() {
        let state_address = Pubkey::new_unique();
        for (version, vault_address) in [
            (MultisigVersion::V3, v3::default_vault(state_address)),
            (MultisigVersion::V4, v4::default_vault(state_address)),
            (
                MultisigVersion::V5,
                smart_account::default_vault(state_address),
            ),
        ] {
            let verified = verify(vault_address, state_address, version).unwrap();
            assert_eq!(verified.vault_address, vault_address);
            assert_eq!(verified.state_address, state_address);
            assert_eq!(verified.version, version);
        }
    }

    #[test]
    fn verifies_a_known_v4_vault_and_state_address_pair() {
        let vault_address =
            Pubkey::from_str("DXtFpbPjcn2hxPnw79x1Pfoj35vXh5AsWBkS37YnXMVv").unwrap();
        let state_address =
            Pubkey::from_str("4CxQs26DewQ1KaCHfyyjktkYjndNdUqCJvVdygtJFwcJ").unwrap();

        verify(vault_address, state_address, MultisigVersion::V4).unwrap();
    }

    #[test]
    fn rejects_mismatched_addresses_and_versions() {
        let state_address = Pubkey::new_unique();
        let v4_vault = v4::default_vault(state_address);

        assert!(verify(Pubkey::new_unique(), state_address, MultisigVersion::V4).is_err());
        assert!(verify(v4_vault, state_address, MultisigVersion::V3).is_err());
        assert!(verify(v4_vault, state_address, MultisigVersion::V5).is_err());
    }
}
