use std::str::FromStr;

use solana_pubkey::Pubkey;

use crate::{CoreError, PublicKey};

const CLASSIC_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Derives the canonical classic-SPL associated token account for a wallet and
/// mint. Token-2022 is intentionally unsupported in the Phase 3 MVP.
///
/// # Errors
///
/// Returns a typed error only if a validated public key cannot be converted to
/// the modular Solana address type.
pub fn derive_classic_ata(owner: &PublicKey, mint: &PublicKey) -> Result<PublicKey, CoreError> {
    let owner = Pubkey::from_str(owner.as_str()).map_err(|_| CoreError::AtaDerivation)?;
    let mint = Pubkey::from_str(mint.as_str()).map_err(|_| CoreError::AtaDerivation)?;
    let token_program =
        Pubkey::from_str(CLASSIC_TOKEN_PROGRAM).map_err(|_| CoreError::AtaDerivation)?;
    let associated_program =
        Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM).map_err(|_| CoreError::AtaDerivation)?;
    let (ata, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    );
    PublicKey::parse(ata.to_string())
}

#[cfg(test)]
mod tests {
    use super::derive_classic_ata;
    use crate::PublicKey;

    #[test]
    fn derives_the_known_mainnet_usdc_ata() {
        let owner =
            PublicKey::parse("4Nd1mJw5JMTHEJoTQ4CE8zsGiySsa4hJr5d7q1pYVmwW").expect("owner");
        let mint = PublicKey::parse("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").expect("mint");
        let ata = derive_classic_ata(&owner, &mint).expect("ATA");
        assert_eq!(ata.as_str(), "BQpvES1YLJG62tzfeJBB2ETkKCmYFHCgqwqse1TQmTVn");
    }
}
