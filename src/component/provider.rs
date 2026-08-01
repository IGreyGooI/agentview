//! Executable provider-native tool runtime for one mounted provider attempt.

use std::{collections::HashMap, error::Error, fmt, sync::Arc};

use tokio::sync::Mutex as TokioMutex;

use super::{
    provision::{AttemptProviderCx, ErasedProviderDispatcher},
    streaming::{
        interpret_live_update, LiveEffectDispatcher, SharedCommitBuffer, SharedLiveEffectRuntime,
    },
    BindingAbortReason, BindingId, BindingOrigin, BindingPhase, CommitStager, LiveAbortOutcome,
    LiveEffectFault, LiveEffectRuntime, PreparedSessionMutation, PreparedUserTurn,
    ProviderAttemptIdentity, ProviderCallIdentity, ProviderCapabilityPlan, ProviderDispatchContext,
    ProviderDispatchFailure, ProviderDispatcherAbortAck, ProviderDispatcherAbortContext,
    ProviderPublicationStagingFailure, ProviderPublicationStagingResult, ProviderToolCall,
    ProviderToolResponse, ProviderToolResult, PublicationFingerprintContext,
    PublicationFingerprintFactory, PublicationRequestId, PublicationStagingFailure,
    PublicationStagingPlan, PublishedTurnReceipt, RuntimeRoute, StagedProviderPublication,
    StagedPublication, StreamUpdate, TurnChannels, TurnEmission, TurnPublication, TurnPublisher,
};

/// Phase in which a provider dispatcher terminally failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderDispatchPhase {
    Initialize,
    Dispatch,
    Finish,
    Abort,
}

impl fmt::Display for ProviderDispatchPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Initialize => "initialize",
            Self::Dispatch => "dispatch",
            Self::Finish => "finish",
            Self::Abort => "abort",
        })
    }
}

/// Stable component and attempt identity attached to dispatcher failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDispatchOrigin {
    attempt: ProviderAttemptIdentity,
    capability_id: BindingId,
    tool_name: Option<crate::StorageString>,
    call_identity: Option<ProviderCallIdentity>,
}

impl ProviderDispatchOrigin {
    fn new(
        attempt: &ProviderAttemptIdentity,
        capability_id: &BindingId,
        call: Option<&ProviderToolCall>,
    ) -> Self {
        Self {
            attempt: attempt.clone(),
            capability_id: capability_id.clone(),
            tool_name: call.map(|call| call.name().into()),
            call_identity: call.map(|call| call.identity().clone()),
        }
    }

    pub fn attempt(&self) -> &ProviderAttemptIdentity {
        &self.attempt
    }

    pub fn capability_id(&self) -> &BindingId {
        &self.capability_id
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    pub fn invocation_id(&self) -> Option<&str> {
        self.call_identity
            .as_ref()
            .and_then(ProviderCallIdentity::invocation_id)
    }

    pub fn result_correlation_id(&self) -> Option<&str> {
        self.call_identity
            .as_ref()
            .and_then(ProviderCallIdentity::result_correlation_id)
    }

    pub fn call_identity(&self) -> Option<&ProviderCallIdentity> {
        self.call_identity.as_ref()
    }
}

/// Terminal infrastructure or lifecycle failure from one dispatcher group.
#[derive(Debug, thiserror::Error)]
#[error(
    "provider dispatcher `{capability}` failed during {phase}: {source}",
    capability = .origin.capability_id(),
)]
pub struct ProviderDispatchFault {
    origin: Box<ProviderDispatchOrigin>,
    phase: ProviderDispatchPhase,
    #[source]
    source: ProviderDispatchFailure,
}

impl ProviderDispatchFault {
    fn new(
        identity: &ProviderAttemptIdentity,
        capability_id: &BindingId,
        call: Option<&ProviderToolCall>,
        phase: ProviderDispatchPhase,
        source: ProviderDispatchFailure,
    ) -> Self {
        Self {
            origin: Box::new(ProviderDispatchOrigin::new(identity, capability_id, call)),
            phase,
            source,
        }
    }

    pub fn origin(&self) -> &ProviderDispatchOrigin {
        &self.origin
    }

    pub fn phase(&self) -> ProviderDispatchPhase {
        self.phase
    }

    pub fn source_failure(&self) -> &ProviderDispatchFailure {
        &self.source
    }
}

