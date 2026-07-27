use serde::{Deserialize, Serialize};

use crate::CoreError;

/// A canonical 32-byte Solana Pay reference value.
///
/// A reference is deliberately not modelled as a `PublicKey`: the Solana Pay
/// specification permits any 32-byte value, including an off-curve value.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Reference([u8; 32]);

impl Reference {
    /// # Errors
    ///
    /// Rejects anything other than canonical base58 for exactly 32 bytes.
    pub fn parse(value: impl AsRef<str>) -> Result<Self, CoreError> {
        let value = value.as_ref();
        let bytes = bs58::decode(value)
            .into_vec()
            .map_err(|_| CoreError::InvalidReference)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| CoreError::InvalidReference)?;
        if bs58::encode(bytes).into_string() != value {
            return Err(CoreError::InvalidReference);
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_base58(&self) -> String {
        bs58::encode(self.0).into_string()
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl TryFrom<String> for Reference {
    type Error = CoreError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Reference> for String {
    fn from(value: Reference) -> Self {
        value.as_base58()
    }
}

#[cfg(test)]
mod tests {
    use super::Reference;
    use crate::CoreError;

    #[test]
    fn uses_canonical_base58_for_32_byte_values() {
        let reference = Reference::from_bytes([7; 32]);
        assert_eq!(Reference::parse(reference.as_base58()), Ok(reference));
        assert_eq!(
            Reference::parse("111111111111111111111111111111111"),
            Err(CoreError::InvalidReference)
        );
    }
}
