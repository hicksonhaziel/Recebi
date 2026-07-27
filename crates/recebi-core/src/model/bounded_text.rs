use crate::CoreError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedText<const MAX_BYTES: usize>(String);

impl<const MAX_BYTES: usize> BoundedText<MAX_BYTES> {
    /// # Errors
    ///
    /// Returns a typed error if `value` is empty or exceeds the byte limit.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CoreError::EmptyText);
        }
        if value.len() > MAX_BYTES {
            return Err(CoreError::TextTooLong);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedText;
    use crate::CoreError;

    #[test]
    fn rejects_empty_and_oversized_text() {
        assert_eq!(BoundedText::<3>::new(""), Err(CoreError::EmptyText));
        assert_eq!(BoundedText::<3>::new("four"), Err(CoreError::TextTooLong));
    }
}
