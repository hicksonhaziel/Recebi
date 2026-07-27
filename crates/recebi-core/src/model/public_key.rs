use crate::CoreError;
use serde::{Deserialize, Serialize};

/// A canonical base58-encoded 32-byte Solana public key.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicKey(String);

impl PublicKey {
    /// # Errors
    ///
    /// Returns `InvalidPublicKey` unless `value` is canonical base58 encoding
    /// of exactly 32 bytes.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, CoreError> {
        let value = value.as_ref();
        let bytes = bs58::decode(value)
            .into_vec()
            .map_err(|_| CoreError::InvalidPublicKey)?;
        if bytes.len() != 32 || bs58::encode(bytes).into_string() != value {
            return Err(CoreError::InvalidPublicKey);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PublicKey {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<PublicKey> for String {
    fn from(value: PublicKey) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::PublicKey;
    use crate::CoreError;

    #[test]
    fn accepts_canonical_32_byte_base58_key() {
        assert!(PublicKey::parse("11111111111111111111111111111111").is_ok());
    }

    #[test]
    fn rejects_invalid_or_noncanonical_public_keys() {
        assert_eq!(
            PublicKey::parse("not-a-public-key"),
            Err(CoreError::InvalidPublicKey)
        );
        assert_eq!(
            PublicKey::parse("111111111111111111111111111111111"),
            Err(CoreError::InvalidPublicKey)
        );
    }
}
