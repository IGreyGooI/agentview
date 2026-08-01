use super::{XmlName, XmlNode};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiffStrategy {
    Recursive,
    Replace,
    Append,
    Sequence,
    Set,
    Keyed(XmlName),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiffSlot {
    role: XmlName,
    strategy: DiffStrategy,
    value: Option<XmlNode>,
}

impl<'de> serde::Deserialize<'de> for DiffSlot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct RawDiffSlot {
            role: XmlName,
            strategy: DiffStrategy,
            value: Option<XmlNode>,
        }

        let raw = RawDiffSlot::deserialize(deserializer)?;
        if let Some(value) = raw.value {
            if raw.role != *value.name() {
                return Err(serde::de::Error::custom(format!(
                    "diff slot role `{}` does not match value root `{}`",
                    raw.role,
                    value.name()
                )));
            }
            Ok(Self::present(raw.strategy, value))
        } else {
            Ok(Self::absent(raw.role, raw.strategy))
        }
    }
}

impl DiffSlot {
    pub fn present(strategy: DiffStrategy, value: XmlNode) -> Self {
        Self {
            role: value.name().clone(),
            strategy,
            value: Some(value),
        }
    }

    pub fn absent(role: XmlName, strategy: DiffStrategy) -> Self {
        Self {
            role,
            strategy,
            value: None,
        }
    }

    pub fn role(&self) -> &XmlName {
        &self.role
    }

    pub fn strategy(&self) -> &DiffStrategy {
        &self.strategy
    }

    pub fn value(&self) -> Option<&XmlNode> {
        self.value.as_ref()
    }

    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }
}
