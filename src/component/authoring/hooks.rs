//! Native Component hooks for implementations authored without Rust macros.

use std::{fmt, future::Future};

use crate::component::{ApplicationId, EventContext};

use super::{
    render_context::HookRenderContext, ApplicationExitHandle, ComponentDefinitionError, Coroutine,
    CoroutineInbox, ProviderEventSelector, ReactionRequest, Signal,
};

/// Render-scoped access to an instance's native state and lifecycle hooks.
///
/// A [`ComponentDefinition`](super::ComponentDefinition) renderer receives this
/// capability from AgentView. Each `site` identifies a lexical hook in the
/// external implementation. Keep hook order, sites, kinds, and retained Rust
/// types stable while an instance remains compatible. Hook declaration errors
/// reject the whole candidate even if the renderer ignores the returned error.
/// User callbacks and initializers retain their ordinary panic behavior.
///
/// State, preparations, tasks, and reaction requests have the same lifecycle
/// as their macro-authored counterparts. Rendering only declares work. A Signal
/// write marks the view dirty; a reaction request remains an explicit, sticky
/// request to the application driver and does not submit a Frame.
pub struct ComponentHooks<'hook, 'render> {
    context: &'hook mut HookRenderContext<'render>,
}

impl<'hook, 'render> ComponentHooks<'hook, 'render> {
    pub(crate) fn new(context: &'hook mut HookRenderContext<'render>) -> Self {
        Self { context }
    }

    pub(crate) fn finish(self) -> Result<(), ComponentDefinitionError> {
        self.context.definition_result()
    }

    /// Retain one state slot for this instance. Initialization runs only when
    /// the slot first mounts, and never replaces a compatible retained value.
    pub fn use_signal_at<T>(
        &mut self,
        site: u32,
        initialize: impl FnOnce() -> T,
    ) -> Result<Signal<T>, ComponentDefinitionError>
    where
        T: Send + Sync + 'static,
    {
        self.context.try_use_signal_at(site, initialize)
    }

