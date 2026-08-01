use std::sync::Arc;

use crate::{pom::Document, StorageString};

use super::{BindingKey, ComponentError, ComponentKey};

/// Prompt document selected by a component's parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptRole {
    System,
    User,
}

impl std::fmt::Display for PromptRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => formatter.write_str("system"),
            Self::User => formatter.write_str("user"),
        }
    }
}

/// Short-lived authoring IR compiled into two POM documents and a binding plan.
pub struct ComponentNode<B = ()> {
    pub(crate) kind: ComponentNodeKind<B>,
}

pub(crate) enum ComponentNodeKind<B> {
    Empty,
    Pom(Document),
    Fragment(Vec<ComponentNode<B>>),
    Role {
        role: PromptRole,
        child: Box<ComponentNode<B>>,
    },
    Component {
        name: StorageString,
        key: Option<ComponentKey>,
        child: Box<ComponentNode<B>>,
    },
    DeferredComponent {
        name: StorageString,
        key: Option<ComponentKey>,
        render: DeferredRender<B>,
    },
    Binding {
        key: BindingKey,
        binding: B,
    },
}

pub(crate) struct DeferredRender<B> {
    render: Box<dyn FnOnce() -> super::View<B> + Send + 'static>,
}

impl<B> DeferredRender<B> {
    pub(crate) fn new(render: impl FnOnce() -> super::View<B> + Send + 'static) -> Self {
        Self {
            render: Box::new(render),
        }
    }

    pub(crate) fn execute(self) -> super::View<B> {
        (self.render)()
    }

    fn map_binding<C, F>(self, map: Arc<F>) -> DeferredRender<C>
    where
        B: 'static,
        C: 'static,
        F: Fn(B) -> C + Send + Sync + 'static,
    {
        DeferredRender::new(move || {
            self.execute()
                .map(|node| node.map_binding_shared(Arc::clone(&map)))
        })
    }
}

impl<B> std::fmt::Debug for DeferredRender<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeferredRender(..)")
    }
}

impl<B> std::fmt::Debug for ComponentNode<B>
where
    B: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(formatter)
    }
}

impl<B> std::fmt::Debug for ComponentNodeKind<B>
where
    B: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Empty"),
            Self::Pom(document) => formatter.debug_tuple("Pom").field(document).finish(),
            Self::Fragment(children) => formatter.debug_tuple("Fragment").field(children).finish(),
            Self::Role { role, child } => formatter
                .debug_struct("Role")
                .field("role", role)
                .field("child", child)
                .finish(),
            Self::Component { name, key, child } => formatter
                .debug_struct("Component")
                .field("name", name)
                .field("key", key)
                .field("child", child)
                .finish(),
            Self::DeferredComponent { name, key, render } => formatter
                .debug_struct("DeferredComponent")
                .field("name", name)
                .field("key", key)
                .field("render", render)
                .finish(),
            Self::Binding { key, binding } => formatter
                .debug_struct("Binding")
                .field("key", key)
                .field("binding", binding)
                .finish(),
        }
    }
}

impl<B> ComponentNode<B> {
    pub fn empty() -> Self {
        Self {
            kind: ComponentNodeKind::Empty,
        }
    }

    pub fn pom(document: Document) -> Self {
        Self {
            kind: ComponentNodeKind::Pom(document),
        }
    }

    pub fn fragment(children: Vec<Self>) -> Self {
        Self {
            kind: ComponentNodeKind::Fragment(children),
        }
    }

    pub(crate) fn role(role: PromptRole, child: Self) -> Self {
        Self {
            kind: ComponentNodeKind::Role {
                role,
                child: Box::new(child),
            },
        }
    }

    pub(crate) fn component(
        name: impl Into<StorageString>,
        key: Option<ComponentKey>,
        child: Self,
    ) -> Self {
        Self {
            kind: ComponentNodeKind::Component {
                name: name.into(),
                key,
                child: Box::new(child),
            },
        }
    }

    pub(crate) fn deferred_component(
        name: impl Into<StorageString>,
        render: impl FnOnce() -> super::View<B> + Send + 'static,
    ) -> Self {
        Self {
            kind: ComponentNodeKind::DeferredComponent {
                name: name.into(),
                key: None,
                render: DeferredRender::new(render),
            },
        }
    }

    pub(crate) fn binding(key: BindingKey, binding: B) -> Self {
        Self {
            kind: ComponentNodeKind::Binding { key, binding },
        }
    }

    /// Attach a stable parent-provided key to a component invocation.
    pub fn with_key(self, key: impl Into<StorageString>) -> Result<Self, ComponentError> {
        let key = ComponentKey::new(key)?;
        match self.kind {
            ComponentNodeKind::Component {
                name,
                key: None,
                child,
            } => Ok(Self {
                kind: ComponentNodeKind::Component {
                    name,
                    key: Some(key),
                    child,
                },
            }),
            ComponentNodeKind::Component { key: Some(_), .. } => {
                Err(ComponentError::ComponentAlreadyKeyed)
            }
            ComponentNodeKind::DeferredComponent {
                name,
                key: None,
                render,
            } => Ok(Self {
                kind: ComponentNodeKind::DeferredComponent {
                    name,
                    key: Some(key),
                    render,
                },
            }),
            ComponentNodeKind::DeferredComponent { key: Some(_), .. } => {
                Err(ComponentError::ComponentAlreadyKeyed)
            }
            _ => Err(ComponentError::KeyRequiresComponent),
        }
    }

    /// Map every typed binding in this subtree without changing POM or identity.
    pub fn map_binding<C>(self, map: impl Fn(B) -> C + Send + Sync + 'static) -> ComponentNode<C>
    where
        B: 'static,
        C: 'static,
    {
        self.map_binding_shared(Arc::new(map))
    }

    pub(crate) fn map_binding_shared<C, F>(self, map: Arc<F>) -> ComponentNode<C>
    where
        B: 'static,
        C: 'static,
        F: Fn(B) -> C + Send + Sync + 'static,
    {
        let kind = match self.kind {
            ComponentNodeKind::Empty => ComponentNodeKind::Empty,
            ComponentNodeKind::Pom(document) => ComponentNodeKind::Pom(document),
            ComponentNodeKind::Fragment(children) => ComponentNodeKind::Fragment(
                children
                    .into_iter()
                    .map(|child| child.map_binding_shared(Arc::clone(&map)))
                    .collect(),
            ),
            ComponentNodeKind::Role { role, child } => ComponentNodeKind::Role {
                role,
                child: Box::new(child.map_binding_shared(Arc::clone(&map))),
            },
            ComponentNodeKind::Component { name, key, child } => ComponentNodeKind::Component {
                name,
                key,
                child: Box::new(child.map_binding_shared(Arc::clone(&map))),
            },
            ComponentNodeKind::DeferredComponent { name, key, render } => {
                ComponentNodeKind::DeferredComponent {
                    name,
                    key,
                    render: render.map_binding(Arc::clone(&map)),
                }
            }
            ComponentNodeKind::Binding { key, binding } => ComponentNodeKind::Binding {
                key,
                binding: map(binding),
            },
        };
        ComponentNode { kind }
    }
}