/// Provider call metadata retained when awaited Live interpretation fails.
#[derive(Debug, thiserror::Error)]
#[error("provider live effect failed: {source}")]
pub struct ProviderLiveEffectFault {
    call_identity: Option<ProviderCallIdentity>,
    #[source]
    source: Box<LiveEffectFault>,
}

impl ProviderLiveEffectFault {
    fn new(call: Option<&ProviderToolCall>, source: LiveEffectFault) -> Self {
        Self {
            call_identity: call.map(|call| call.identity().clone()),
            source: Box::new(source),
        }
    }

    pub fn call_identity(&self) -> Option<&ProviderCallIdentity> {
        self.call_identity.as_ref()
    }

    pub fn source_fault(&self) -> &LiveEffectFault {
        &self.source
    }
}

/// Conflicting calls that reused one provider invocation identity.
#[derive(Debug, thiserror::Error)]
#[error("provider reused invocation id `{invocation_id}` with different correlation or payload")]
pub struct ProviderInvocationCollision {
    invocation_id: crate::StorageString,
    first: ProviderCallIdentity,
    conflicting: ProviderCallIdentity,
}

impl ProviderInvocationCollision {
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    pub fn first(&self) -> &ProviderCallIdentity {
        &self.first
    }

    pub fn conflicting(&self) -> &ProviderCallIdentity {
        &self.conflicting
    }
}

/// Terminal provider-call failure distinct from a model-visible tool error.
#[derive(Debug, thiserror::Error)]
pub enum ProviderToolAttemptError {
    #[error(transparent)]
    Dispatcher(#[from] ProviderDispatchFault),

    #[error(transparent)]
    Live(#[from] ProviderLiveEffectFault),

    #[error(transparent)]
    InvocationIdCollision(Box<ProviderInvocationCollision>),

    #[error("provider attempt is terminal after a prior failure")]
    Terminal,
}

/// One native call result plus already lifecycle-filtered component values.
#[must_use]
pub struct ProviderToolCallOutcome<C>
where
    C: TurnChannels,
{
    result: ProviderToolResult,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    replayed: bool,
}

impl<C> ProviderToolCallOutcome<C>
where
    C: TurnChannels,
{
    pub fn result(&self) -> &ProviderToolResult {
        &self.result
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        std::mem::take(&mut self.update)
    }

    pub fn replayed(&self) -> bool {
        self.replayed
    }

    pub fn into_parts(
        self,
    ) -> (
        ProviderToolResult,
        StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) {
        (self.result, self.update)
    }
}

impl<C> fmt::Debug for ProviderToolCallOutcome<C>
where
    C: TurnChannels,
    TurnEmission<C>: fmt::Debug,
    C::Diagnostic: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderToolCallOutcome")
            .field("result", &self.result)
            .field("update", &self.update)
            .field("replayed", &self.replayed)
            .finish()
    }
}

struct MountedProviderDispatcher<C>
where
    C: TurnChannels,
{
    id: BindingId,
    dispatcher: Box<dyn ErasedProviderDispatcher<C>>,
    completed_calls: u64,
}

impl<C> fmt::Debug for MountedProviderDispatcher<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedProviderDispatcher")
            .field("id", &self.id)
            .field("kind", &self.dispatcher.kind())
            .field("completed_calls", &self.completed_calls)
            .finish()
    }
}

#[derive(Clone)]
struct CompletedProviderCall {
    call: ProviderToolCall,
    result: ProviderToolResult,
}

/// Native-dispatch portion shared by native-only and text-streaming attempts.
pub(crate) struct ProviderDispatchRuntime<C>
where
    C: TurnChannels,
{
    identity: ProviderAttemptIdentity,
    dispatchers: Vec<MountedProviderDispatcher<C>>,
    tool_routes: HashMap<crate::StorageString, usize>,
    completed: HashMap<crate::StorageString, CompletedProviderCall>,
    next_sequence: u64,
    terminal: bool,
}

