//! Explicit compatibility surface for the pre-mounted component compiler.
//!
//! New component authors should use [`super::PomView`], [`super::Component`],
//! [`super::system_view`], [`super::user_view`], and
//! [`super::StreamingXmlFactoryReducer::into_component`]. This module intentionally keeps
//! the old two-role compiler and raw carrier types available for migration
//! tests and diagnostics without making them ambient component APIs.

use crate::{pom::Document, StorageString};

use super::{BindingKey, ComponentError, ComponentKey, PomView, PromptRole};

/// Raw fallible component IR used by the compatibility compiler.
pub type ProvidedView<B> = Result<ComponentNode<B>, ComponentError>;

/// Compatibility name for a binding-bearing raw component view.
pub type View<B = ()> = ProvidedView<B>;

/// Convenience operations on a raw component view.
pub trait ViewExt<B>
where
    B: 'static,
{
    fn key(self, key: impl Into<StorageString>) -> View<B>;

    fn map_binding<C>(self, map: impl Fn(B) -> C + Send + Sync + 'static) -> View<C>
    where
        C: 'static;
}

impl<B> ViewExt<B> for View<B>
where
    B: 'static,
{
    fn key(self, key: impl Into<StorageString>) -> View<B> {
        self.and_then(|node| node.with_key(key))
    }

    fn map_binding<C>(self, map: impl Fn(B) -> C + Send + Sync + 'static) -> View<C>
    where
        C: 'static,
    {
        self.map(|node| node.map_binding(map))
    }
}

/// Conversion accepted by raw compatibility composition helpers.
pub trait IntoComponentNode<B> {
    fn into_component_node(self) -> View<B>;
}

impl<B> IntoComponentNode<B> for ComponentNode<B> {
    fn into_component_node(self) -> View<B> {
        Ok(self)
    }
}

impl<B> IntoComponentNode<B> for View<B> {
    fn into_component_node(self) -> View<B> {
        self
    }
}

impl<B> IntoComponentNode<B> for PomView
where
    B: 'static,
{
    fn into_component_node(self) -> View<B> {
        self.into_view()
    }
}

impl<B> IntoComponentNode<B> for Document {
    fn into_component_node(self) -> View<B> {
        Ok(ComponentNode::pom(self))
    }
}

impl<B> IntoComponentNode<B> for () {
    fn into_component_node(self) -> View<B> {
        Ok(ComponentNode::empty())
    }
}

impl<B, T> IntoComponentNode<B> for Option<T>
where
    T: IntoComponentNode<B>,
{
    fn into_component_node(self) -> View<B> {
        self.map(IntoComponentNode::into_component_node)
            .unwrap_or_else(|| Ok(ComponentNode::empty()))
    }
}

impl<B, T> IntoComponentNode<B> for Vec<T>
where
    T: IntoComponentNode<B>,
{
    fn into_component_node(self) -> View<B> {
        self.into_iter()
            .map(IntoComponentNode::into_component_node)
            .collect::<Result<Vec<_>, _>>()
            .map(ComponentNode::fragment)
    }
}

impl<B, T, const N: usize> IntoComponentNode<B> for [T; N]
where
    T: IntoComponentNode<B>,
{
    fn into_component_node(self) -> View<B> {
        self.into_iter()
            .map(IntoComponentNode::into_component_node)
            .collect::<Result<Vec<_>, _>>()
            .map(ComponentNode::fragment)
    }
}

macro_rules! impl_component_tuple {
    ($($name:ident),+ $(,)?) => {
        impl<Binding, $($name),+> IntoComponentNode<Binding> for ($($name,)+)
        where
            $($name: IntoComponentNode<Binding>,)+
        {
            #[allow(non_snake_case)]
            fn into_component_node(self) -> View<Binding> {
                let ($($name,)+) = self;
                Ok(ComponentNode::fragment(vec![
                    $($name.into_component_node()?,)+
                ]))
            }
        }
    };
}

impl_component_tuple!(A);
impl_component_tuple!(A, B);
impl_component_tuple!(A, B, C);
impl_component_tuple!(A, B, C, D);
impl_component_tuple!(A, B, C, D, E);
impl_component_tuple!(A, B, C, D, E, F);
impl_component_tuple!(A, B, C, D, E, F, G);
impl_component_tuple!(A, B, C, D, E, F, G, H);
impl_component_tuple!(A, B, C, D, E, F, G, H, I);
impl_component_tuple!(A, B, C, D, E, F, G, H, I, J);
impl_component_tuple!(A, B, C, D, E, F, G, H, I, J, K);
impl_component_tuple!(A, B, C, D, E, F, G, H, I, J, K, L);

/// Compose values into a raw compatibility fragment.
pub fn view<B>(children: impl IntoComponentNode<B>) -> ProvidedView<B> {
    children.into_component_node()
}

/// Explicit raw compatibility spelling for a binding-bearing fragment.
pub fn provided_view<B>(children: impl IntoComponentNode<B>) -> ProvidedView<B> {
    view(children)
}

/// Place every POM fragment in a raw subtree into the System document.
pub fn system<B>(children: impl IntoComponentNode<B>) -> View<B> {
    children
        .into_component_node()
        .map(|child| ComponentNode::role(PromptRole::System, child))
}

/// Place every POM fragment in a raw subtree into the User document.
pub fn user<B>(children: impl IntoComponentNode<B>) -> View<B> {
    children
        .into_component_node()
        .map(|child| ComponentNode::role(PromptRole::User, child))
}

/// Mount a named raw child component without an explicit key.
pub fn mount<B>(name: impl Into<StorageString>, child: impl IntoComponentNode<B>) -> View<B> {
    child
        .into_component_node()
        .map(|child| ComponentNode::component(name, None, child))
}

/// Mount a named raw child component with stable parent-provided identity.
pub fn keyed<B>(
    name: impl Into<StorageString>,
    key: impl Into<StorageString>,
    child: impl IntoComponentNode<B>,
) -> View<B> {
    let key = ComponentKey::new(key)?;
    child
        .into_component_node()
        .map(|child| ComponentNode::component(name, Some(key), child))
}

/// Declare one typed runtime binding in the raw compatibility IR.
pub fn binding<B>(key: impl Into<StorageString>, value: B) -> View<B> {
    Ok(ComponentNode::binding(BindingKey::new(key)?, value))
}

/// Compatibility-only combined System/User component adapter.
#[doc(hidden)]
pub use super::agent::{
    ComponentAgent, ComponentAgentViewModel, ComponentBinding, ComponentHarness,
    ComponentTurnContext,
};
pub use super::compile::{compile_component, HookPlan, MountedBinding, TurnPlan};
pub use super::node::ComponentNode;
pub use super::provision::{
    compile_mount_provided, BindingFactoryPlan, FactoryDeclaration, MountPlan, MountPlanError,
    MountProvidedView, MountProvidedViewExt, MountedBindingFactory, MountedProviderCapability,
    ProviderCapabilityDeclaration, ProviderCapabilityPlan, RuntimeDeclaration,
};
pub use super::streaming::{
    FinishedStreamingAttempt, MountedStreamingAttempt, PublishedStreamingAttempt,
    PublishedTurnReceipt, StreamingAbortReport, StreamingAttemptError, StreamingAttemptStartError,
    StreamingBinding, StreamingBindingAbort, StreamingChannelView, StreamingChannelsView,
    StreamingChannelsViewExt, StreamingComponentError, StreamingComponentSink,
    StreamingFinishFailure, StreamingOutcome, StreamingProvidedView, StreamingPublishFailure,
    StreamingValueView, StreamingXmlReducer, TurnPublication, TurnPublisher,
};