    /// Declare required work before the next explicit preparation/reaction.
    /// The runtime awaits each active slot once per operation, reconciles its
    /// Signal writes, and handles failure and cancellation using native rules.
    pub fn use_preparation_at<Loader, LoaderFuture, Error>(
        &mut self,
        site: u32,
        loader: Loader,
    ) -> Result<(), ComponentDefinitionError>
    where
        Loader: FnOnce() -> LoaderFuture + Send + 'static,
        LoaderFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.context.try_use_preparation_at(site, loader)
    }

    /// Declare the root's native command preparation barrier.
    /// The Application accepts one active root command wait hook.
    pub fn use_wait_for_command_at(&mut self, site: u32) -> Result<(), ComponentDefinitionError> {
        self.context.try_use_wait_for_command_at(site)
    }

    /// Bind a Provider event callback to this render and its actual Frame.
    /// EventContext is runtime provenance, independent of business payload.
    pub fn use_provider_event_handler_at<Event, Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        selector: ProviderEventSelector<Event>,
        handler: Handler,
    ) -> Result<(), ComponentDefinitionError>
    where
        Event: Clone + Send + Sync + 'static,
        Handler: FnMut(EventContext, Event) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.context
            .try_use_provider_event_handler_at(site, selector, handler)
    }

    /// Bind normal reaction completion to this render's callback version.
    /// Interruption does not synthesize a successful completion callback.
    pub fn use_reaction_completion_at<Handler, HandlerFuture, Error>(
        &mut self,
        site: u32,
        handler: Handler,
    ) -> Result<(), ComponentDefinitionError>
    where
        Handler: FnOnce(EventContext) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
        Error: fmt::Display + Send + 'static,
    {
        self.context.try_use_reaction_completion_at(site, handler)
    }

    /// Return the stable identity of this instance's Application owner.
    pub fn use_application_id_at(
        &mut self,
        site: u32,
    ) -> Result<ApplicationId, ComponentDefinitionError> {
        self.context.try_use_application_id_at(site)
    }

    /// Return a mount-scoped request for a later explicitly driven turn.
    /// The request coalesces until consumed, and becomes stale on retirement.
    pub fn use_reaction_request_at(
        &mut self,
        site: u32,
    ) -> Result<ReactionRequest, ComponentDefinitionError> {
        self.context.try_use_reaction_request_at(site)
    }

    /// Return the native mount-scoped normal application exit capability.
    pub fn use_application_exit_at(
        &mut self,
        site: u32,
    ) -> Result<ApplicationExitHandle, ComponentDefinitionError> {
        self.context.try_use_application_exit_at(site)
    }

    /// Declare a future started once after mount commit. A compatible
    /// definition update retains the original task; it does not run the new
    /// factory. Retirement fences, aborts, and joins mount-owned tasks.
    pub fn use_future_at<Factory, TaskFuture>(
        &mut self,
        site: u32,
        factory: Factory,
    ) -> Result<(), ComponentDefinitionError>
    where
        Factory: FnOnce() -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.context.try_use_future_at(site, factory)
    }

    /// Declare a bounded service started once after mount commit. Compatible
    /// updates retain the original service, inbox, message type and capacity.
    /// A zero capacity or retained message-type mismatch rejects the candidate.
    pub fn use_coroutine_at<Message, Factory, TaskFuture>(
        &mut self,
        site: u32,
        capacity: usize,
        service: Factory,
    ) -> Result<Coroutine<Message>, ComponentDefinitionError>
    where
        Message: Send + 'static,
        Factory: FnOnce(CoroutineInbox<Message>) -> TaskFuture + Send + 'static,
        TaskFuture: Future<Output = ()> + Send + 'static,
    {
        self.context.try_use_coroutine_at(site, capacity, service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{
        authoring::{Component, ComponentAttemptFault, ComponentDefinition},
        ComponentHost, ComponentHostFault,
    };

    fn root(definition: ComponentDefinition) -> Component {
        definition.instantiate("root")
    }

    #[test]
    fn ignored_unavailable_capability_errors_still_reject_the_candidate() {
        for capability in [
            "future",
            "coroutine",
            "reaction request",
            "application exit",
        ] {
            let definition = ComponentDefinition::new("capabilities", "v1", move |hooks| {
                let error = match capability {
                    "future" => hooks.use_future_at(0, || async {}).unwrap_err(),
                    "coroutine" => hooks
                        .use_coroutine_at(0, 1, |_: CoroutineInbox<u8>| async {})
                        .unwrap_err(),
                    "reaction request" => match hooks.use_reaction_request_at(0) {
                        Err(error) => error,
                        Ok(_) => panic!("low-level Host has no reaction driver"),
                    },
                    "application exit" => match hooks.use_application_exit_at(0) {
                        Err(error) => error,
                        Ok(_) => panic!("low-level Host has no application exit capability"),
                    },
                    _ => unreachable!(),
                };
                assert!(error.message().contains(capability));
                // A renderer cannot clear the failed declaration by ignoring
                // its result and returning otherwise valid content.
                Ok(Component::text("candidate"))
            })
            .unwrap();
            let mut host = ComponentHost::new_root(root, definition);
            let error = match host.render() {
                Err(error) => error,
                Ok(_) => panic!("an unavailable hook must reject the entire candidate"),
            };
            assert!(matches!(
                error,
                ComponentHostFault::Attempt(ComponentAttemptFault::Definition { .. })
            ));
            assert!(host.current_projection().is_none());
        }
    }

    #[test]
    fn system_definition_hooks_reject_state_before_calling_initializer() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        fn system_root(definition: ComponentDefinition) -> Component {
            definition.instantiate("system").system_once()
        }

        let initializations = Arc::new(AtomicUsize::new(0));
        let initialized = initializations.clone();
        let definition = ComponentDefinition::new("system", "v1", move |hooks| {
            // Application identity is available under System placement, as it
            // is for ordinary macro components.
            let _owner = hooks.use_application_id_at(0)?;
            let initialized = initialized.clone();
            let _state = hooks.use_signal_at(1, move || {
                initialized.fetch_add(1, Ordering::SeqCst);
                0_u8
            })?;
            Ok(Component::text("policy"))
        })
        .unwrap();
        let mut host = ComponentHost::new_root(system_root, definition);
        assert!(matches!(
            host.render(),
            Err(ComponentHostFault::Attempt(
                ComponentAttemptFault::Definition { .. }
            ))
        ));
        assert_eq!(initializations.load(Ordering::SeqCst), 0);
        assert!(host.current_projection().is_none());
    }
}