impl<C> fmt::Debug for ProviderDispatchRuntime<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDispatchRuntime")
            .field("identity", &self.identity)
            .field("dispatchers", &self.dispatchers)
            .field("tool_count", &self.tool_routes.len())
            .field("completed_calls", &self.completed.len())
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl<C> ProviderDispatchRuntime<C>
where
    C: TurnChannels,
{
    pub(crate) fn new<Props>(
        plan: &ProviderCapabilityPlan<C, Props>,
        props: &Props,
        identity: ProviderAttemptIdentity,
    ) -> Result<Self, ProviderDispatchFault>
    where
        Props: ?Sized + 'static,
    {
        let mut dispatchers = Vec::with_capacity(plan.len());
        let mut tool_routes = HashMap::new();
        for capability in plan.capabilities() {
            let dispatcher = capability
                .instantiate_dispatcher(AttemptProviderCx {
                    props,
                    identity: &identity,
                })
                .map_err(|source| {
                    ProviderDispatchFault::new(
                        &identity,
                        capability.id(),
                        None,
                        ProviderDispatchPhase::Initialize,
                        source,
                    )
                })?;
            let index = dispatchers.len();
            for spec in capability.specs() {
                tool_routes.insert(spec.name().into(), index);
            }
            dispatchers.push(MountedProviderDispatcher {
                id: capability.id().clone(),
                dispatcher,
                completed_calls: 0,
            });
        }
        Ok(Self {
            identity,
            dispatchers,
            tool_routes,
            completed: HashMap::new(),
            next_sequence: 0,
            terminal: false,
        })
    }

    pub(crate) fn dispatcher_count(&self) -> usize {
        self.dispatchers.len()
    }

    pub(crate) fn tool_count(&self) -> usize {
        self.tool_routes.len()
    }

    pub(crate) async fn call_tool(
        &mut self,
        call: ProviderToolCall,
        live_runtime: SharedLiveEffectRuntime<C::Live>,
        pending_commits: SharedCommitBuffer<C::Commit>,
    ) -> Result<ProviderToolCallOutcome<C>, ProviderToolAttemptError> {
        if self.terminal {
            return Err(ProviderToolAttemptError::Terminal);
        }
        let Some(invocation_id) = call.invocation_id().map(crate::StorageString::from) else {
            return Ok(self.record_model_visible_result(
                call,
                ProviderToolResponse::error(
                    "invalid_invocation_id",
                    "provider tool invocation id is missing or empty",
                ),
            ));
        };
        if let Some(completed) = self.completed.get(&invocation_id) {
            if completed.call == call {
                return Ok(ProviderToolCallOutcome {
                    result: completed.result.clone(),
                    update: StreamUpdate::new(),
                    replayed: true,
                });
            }
            let collision = ProviderInvocationCollision {
                invocation_id,
                first: completed.call.identity().clone(),
                conflicting: call.identity().clone(),
            };
            self.terminal = true;
            return Err(ProviderToolAttemptError::InvocationIdCollision(Box::new(
                collision,
            )));
        }

        let Some(&dispatcher_index) = self.tool_routes.get(call.name()) else {
            let message = format!("provider tool `{}` is not mounted", call.name());
            return Ok(self.record_model_visible_result(
                call,
                ProviderToolResponse::error("unknown_tool", message),
            ));
        };

        if !call.arguments().is_object() {
            let message = format!(
                "provider tool `{}` requires JSON object arguments",
                call.name()
            );
            return Ok(self.record_model_visible_result(
                call,
                ProviderToolResponse::error("invalid_arguments", message),
            ));
        }

        let context = ProviderDispatchContext::new(
            &self.identity,
            &self.dispatchers[dispatcher_index].id,
            &invocation_id,
            self.next_sequence,
        );
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("provider dispatch sequence space exhausted");
        let dispatch = self.dispatchers[dispatcher_index]
            .dispatcher
            .dispatch(&context, call.clone())
            .await
            .map_err(|source| {
                self.terminal = true;
                ProviderDispatchFault::new(
                    &self.identity,
                    &self.dispatchers[dispatcher_index].id,
                    Some(&call),
                    ProviderDispatchPhase::Dispatch,
                    source,
                )
            })?;
        let (response, update) = dispatch.into_parts();
        self.dispatchers[dispatcher_index].completed_calls = self.dispatchers[dispatcher_index]
            .completed_calls
            .checked_add(1)
            .expect("provider completed-call count exhausted");
        let route = RuntimeRoute::new("provider", call.name())
            .expect("validated provider tool names always form a runtime route");
        let origin = BindingOrigin::new(
            &self.identity,
            &self.dispatchers[dispatcher_index].id,
            &route,
        );
        let update = interpret_live_update(
            update,
            live_runtime,
            pending_commits,
            origin,
            BindingPhase::Dispatch,
        )
        .await
        .map_err(|fault| {
            self.terminal = true;
            ProviderToolAttemptError::Live(ProviderLiveEffectFault::new(Some(&call), fault))
        })?;
        let result = ProviderToolResult::new(&call, response);
        self.completed.insert(
            invocation_id,
            CompletedProviderCall {
                call,
                result: result.clone(),
            },
        );
        Ok(ProviderToolCallOutcome {
            result,
            update,
            replayed: false,
        })
    }

    fn record_model_visible_result(
        &mut self,
        call: ProviderToolCall,
        response: ProviderToolResponse,
    ) -> ProviderToolCallOutcome<C> {
        let result = ProviderToolResult::new(&call, response);
        if let Some(invocation_id) = call.invocation_id().map(crate::StorageString::from) {
            self.completed.insert(
                invocation_id,
                CompletedProviderCall {
                    call,
                    result: result.clone(),
                },
            );
        }
        ProviderToolCallOutcome {
            result,
            update: StreamUpdate::new(),
            replayed: false,
        }
    }

    pub(crate) async fn finish(
        &mut self,
        live_runtime: SharedLiveEffectRuntime<C::Live>,
        pending_commits: SharedCommitBuffer<C::Commit>,
    ) -> Result<StreamUpdate<TurnEmission<C>, C::Diagnostic>, ProviderToolAttemptError> {
        if self.terminal {
            return Err(ProviderToolAttemptError::Terminal);
        }
        let mut combined = StreamUpdate::new();
        for mounted in &mut self.dispatchers {
            let update = mounted.dispatcher.finish().await.map_err(|source| {
                self.terminal = true;
                ProviderDispatchFault::new(
                    &self.identity,
                    &mounted.id,
                    None,
                    ProviderDispatchPhase::Finish,
                    source,
                )
            })?;
            let route = RuntimeRoute::new("provider-group", mounted.id.to_string())
                .expect("mounted capability ids are non-empty");
            let update = interpret_live_update(
                update,
                Arc::clone(&live_runtime),
                Arc::clone(&pending_commits),
                BindingOrigin::new(&self.identity, &mounted.id, &route),
                BindingPhase::Finish,
            )
            .await
            .map_err(|fault| {
                self.terminal = true;
                ProviderToolAttemptError::Live(ProviderLiveEffectFault::new(None, fault))
            })?;
            combined.append(update);
        }
        Ok(combined)
    }

    pub(crate) async fn abort(self, reason: BindingAbortReason) -> Vec<ProviderDispatcherAbort> {
        let mut reports = Vec::with_capacity(self.dispatchers.len());
        for mut mounted in self.dispatchers.into_iter().rev() {
            let context = ProviderDispatcherAbortContext::new(
                &self.identity,
                &mounted.id,
                reason,
                mounted.completed_calls,
            );
            let outcome = match mounted.dispatcher.abort(&context).await {
                Ok(acknowledgement) => {
                    ProviderDispatcherAbortOutcome::Acknowledged(acknowledgement)
                }
                Err(source) => ProviderDispatcherAbortOutcome::Failed(ProviderDispatchFault::new(
                    &self.identity,
                    &mounted.id,
                    None,
                    ProviderDispatchPhase::Abort,
                    source,
                )),
            };
            reports.push(ProviderDispatcherAbort {
                capability_id: mounted.id,
                outcome,
            });
        }
        reports
    }
}

