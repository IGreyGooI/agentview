use std::fmt;

use crate::StorageString;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ComponentId(StorageString);

impl ComponentId {
    pub(crate) fn root() -> Self {
        Self("root".into())
    }

    pub(crate) fn child(&self, name: &str, position: usize) -> Self {
        let identity = format!("{name}#{position}");
        Self(format!("{}/{identity}", self.0).into())
    }

    pub(crate) fn system_child(&self, name: &str, position: usize) -> Self {
        let identity = format!("{name}#system:{position}");
        Self(format!("{}/{identity}", self.0).into())
    }

    /// Length-prefix every owned segment so keys cannot collide through path
    /// separators, punctuation, or ambiguity between type and instance names.
    pub(crate) fn dynamic_child(
        &self,
        type_id: &str,
        instance_key: &str,
        compatibility_key: &str,
        system: bool,
    ) -> Self {
        Self(
            format!(
                "{}/@dynamic:{}:{}:{}:{}:{}:{}:{};",
                self.0,
                u8::from(system),
                type_id.len(),
                type_id,
                instance_key.len(),
                instance_key,
                compatibility_key.len(),
                compatibility_key,
            )
            .into(),
        )
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
