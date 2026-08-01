use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::StorageString;

use super::ComponentError;

fn next_runtime_id(counter: &AtomicU64, kind: &'static str) -> u64 {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .unwrap_or_else(|_| panic!("{kind} identity space exhausted"))
}

macro_rules! runtime_id {
    ($name:ident, $counter:ident, $prefix:literal) => {
        static $counter: AtomicU64 = AtomicU64::new(1);

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(u64);

        impl $name {
            pub(crate) fn fresh() -> Self {
                Self(next_runtime_id(&$counter, stringify!($name)))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

runtime_id!(HarnessEpochId, NEXT_HARNESS_EPOCH_ID, "epoch-");
runtime_id!(TurnInstanceId, NEXT_TURN_INSTANCE_ID, "turn-");
runtime_id!(ProviderAttemptId, NEXT_PROVIDER_ATTEMPT_ID, "attempt-");
runtime_id!(LiveScopeId, NEXT_LIVE_SCOPE_ID, "live-");

fn validate_key(
    value: impl Into<StorageString>,
    kind: &'static str,
) -> Result<StorageString, ComponentError> {
    let value = value.into();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ComponentError::InvalidKey {
            kind,
            value: value.to_string(),
        });
    }
    Ok(value)
}

/// Stable identity supplied by a parent for a repeated child component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentKey(StorageString);

impl ComponentKey {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ComponentError> {
        validate_key(value, "component").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComponentKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable local name for a hook or other runtime binding owned by a component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BindingKey(StorageString);

impl BindingKey {
    pub fn new(value: impl Into<StorageString>) -> Result<Self, ComponentError> {
        validate_key(value, "binding").map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable path assigned while compiling one component tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentId(StorageString);

impl ComponentId {
    pub(crate) fn root() -> Self {
        Self("root".into())
    }

    pub(crate) fn child(&self, name: &str, key: Option<&ComponentKey>, position: usize) -> Self {
        let identity = match key {
            Some(key) => format!("{name}[{key}]"),
            None => format!("{name}#{position}"),
        };
        Self(format!("{}/{identity}", self.0).into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable identity of one compiled runtime binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BindingId {
    component: ComponentId,
    key: BindingKey,
}

impl BindingId {
    pub(crate) fn new(component: ComponentId, key: BindingKey) -> Self {
        Self { component, key }
    }

    pub fn component(&self) -> &ComponentId {
        &self.component
    }

    pub fn key(&self) -> &BindingKey {
        &self.key
    }
}

impl fmt::Display for BindingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}::{}", self.component, self.key)
    }
}