/// Result of explicitly aborting one native dispatcher group.
#[derive(Debug)]
pub enum ProviderDispatcherAbortOutcome {
    Acknowledged(ProviderDispatcherAbortAck),
    Failed(ProviderDispatchFault),
}

/// One dispatcher group's entry in an aggregate abort report.
#[derive(Debug)]
pub struct ProviderDispatcherAbort {
    capability_id: BindingId,
    outcome: ProviderDispatcherAbortOutcome,
}

impl ProviderDispatcherAbort {
    pub fn capability_id(&self) -> &BindingId {
        &self.capability_id
    }

    pub fn outcome(&self) -> &ProviderDispatcherAbortOutcome {
        &self.outcome
    }
}

/// Native-only provider attempt. Text/XML harnesses use the combined streaming
/// attempt, which embeds the same internal provider-dispatch runtime.
#[must_use = "an active provider attempt must be finished or explicitly aborted"]
pub struct MountedProviderAttempt<C>
where
    C: TurnChannels,
{
    identity: ProviderAttemptIdentity,
    runtime: ProviderDispatchRuntime<C>,
    live_runtime: SharedLiveEffectRuntime<C::Live>,
    pending_commits: SharedCommitBuffer<C::Commit>,
    results: Vec<ProviderToolResult>,
    terminal: bool,
}

