//! Target Component declaration, render staging, and local event bindings.
//!
//! This module does not convert to the legacy generic component compiler. Its
//! private declaration tree is the permanent lowering target for `view!`.

pub(crate) mod application_exit;
mod async_task;
mod attempt;
mod capture;
mod declaration;
mod event_input;
mod event_listener;
mod handler;
mod native_tool;
mod preparation;
mod provider_event_handler;
mod reaction_completion;
mod reaction_request;
mod render_context;
mod signal;
pub mod streaming_attempt;
mod streaming_xml;

pub use application_exit::{
    use_application_exit, ApplicationExitError, ApplicationExitHandle, ExitReason,
};
pub(crate) use async_task::MountTaskStart;
pub use async_task::{
    spawn, use_coroutine, use_future, Coroutine, CoroutineInbox, CoroutineSendError, SpawnError,
};
pub use attempt::ComponentAttemptFault;
pub(crate) use attempt::{ComponentRenderStage, RenderBindings};
pub use capture::ComponentCaptureError;
pub use declaration::Component;
pub(crate) use event_input::EventInput as InternalEventInput;
pub use preparation::use_preparation;
pub(crate) use preparation::{PreparationFault, PreparationRun, PreparationSet};
#[cfg(feature = "legacy-provider-port")]
#[deprecated(note = "use `use_provider_event_handler` for provider event routing")]
pub type EventInput<E> = InternalEventInput<E>;
#[cfg(test)]
pub(crate) use event_listener::EventListener as InternalEventListener;
#[cfg(feature = "legacy-provider-port")]
#[deprecated(note = "use `use_provider_event_handler` for provider event routing")]
pub type EventListener = event_listener::EventListener;
pub use native_tool::NativeToolCall;
pub use provider_event_handler::{use_provider_event_handler, ProviderEventSelector};
pub use reaction_completion::use_reaction_completion;
pub(crate) use reaction_completion::ReactionCompletionDeclaration;
pub use reaction_request::{use_reaction_request, ReactionRequest, ReactionRequestError};
pub use signal::{use_signal, Signal};
pub use streaming_attempt::*;
pub use streaming_xml::{
    StreamingXml, StreamingXmlTag, XmlContractDiagnostic, XmlStreamingToolCall,
};

/// Macro expansion helpers. They are public only because proc-macro output is
/// type-checked in the consuming crate.
#[doc(hidden)]
pub mod __private {
    use super::declaration::{
        ComponentNode, MarkdownInlineTemplate, PomFragment, TextTemplateSlot, XmlTemplate,
        XmlTemplateChild,
    };
    pub use super::declaration::{
        IntoDiffViewRoot, IntoViewRoot, MacroPlacement as Placement, TextTemplate, TextTemplatePart,
    };
    pub use super::event_input::{EventSelector, COMPONENT_EVENTS_ABI_V1};
    pub use super::render_context::HookRenderContext;
    use super::Component;

    pub(crate) const SYSTEM_ONCE_BOUNDARY_PREFIX: &str = "agentview::system_once:";

    pub(crate) fn is_system_once_boundary(function: &str) -> bool {
        function.starts_with(SYSTEM_ONCE_BOUNDARY_PREFIX)
    }

    pub fn fragment(children: Vec<Component>) -> Component {
        Component::from_node(ComponentNode::Fragment(children))
    }

    pub fn text_template(parts: Vec<TextTemplatePart>) -> TextTemplate {
        TextTemplate::new(parts)
    }

    pub fn static_text(value: &'static str) -> TextTemplatePart {
        TextTemplatePart::Static(value)
    }

    pub fn formatted_text_slot<T: ?Sized>(
        value: &T,
        arguments: std::fmt::Arguments<'_>,
    ) -> TextTemplatePart {
        TextTemplatePart::Slot(TextTemplateSlot::new(
            std::any::type_name_of_val(value),
            std::fmt::format(arguments),
        ))
    }

    pub fn paragraph(text: TextTemplate) -> Component {
        Component::from_node(ComponentNode::Pom(PomFragment::Paragraph(text)))
    }

    pub fn markdown_paragraph(children: Vec<MarkdownInlineTemplate>) -> Component {
        Component::from_node(ComponentNode::Pom(PomFragment::MarkdownParagraph(children)))
    }

    pub fn markdown_text(text: TextTemplate) -> MarkdownInlineTemplate {
        MarkdownInlineTemplate::Text(text)
    }

    pub fn markdown_code_span(text: TextTemplate) -> MarkdownInlineTemplate {
        MarkdownInlineTemplate::CodeSpan(text)
    }

    pub fn dynamic_root(value: impl IntoViewRoot) -> Component {
        value.into_view_root()
    }

    pub fn dynamic_diff_root(value: impl IntoDiffViewRoot) -> Component {
        value.into_diff_view_root()
    }

    pub fn xml_component(
        name: &'static str,
        attributes: Vec<(&'static str, TextTemplate)>,
        children: Vec<XmlTemplateChild>,
    ) -> Component {
        Component::from_node(ComponentNode::Pom(PomFragment::Xml(XmlTemplate::new(
            name, attributes, children,
        ))))
    }

    pub fn xml_child(
        name: &'static str,
        attributes: Vec<(&'static str, TextTemplate)>,
        children: Vec<XmlTemplateChild>,
    ) -> XmlTemplateChild {
        XmlTemplateChild::Element(XmlTemplate::new(name, attributes, children))
    }

    pub fn xml_text(text: TextTemplate) -> XmlTemplateChild {
        XmlTemplateChild::Text(text)
    }

    pub fn placed(placement: Placement, child: Component) -> Component {
        Component::from_node(ComponentNode::Placement {
            placement: placement.into(),
            child: Box::new(child),
        })
    }

    pub fn system_once(
        function: &'static str,
        render: impl FnOnce() -> Component + Send + 'static,
    ) -> Component {
        placed(Placement::SystemOnce, defer_component(function, render))
    }

    pub fn diff(slot: &'static str, child: Component) -> Component {
        Component::from_node(ComponentNode::Diff {
            slot,
            child: Box::new(child),
        })
    }

    pub fn defer_component(
        function: &'static str,
        render: impl FnOnce() -> Component + Send + 'static,
    ) -> Component {
        Component::from_node(ComponentNode::Scope {
            function,
            render: super::declaration::ScopeRender::Fresh(Box::new(render)),
        })
    }

    pub fn defer_repeatable_component(
        function: &'static str,
        render: impl for<'render> Fn(&mut HookRenderContext<'render>) -> Component + Send + 'static,
    ) -> Component {
        Component::from_node(ComponentNode::Scope {
            function,
            render: super::declaration::ScopeRender::Repeatable(Box::new(render)),
        })
    }

    #[allow(clippy::clone_on_copy)]
    pub fn clone_repeatable_input<T>(value: &T) -> T
    where
        T: Clone,
    {
        value.clone()
    }
}
