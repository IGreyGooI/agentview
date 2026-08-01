use std::collections::HashSet;

use crate::pom::{BlockChildren, Document};

use super::node::ComponentNodeKind;
use super::{
    BindingId, BindingKey, ComponentError, ComponentId, ComponentKey, ComponentNode, PromptRole,
};

/// One typed runtime binding annotated with its stable component-tree identity.
#[derive(Debug)]
pub struct MountedBinding<B> {
    id: BindingId,
    role: Option<PromptRole>,
    binding: B,
}

impl<B> MountedBinding<B> {
    pub fn id(&self) -> &BindingId {
        &self.id
    }

    pub fn binding(&self) -> &B {
        &self.binding
    }

    /// Prompt placement surrounding this runtime declaration.
    ///
    /// Legacy compilation records this value without enforcing it. The mount
    /// compiler uses it to reject declarations outside the System subtree.
    pub fn role(&self) -> Option<PromptRole> {
        self.role
    }

    pub fn into_binding(self) -> B {
        self.binding
    }

    pub fn into_parts(self) -> (BindingId, B) {
        (self.id, self.binding)
    }

    pub fn into_placed_parts(self) -> (BindingId, Option<PromptRole>, B) {
        (self.id, self.role, self.binding)
    }
}

/// Internal runtime declarations collected from the component tree.
#[derive(Debug, Default)]
pub struct HookPlan<B> {
    bindings: Vec<MountedBinding<B>>,
}

impl<B> HookPlan<B> {
    pub fn bindings(&self) -> &[MountedBinding<B>] {
        &self.bindings
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn into_bindings(self) -> Vec<MountedBinding<B>> {
        self.bindings
    }
}

/// Complete output of one pure component-tree compilation.
#[derive(Debug)]
pub struct TurnPlan<B> {
    system: Document,
    user: Document,
    hooks: HookPlan<B>,
}

impl<B> TurnPlan<B> {
    pub fn system_document(&self) -> &Document {
        &self.system
    }

    pub fn user_document(&self) -> &Document {
        &self.user
    }

    pub fn hooks(&self) -> &HookPlan<B> {
        &self.hooks
    }

    pub fn into_parts(self) -> (Document, Document, HookPlan<B>) {
        (self.system, self.user, self.hooks)
    }
}

struct ComponentScope {
    id: ComponentId,
    next_child_position: usize,
    child_keys: HashSet<ComponentKey>,
    binding_keys: HashSet<BindingKey>,
}

impl ComponentScope {
    fn new(id: ComponentId) -> Self {
        Self {
            id,
            next_child_position: 0,
            child_keys: HashSet::new(),
            binding_keys: HashSet::new(),
        }
    }

    fn claim_child(
        &mut self,
        name: &str,
        key: Option<&ComponentKey>,
    ) -> Result<ComponentId, ComponentError> {
        if let Some(key) = key {
            if !self.child_keys.insert(key.clone()) {
                return Err(ComponentError::DuplicateComponentKey {
                    parent: self.id.clone(),
                    key: key.clone(),
                });
            }
        }
        let position = self.next_child_position;
        self.next_child_position += 1;
        Ok(self.id.child(name, key, position))
    }

    fn claim_binding(&mut self, key: BindingKey) -> Result<BindingId, ComponentError> {
        if !self.binding_keys.insert(key.clone()) {
            return Err(ComponentError::DuplicateBindingKey {
                component: self.id.clone(),
                key,
            });
        }
        Ok(BindingId::new(self.id.clone(), key))
    }
}

struct Compiler<B> {
    system: BlockChildren,
    user: BlockChildren,
    bindings: Vec<MountedBinding<B>>,
}

impl<B> Compiler<B> {
    fn new() -> Self {
        Self {
            system: BlockChildren::new(),
            user: BlockChildren::new(),
            bindings: Vec::new(),
        }
    }

    fn compile_node(
        &mut self,
        node: ComponentNode<B>,
        role: Option<PromptRole>,
        scope: &mut ComponentScope,
    ) -> Result<(), ComponentError> {
        match node.kind {
            ComponentNodeKind::Empty => Ok(()),
            ComponentNodeKind::Pom(document) => {
                let role = role.ok_or_else(|| ComponentError::UnplacedPom {
                    component: scope.id.clone(),
                })?;
                match role {
                    PromptRole::System => self.system.extend(document.into_children()),
                    PromptRole::User => self.user.extend(document.into_children()),
                }
                Ok(())
            }
            ComponentNodeKind::Fragment(children) => {
                for child in children {
                    self.compile_node(child, role, scope)?;
                }
                Ok(())
            }
            ComponentNodeKind::Role {
                role: nested_role,
                child,
            } => {
                if let Some(outer_role) = role {
                    return Err(ComponentError::NestedRole {
                        component: scope.id.clone(),
                        outer: outer_role,
                        inner: nested_role,
                    });
                }
                self.compile_node(*child, Some(nested_role), scope)
            }
            ComponentNodeKind::Component { name, key, child } => {
                let id = scope.claim_child(&name, key.as_ref())?;
                let mut child_scope = ComponentScope::new(id);
                self.compile_node(*child, role, &mut child_scope)
            }
            ComponentNodeKind::DeferredComponent { name, key, render } => {
                let id = scope.claim_child(&name, key.as_ref())?;
                let mut child_scope = ComponentScope::new(id);
                let child = render.execute()?;
                self.compile_node(child, role, &mut child_scope)
            }
            ComponentNodeKind::Binding { key, binding } => {
                let id = scope.claim_binding(key)?;
                self.bindings.push(MountedBinding { id, role, binding });
                Ok(())
            }
        }
    }

    fn finish(self) -> TurnPlan<B> {
        TurnPlan {
            system: Document::new(self.system),
            user: Document::new(self.user),
            hooks: HookPlan {
                bindings: self.bindings,
            },
        }
    }
}

/// Compile a short-lived component tree into role documents and runtime bindings.
pub fn compile_component<B>(
    view: impl super::experimental::IntoComponentNode<B>,
) -> Result<TurnPlan<B>, ComponentError> {
    let node = view.into_component_node()?;
    let mut compiler = Compiler::new();
    let mut root = ComponentScope::new(ComponentId::root());
    compiler.compile_node(node, None, &mut root)?;
    Ok(compiler.finish())
}