impl<C> fmt::Debug for MountedProviderAttempt<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedProviderAttempt")
            .field("identity", &self.identity)
            .field("runtime", &self.runtime)
            .field("results", &self.results.len())
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl<C> MountedProviderAttempt<C>
where
    C: TurnChannels,
{
    fn new<Props, R>(
        plan: &ProviderCapabilityPlan<C, Props>,
        props: &Props,
        identity: ProviderAttemptIdentity,
        live_runtime: R,
    ) -> Result<Self, ProviderDispatchFault>
    where
        Props: ?Sized + 'static,
        R: LiveEffectRuntime<C::Live>,
    {
        let runtime = ProviderDispatchRuntime::new(plan, props, identity.clone())?;
        Ok(Self {
            identity,
            runtime,
            live_runtime: Arc::new(TokioMutex::new(LiveEffectDispatcher::new(live_runtime))),
            pending_commits: Arc::new(TokioMutex::new(Vec::new())),
            results: Vec::new(),
            terminal: false,
        })
    }

    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn dispatcher_count(&self) -> usize {
        self.runtime.dispatcher_count()
    }

    pub fn tool_count(&self) -> usize {
        self.runtime.tool_count()
    }

    pub async fn call_tool(
        &mut self,
        call: ProviderToolCall,
    ) -> Result<ProviderToolCallOutcome<C>, ProviderToolAttemptError> {
        if self.terminal {
            return Err(ProviderToolAttemptError::Terminal);
        }
        match self
            .runtime
            .call_tool(
                call,
                Arc::clone(&self.live_runtime),
                Arc::clone(&self.pending_commits),
            )
            .await
        {
            Ok(outcome) => {
                if !outcome.replayed() {
                    self.results.push(outcome.result().clone());
                }
                Ok(outcome)
            }
            Err(error) => {
                self.terminal = true;
                Err(error)
            }
        }
    }

    pub async fn finish(
        mut self,
    ) -> Result<FinishedProviderAttempt<C>, ProviderAttemptFinishFailure<C>> {
        if self.terminal {
            return Err(ProviderAttemptFinishFailure {
                attempt: self,
                error: ProviderToolAttemptError::Terminal,
            });
        }
        let update = match self
            .runtime
            .finish(
                Arc::clone(&self.live_runtime),
                Arc::clone(&self.pending_commits),
            )
            .await
        {
            Ok(update) => update,
            Err(error) => {
                self.terminal = true;
                return Err(ProviderAttemptFinishFailure {
                    attempt: self,
                    error,
                });
            }
        };
        Ok(FinishedProviderAttempt {
            attempt: self,
            update,
        })
    }

    pub async fn abort(self, reason: BindingAbortReason) -> ProviderAttemptAbortReport {
        let live = self
            .live_runtime
            .lock()
            .await
            .abort(self.identity.clone(), reason)
            .await;
        let dispatchers = self.runtime.abort(reason).await;
        ProviderAttemptAbortReport {
            identity: self.identity,
            reason,
            live,
            dispatchers,
        }
    }
}

/// Aggregate acknowledgement for a native-only attempt abort.
#[derive(Debug)]
pub struct ProviderAttemptAbortReport {
    identity: ProviderAttemptIdentity,
    reason: BindingAbortReason,
    live: LiveAbortOutcome,
    dispatchers: Vec<ProviderDispatcherAbort>,
}

impl ProviderAttemptAbortReport {
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        &self.identity
    }

    pub fn reason(&self) -> BindingAbortReason {
        self.reason
    }

    pub fn live(&self) -> &LiveAbortOutcome {
        &self.live
    }

    pub fn dispatchers(&self) -> &[ProviderDispatcherAbort] {
        &self.dispatchers
    }

    /// Whether every fallible host cleanup boundary acknowledged the abort.
    pub fn cleanup_acknowledged(&self) -> bool {
        self.live.acknowledgement().is_some()
            && self.dispatchers.iter().all(|dispatcher| {
                matches!(
                    dispatcher.outcome(),
                    ProviderDispatcherAbortOutcome::Acknowledged(_)
                )
            })
    }
}

