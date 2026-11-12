//! Owned Component implementations with stable, explicitly keyed instances.

use std::{fmt, sync::Arc};

use super::{declaration::ComponentNode, Component, ComponentHooks};

type DefinitionRenderer = dyn for<'hook, 'render> Fn(
        &mut ComponentHooks<'hook, 'render>,
    ) -> Result<Component, ComponentDefinitionError>
    + Send
    + Sync
    + 'static;

/// A replaceable implementation of a logical Component type.
///
/// The type and compatibility keys describe retained state, independently of
/// the renderer's implementation. Reuse both keys for a compatible update.
/// Changing either key mounts a fresh instance and retires its previous subtree.
/// Hook slots must retain their order, kind, site and state type while compatible.
#[derive(Clone)]
pub struct ComponentDefinition {
    type_id: Arc<str>,
    compatibility_key: Arc<str>,
    renderer: Arc<DefinitionRenderer>,
}

impl ComponentDefinition {
    /// Construct an owned implementation without a Rust authoring macro.
    pub fn new<Renderer>(
        type_id: impl Into<String>,
        compatibility_key: impl Into<String>,
        renderer: Renderer,
    ) -> Result<Self, ComponentDefinitionError>
    where
        Renderer: for<'hook, 'render> Fn(
                &mut ComponentHooks<'hook, 'render>,
            ) -> Result<Component, ComponentDefinitionError>
            + Send
            + Sync
            + 'static,
    {
        let type_id = type_id.into();
        let compatibility_key = compatibility_key.into();
        validate_key("Component type", &type_id)?;
        validate_key("Component compatibility", &compatibility_key)?;
        Ok(Self {
            type_id: type_id.into(),
            compatibility_key: compatibility_key.into(),
            renderer: Arc::new(renderer),
        })
    }

    pub fn type_id(&self) -> &str {
        &self.type_id
    }

    pub fn compatibility_key(&self) -> &str {
        &self.compatibility_key
    }

    /// Declare one instance identified by a key unique under its owning parent.
    ///
    /// The key does not depend on sibling order. Its validity and uniqueness are
    /// checked while staging the complete render, before any lifecycle commit.
    pub fn instantiate(&self, instance_key: impl Into<String>) -> Component {
        Component::from_node(ComponentNode::DynamicScope {
            definition: self.clone(),
            instance_key: instance_key.into(),
        })
    }

    pub(crate) fn render(
        &self,
        hooks: &mut ComponentHooks<'_, '_>,
    ) -> Result<Component, ComponentDefinitionError> {
        (self.renderer)(hooks)
    }
}

impl fmt::Debug for ComponentDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComponentDefinition")
            .field("type_id", &self.type_id)
            .field("compatibility_key", &self.compatibility_key)
            .finish_non_exhaustive()
    }
}

/// An invalid definition or a fallible external Component renderer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ComponentDefinitionError {
    message: String,
}

impl ComponentDefinitionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) fn validate_key(kind: &str, value: &str) -> Result<(), ComponentDefinitionError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ComponentDefinitionError::new(format!(
            "{kind} key must be nonempty and contain no control characters"
        )));
    }
    Ok(())
}
