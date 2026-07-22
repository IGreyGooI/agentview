use crate::StorageString;

use super::PomError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XmlName {
    value: StorageString,
}

impl XmlName {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, PomError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let is_valid = bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));

        if is_valid {
            Ok(Self { value })
        } else {
            Err(PomError::InvalidXmlName { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl TryFrom<&str> for XmlName {
    type Error = PomError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