/// Native dispatcher state that has finished but has not crossed publication.
#[must_use = "a finished provider attempt must be published or explicitly aborted"]
pub struct FinishedProviderAttempt<C>
where
    C: TurnChannels,
{
    attempt: MountedProviderAttempt<C>,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
}

impl<C> FinishedProviderAttempt<C>
where
    C: TurnChannels,
{
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub fn results(&self) -> &[ProviderToolResult] {
        &self.attempt.results
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        std::mem::take(&mut self.update)
    }

    /// Encode native-dispatch Commit values before the session/outbox
    /// transaction. This uses the same durable contract as the combined
    /// text/XML/native attempt path. The fingerprint factory is called only
    /// after Commit staging and sees the exact ordered outbox that will be
    /// published.
    pub fn stage_durable_publication<S, F, Mutation, Version>(
        self,
        request_id: PublicationRequestId,
        expected_revision: Version,
        mutation: PreparedSessionMutation<Mutation>,
        stager: &S,
        fingerprint_factory: &F,
    ) -> ProviderPublicationStagingResult<C, Mutation, S::Payload, Version, S::Error, F::Error>
    where
        S: CommitStager<C>,
        F: PublicationFingerprintFactory<Mutation, S::Payload, Version>,
    {
        let plan = PublicationStagingPlan::new(request_id, expected_revision, mutation);
        let outbox = {
            let commits = self
                .attempt
                .pending_commits
                .try_lock()
                .expect("a finished provider attempt has no active Commit producer");
            super::stage_commit_outbox::<C, S>(plan.request_id().clone(), &commits, stager)
        };
        let outbox = match outbox {
            Ok(outbox) => outbox,
            Err(source) => {
                return Err(ProviderPublicationStagingFailure::new(
                    self,
                    plan,
                    PublicationStagingFailure::Commit(source),
                ));
            }
        };
        let fingerprint = fingerprint_factory.fingerprint(PublicationFingerprintContext::new(
            plan.request_id(),
            plan.expected_revision(),
            plan.mutation(),
            "",
            self.results(),
            &outbox,
        ));
        let fingerprint = match fingerprint {
            Ok(fingerprint) => fingerprint,
            Err(source) => {
                return Err(ProviderPublicationStagingFailure::new(
                    self,
                    plan,
                    PublicationStagingFailure::Fingerprint(source),
                ));
            }
        };
        let (request_id, expected_revision, mutation) = plan.into_parts();
        let candidate = StagedPublication::new(
            request_id,
            fingerprint,
            expected_revision,
            mutation,
            self.identity().clone(),
            "",
            self.results().to_vec(),
            outbox,
        )
        .expect("outbox was staged with the same publication request id");
        Ok(StagedProviderPublication::new(self, candidate))
    }

    pub async fn publish_with<P>(
        self,
        publisher: &mut P,
    ) -> Result<PublishedProviderAttempt<C>, ProviderPublishFailure<C, P::Error>>
    where
        P: TurnPublisher,
    {
        let publication = TurnPublication::provider(self.identity(), self.results());
        if let Err(source) = publisher.publish(publication).await {
            return Err(ProviderPublishFailure {
                attempt: self,
                source,
            });
        }
        let Self { attempt, update } = self;
        let commits = {
            let mut pending = attempt.pending_commits.lock().await;
            std::mem::take(&mut *pending)
        };
        Ok(PublishedProviderAttempt {
            receipt: PublishedTurnReceipt::new(attempt.identity),
            results: attempt.results,
            update,
            commits,
        })
    }

    pub async fn abort(self, reason: BindingAbortReason) -> ProviderAttemptAbortReport {
        self.attempt.abort(reason).await
    }

    pub(crate) async fn into_durable_parts(
        self,
    ) -> (
        Vec<ProviderToolResult>,
        StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    ) {
        let Self {
            mut attempt,
            update,
        } = self;
        let commits = {
            let mut pending = attempt.pending_commits.lock().await;
            std::mem::take(&mut *pending)
        };
        drop(commits);
        let results = std::mem::take(&mut attempt.results);
        (results, update)
    }
}

/// Successfully published native provider attempt.
#[must_use = "published commit values must be delivered or deliberately discarded"]
pub struct PublishedProviderAttempt<C>
where
    C: TurnChannels,
{
    receipt: PublishedTurnReceipt,
    results: Vec<ProviderToolResult>,
    update: StreamUpdate<TurnEmission<C>, C::Diagnostic>,
    commits: Vec<C::Commit>,
}

