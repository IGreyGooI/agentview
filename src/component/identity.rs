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

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
