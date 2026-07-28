use serde::{Deserialize, Serialize};

use crate::CoreError;

/// A canonical base58-encoded 32-byte Solana cluster genesis hash.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct GenesisHash(String);

impl GenesisHash {
    /// # Errors
    ///
    /// Rejects non-canonical base58 or values other than 32 bytes.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, CoreError> {
        let value = value.as_ref();
        let bytes = bs58::decode(value)
            .into_vec()
            .map_err(|_| CoreError::InvalidGenesisHash)?;
        if bytes.len() != 32 || bs58::encode(bytes).into_string() != value {
            return Err(CoreError::InvalidGenesisHash);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GenesisHash {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<GenesisHash> for String {
    fn from(value: GenesisHash) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_canonical_32_byte_base58_hashes() {
        let value = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
        assert_eq!(GenesisHash::parse(value).expect("genesis").as_str(), value);
        assert_eq!(
            GenesisHash::parse("not base58!"),
            Err(CoreError::InvalidGenesisHash)
        );
        assert_eq!(
            GenesisHash::parse("1111111111111111111111111111111"),
            Err(CoreError::InvalidGenesisHash)
        );
    }
}