impl<C> PublishedProviderAttempt<C>
where
    C: TurnChannels,
{
    pub fn receipt(&self) -> &PublishedTurnReceipt {
        &self.receipt
    }

    pub fn results(&self) -> &[ProviderToolResult] {
        &self.results
    }

    pub fn update(&self) -> &StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        &self.update
    }

    pub fn take_update(&mut self) -> StreamUpdate<TurnEmission<C>, C::Diagnostic> {
        std::mem::take(&mut self.update)
    }

    pub fn pending_commits(&self) -> &[C::Commit] {
        &self.commits
    }

    pub fn take_pending_commits(&mut self) -> Vec<C::Commit> {
        std::mem::take(&mut self.commits)
    }
}

/// Recoverable finish failure retaining the active native attempt.
#[must_use = "a finish failure must explicitly abort the retained attempt"]
pub struct ProviderAttemptFinishFailure<C>
where
    C: TurnChannels,
{
    attempt: MountedProviderAttempt<C>,
    error: ProviderToolAttemptError,
}

impl<C> ProviderAttemptFinishFailure<C>
where
    C: TurnChannels,
{
    pub fn error(&self) -> &ProviderToolAttemptError {
        &self.error
    }

    pub async fn abort(self, reason: BindingAbortReason) -> ProviderAttemptAbortReport {
        self.attempt.abort(reason).await
    }
}

impl<C> fmt::Debug for ProviderAttemptFinishFailure<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAttemptFinishFailure")
            .field("identity", self.attempt.identity())
            .field("error", &self.error)
            .finish()
    }
}

/// Publication failure retaining the finished native attempt for abort.
#[must_use = "a failed publication must explicitly abort the retained attempt"]
pub struct ProviderPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    attempt: FinishedProviderAttempt<C>,
    source: E,
}

impl<C, E> ProviderPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    pub fn identity(&self) -> &ProviderAttemptIdentity {
        self.attempt.identity()
    }

    pub fn results(&self) -> &[ProviderToolResult] {
        self.attempt.results()
    }

    pub fn source_error(&self) -> &E {
        &self.source
    }

    pub async fn abort(self, reason: BindingAbortReason) -> ProviderAttemptAbortReport {
        self.attempt.abort(reason).await
    }
}

impl<C, E> fmt::Debug for ProviderPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderPublishFailure")
            .field("identity", self.attempt.identity())
            .field("source", &self.source)
            .finish()
    }
}

impl<C, E> fmt::Display for ProviderPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "publication failed for provider attempt `{}`: {}",
            self.attempt.identity().provider_attempt_id(),
            self.source
        )
    }
}

impl<C, E> Error for ProviderPublishFailure<C, E>
where
    C: TurnChannels,
    E: Error + Send + Sync + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl<C, Props> PreparedUserTurn<'_, '_, C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    /// Start a native-only attempt from the exact props used to render User.
    ///
    /// This entry point rejects mounted event bindings. A text harness that
    /// combines XML and provider-native tools must use `start_streaming_attempt`,
    /// which installs both runtimes under one attempt identity.
    pub fn start_provider_attempt<R>(
        &self,
        live_runtime: R,
    ) -> Result<MountedProviderAttempt<C>, ProviderAttemptStartError>
    where
        R: LiveEffectRuntime<C::Live>,
    {
        if !self.epoch().binding_factories().is_empty() {
            return Err(
                ProviderAttemptStartError::EventBindingsRequireCombinedAttempt {
                    count: self.epoch().binding_factories().len(),
                },
            );
        }
        MountedProviderAttempt::new(
            self.epoch().provider_capabilities(),
            self.props(),
            self.next_attempt_identity(),
            live_runtime,
        )
        .map_err(ProviderAttemptStartError::Dispatcher)
    }
}

/// Failure before a native-only provider attempt becomes active.
#[derive(Debug, thiserror::Error)]
pub enum ProviderAttemptStartError {
    #[error(transparent)]
    Dispatcher(ProviderDispatchFault),

    #[error(
        "native-only attempt cannot ignore {count} mounted event binding(s); use the combined attempt entry point"
    )]
    EventBindingsRequireCombinedAttempt { count: usize },
}
