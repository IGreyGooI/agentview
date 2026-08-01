//! Isolated mounted-System and per-attempt User lifecycle boundary.
//!
//! This module deliberately stops before `AgentLoop` integration. One call to
//! [`mount_system_epoch`] executes a System root once and returns its complete
//! bundle; each call to [`MountedEpoch::prepare_user`] can only render a
//! POM-only User view. The final runtime owner must still enforce how those
//! calls correspond to logical epochs and preparation attempts.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    pom::{Document, ResolvedDocument},
    pom_renderer::{render_pom_document, PomRenderError},
    pom_resolution::resolve_system_document,
};

use super::{
    compile_component, compile_durable_mount_provided, compile_mount_provided, component,
    durable_epoch::{
        DurableEpochRuntime, DurableRuntimeContractError, RenderedEpochArtifact,
        RuntimeRebindProjection, RuntimeRebindRequest,
    },
    erasure::ErasedMountedEpoch,
    system, user, BindingFactoryPlan, ChannelTypeInfo, Component, ComponentError, EpochContractId,
    HarnessEpochId, MountPlanError, MountedProviderEpoch, NoBindings, PomChildren,
    ProviderAttemptIdentity, ProviderCapabilityPlan, ProviderToolCatalog, RuntimeBindingRegistry,
    TurnChannels, TurnInstanceId, View,
};

/// Once-per-epoch System root containing POM and reusable declarations.
pub struct SystemView<C, TurnProps: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    view: Component<C, TurnProps>,
}

impl<C, TurnProps> SystemView<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    fn into_view(self) -> Component<C, TurnProps> {
        self.view
    }
}

/// Per-preparation User root containing POM only.
pub struct UserView {
    view: View<NoBindings>,
}

impl UserView {
    fn into_view(self) -> View<NoBindings> {
        self.view
    }
}

/// Construct the only System root accepted by [`mount_system_epoch`].
///
/// Requiring the nominal [`Component`] carrier keeps raw runtime declarations
/// out of the lifecycle boundary. POM-only policy is still valid: wrap it with
/// [`component`] before constructing the System root.
pub fn system_view<C, TurnProps>(root: Component<C, TurnProps>) -> SystemView<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    SystemView {
        view: component(system(root)),
    }
}

/// Construct a POM-only User root for one preparation attempt.
pub fn user_view(children: impl PomChildren) -> UserView {
    UserView {
        view: user(children.into_pom_view()),
    }
}

/// Lifecycle-provided inputs visible while constructing one System candidate.
///
/// Mount props are the only input supplied by this API. Rust cannot prevent a
/// named render function from reading globals or an application from putting
/// turn-shaped data inside its mount-props type; component authors must keep
/// render functions functional and side-effect free.
pub struct SystemMountContext<'a, Props: ?Sized> {
    props: &'a Props,
}

impl<'a, Props: ?Sized> SystemMountContext<'a, Props> {
    pub fn props(&self) -> &'a Props {
        self.props
    }
}

impl<Props: ?Sized> Clone for SystemMountContext<'_, Props> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Props: ?Sized> Copy for SystemMountContext<'_, Props> {}

impl<Props: ?Sized> std::fmt::Debug for SystemMountContext<'_, Props> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemMountContext")
            .finish_non_exhaustive()
    }
}

/// Immutable application-defined inputs visible to one User render.
///
/// `Props` is the complete User-authoring input chosen by the application. The
/// mounted component framework does not inject or reorder context, artifacts,
/// task text, call labels, or captured views. A future AgentLoop owner assembles
/// the application's `Props` value and the User component decides which fields
/// become POM and in what order.
pub struct UserTurnContext<'a, Props: ?Sized> {
    props: &'a Props,
}

impl<'a, Props: ?Sized> UserTurnContext<'a, Props> {
    /// Construct one pure User-render context from application-owned props.
    ///
    /// The framework deliberately does not synthesize context, artifacts,
    /// task text, or call metadata here. A mounted owner supplies the exact
    /// snapshot selected by its capture phase; this constructor also lets an
    /// author exercise a User renderer without mounting a provider epoch.
    pub fn new(props: &'a Props) -> Self {
        Self { props }
    }

    pub fn props(&self) -> &'a Props {
        self.props
    }
}

impl<Props: ?Sized> Clone for UserTurnContext<'_, Props> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Props: ?Sized> Copy for UserTurnContext<'_, Props> {}

impl<Props: ?Sized> std::fmt::Debug for UserTurnContext<'_, Props> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UserTurnContext")
            .finish_non_exhaustive()
    }
}

/// Failure while compiling, validating, resolving, or rendering a System epoch.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SystemMountError {
    #[error(transparent)]
    Plan(#[from] MountPlanError),

    #[error(transparent)]
    Render(#[from] PomRenderError),
}

/// Atomic once-rendered System bundle for one harness epoch.
pub struct MountedEpoch<C, TurnProps: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    inner: Arc<ErasedMountedEpoch>,
    _contract: PhantomData<fn(&TurnProps) -> C>,
}

impl<C, TurnProps> RuntimeBindingRegistry<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    pub(crate) fn into_mounted_epoch(
        self,
        artifact: &RenderedEpochArtifact,
    ) -> Result<MountedEpoch<C, TurnProps>, ReboundEpochError> {
        artifact
            .manifest()
            .validate_runtime_plans(self.binding_factories(), self.provider_capabilities())?;
        let system = artifact
            .system_document()
            .cloned()
            .ok_or(ReboundEpochError::MissingSystemDocuments)?;
        let resolved_system = artifact
            .resolved_system_document()
            .cloned()
            .ok_or(ReboundEpochError::MissingSystemDocuments)?;
        let (binding_factories, provider_capabilities) = self.into_plans();
        Ok(MountedEpoch {
            inner: Arc::new(ErasedMountedEpoch::new::<C, TurnProps, _>(
                MountedEpochState {
                    id: HarnessEpochId::fresh(),
                    epoch_contract_id: Some(artifact.manifest().epoch_contract_id().clone()),
                    system,
                    resolved_system,
                    rendered_system: Arc::from(artifact.rendered_system()),
                    binding_factories: Arc::new(binding_factories),
                    provider_capabilities: Arc::new(provider_capabilities),
                },
            )),
            _contract: PhantomData,
        })
    }
}

impl<C, TurnProps> DurableEpochRuntime for RuntimeBindingRegistry<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    type Error = DurableRuntimeContractError;

    fn validate_against_manifest(
        &self,
        manifest: &super::durable_epoch::EpochContractManifest,
    ) -> Result<(), Self::Error> {
        manifest.validate_runtime_plans(self.binding_factories(), self.provider_capabilities())
    }
}

impl<C, TurnProps> RuntimeRebindProjection for RuntimeBindingRegistry<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    type Runtime = Self;
    type Error = DurableRuntimeContractError;

    fn rebind(&self, request: RuntimeRebindRequest<'_>) -> Result<Self::Runtime, Self::Error> {
        request
            .manifest()
            .validate_runtime_plans(self.binding_factories(), self.provider_capabilities())?;
        Ok(self.clone())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReboundEpochError {
    #[error("durable mounted epoch artifact does not retain its System POM documents")]
    MissingSystemDocuments,

    #[error(transparent)]
    RuntimeContract(#[from] DurableRuntimeContractError),
}

impl<C, TurnProps> Clone for MountedEpoch<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _contract: PhantomData,
        }
    }
}

struct MountedEpochState<C, TurnProps: ?Sized + 'static>
where
    C: TurnChannels,
{
    id: HarnessEpochId,
    epoch_contract_id: Option<EpochContractId>,
    system: Document,
    resolved_system: ResolvedDocument,
    rendered_system: Arc<str>,
    binding_factories: Arc<BindingFactoryPlan<C, TurnProps>>,
    provider_capabilities: Arc<ProviderCapabilityPlan<C, TurnProps>>,
}

impl<C, TurnProps> MountedEpoch<C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    fn state(&self) -> &MountedEpochState<C, TurnProps> {
        debug_assert!(self.inner.channel_types().ensure::<C, TurnProps>().is_ok());
        self.inner.state::<MountedEpochState<C, TurnProps>>()
    }

    /// Process-local descriptor retained by the private erased epoch adapter.
    pub fn channel_type_info(&self) -> &ChannelTypeInfo {
        self.inner.channel_types()
    }

    pub fn id(&self) -> HarnessEpochId {
        self.state().id
    }

    /// Stable durable contract identity when this epoch was mounted for an
    /// authoritative agent owner. Isolated lifecycle mounts may omit it.
    pub fn epoch_contract_id(&self) -> Option<&EpochContractId> {
        self.state().epoch_contract_id.as_ref()
    }

    pub fn system_document(&self) -> &Document {
        &self.state().system
    }

    pub fn resolved_system_document(&self) -> &ResolvedDocument {
        &self.state().resolved_system
    }

    pub fn rendered_system(&self) -> &str {
        &self.state().rendered_system
    }

    pub fn binding_factories(&self) -> &BindingFactoryPlan<C, TurnProps> {
        &self.state().binding_factories
    }

    pub fn provider_capabilities(&self) -> &ProviderCapabilityPlan<C, TurnProps> {
        &self.state().provider_capabilities
    }

    /// Immutable provider-facing schema snapshot for this mounted System epoch.
    /// Dispatcher factories and turn props remain private to the runtime.
    pub fn provider_tool_catalog(&self) -> ProviderToolCatalog {
        ProviderToolCatalog::from_capabilities(self.id(), self.provider_capabilities())
    }

    /// Construct the owner-only provider attachment for this System epoch.
    ///
    /// Keeping this inside the crate prevents the public isolated lifecycle
    /// API from turning one epoch into arbitrary repeated System attachments.
    /// The mounted owner is the sole authority that opens or replaces a
    /// provider epoch.
    pub(crate) fn provider_epoch(&self) -> MountedProviderEpoch {
        MountedProviderEpoch::new(
            Arc::clone(&self.state().rendered_system),
            self.provider_tool_catalog(),
        )
    }

    pub(crate) fn runtime_registry(&self) -> RuntimeBindingRegistry<C, TurnProps> {
        RuntimeBindingRegistry::from_compiled_plans(
            self.state().binding_factories.as_ref().clone(),
            self.state().provider_capabilities.as_ref().clone(),
        )
    }

    /// Render one independent User candidate without traversing System.
    pub fn prepare_user(
        &self,
        props: &TurnProps,
        render: impl FnOnce(UserTurnContext<'_, TurnProps>) -> UserView,
    ) -> Result<UserTurnPlan, ComponentError> {
        let view = render(UserTurnContext::new(props));
        compile_user_view(view)
    }

    /// Create one logical turn identity without rendering or instantiating it.
    pub fn begin_turn(
        &self,
        call_label: impl Into<crate::StorageString>,
    ) -> MountedTurn<'_, C, TurnProps> {
        MountedTurn {
            epoch: self,
            id: TurnInstanceId::fresh(),
            call_label: call_label.into(),
        }
    }
}

/// Host-owned logical turn that can prepare multiple candidates and attempts.
pub struct MountedTurn<'epoch, C, TurnProps: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    epoch: &'epoch MountedEpoch<C, TurnProps>,
    id: TurnInstanceId,
    call_label: crate::StorageString,
}

impl<'epoch, C, TurnProps> MountedTurn<'epoch, C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    pub fn id(&self) -> TurnInstanceId {
        self.id
    }

    pub fn call_label(&self) -> &str {
        &self.call_label
    }

    pub fn prepare_user<'props>(
        &self,
        props: &'props TurnProps,
        render: impl FnOnce(UserTurnContext<'_, TurnProps>) -> UserView,
    ) -> Result<PreparedUserTurn<'epoch, 'props, C, TurnProps>, ComponentError> {
        let user = self.epoch.prepare_user(props, render)?;
        Ok(PreparedUserTurn {
            epoch: self.epoch,
            turn_instance_id: self.id,
            call_label: self.call_label.clone(),
            props,
            user,
        })
    }
}

impl<C, TurnProps> std::fmt::Debug for MountedTurn<'_, C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MountedTurn")
            .field("epoch_id", &self.epoch.id())
            .field("id", &self.id)
            .field("call_label", &self.call_label)
            .finish()
    }
}

/// POM-only output of one User preparation attempt.
#[derive(Debug)]
pub struct UserTurnPlan {
    user: Document,
}

/// Successfully rendered User candidate tied to the exact props that future
/// provider-attempt factories will receive.
///
/// Context preparation may create and discard multiple candidates. Only the
/// final Ready candidate should be used to start a provider attempt.
pub struct PreparedUserTurn<'epoch, 'props, C, TurnProps: ?Sized + 'static>
where
    C: TurnChannels,
{
    epoch: &'epoch MountedEpoch<C, TurnProps>,
    turn_instance_id: TurnInstanceId,
    call_label: crate::StorageString,
    props: &'props TurnProps,
    user: UserTurnPlan,
}

impl<C, TurnProps> std::fmt::Debug for PreparedUserTurn<'_, '_, C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedUserTurn")
            .field("epoch_id", &self.epoch.id())
            .field("turn_instance_id", &self.turn_instance_id)
            .field("call_label", &self.call_label)
            .finish_non_exhaustive()
    }
}

impl<'epoch, 'props, C, TurnProps> PreparedUserTurn<'epoch, 'props, C, TurnProps>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    pub fn turn_instance_id(&self) -> TurnInstanceId {
        self.turn_instance_id
    }

    pub fn call_label(&self) -> &str {
        &self.call_label
    }

    pub fn user_document(&self) -> &Document {
        self.user.user_document()
    }

    pub fn into_user_plan(self) -> UserTurnPlan {
        self.user
    }

    pub(crate) fn epoch(&self) -> &'epoch MountedEpoch<C, TurnProps> {
        self.epoch
    }

    pub(crate) fn props(&self) -> &'props TurnProps {
        self.props
    }

    pub(crate) fn next_attempt_identity(&self) -> ProviderAttemptIdentity {
        ProviderAttemptIdentity::fresh(
            self.epoch.id(),
            self.turn_instance_id,
            self.call_label.clone(),
        )
    }
}

impl UserTurnPlan {
    pub fn user_document(&self) -> &Document {
        &self.user
    }

    pub fn into_document(self) -> Document {
        self.user
    }
}

/// Execute and compile a System root once for this mount invocation.
pub fn mount_system_epoch<MountProps, C, TurnProps>(
    props: &MountProps,
    render: for<'a> fn(SystemMountContext<'a, MountProps>) -> SystemView<C, TurnProps>,
) -> Result<MountedEpoch<C, TurnProps>, SystemMountError>
where
    MountProps: ?Sized,
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    mount_system_epoch_inner(None, props, render)
}

/// Execute and compile a System root once with its host-defined durable
/// contract identity.
pub fn mount_system_epoch_with_contract<MountProps, C, TurnProps>(
    epoch_contract_id: EpochContractId,
    props: &MountProps,
    render: for<'a> fn(SystemMountContext<'a, MountProps>) -> SystemView<C, TurnProps>,
) -> Result<MountedEpoch<C, TurnProps>, SystemMountError>
where
    MountProps: ?Sized,
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    mount_system_epoch_inner(Some(epoch_contract_id), props, render)
}

/// Compile an already-authored durable System component exactly once.
///
/// The durable owner uses this after combining a POM-only harness root with
/// the retained runtime catalog owned by its binding.
pub(crate) fn mount_system_component_with_contract<C, TurnProps>(
    epoch_contract_id: EpochContractId,
    root: Component<C, TurnProps>,
) -> Result<MountedEpoch<C, TurnProps>, SystemMountError>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    mount_system_view_inner(Some(epoch_contract_id), system_view(root))
}

fn mount_system_epoch_inner<MountProps, C, TurnProps>(
    epoch_contract_id: Option<EpochContractId>,
    props: &MountProps,
    render: for<'a> fn(SystemMountContext<'a, MountProps>) -> SystemView<C, TurnProps>,
) -> Result<MountedEpoch<C, TurnProps>, SystemMountError>
where
    MountProps: ?Sized,
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    let view = render(SystemMountContext { props });
    mount_system_view_inner(epoch_contract_id, view)
}

fn mount_system_view_inner<C, TurnProps>(
    epoch_contract_id: Option<EpochContractId>,
    view: SystemView<C, TurnProps>,
) -> Result<MountedEpoch<C, TurnProps>, SystemMountError>
where
    C: TurnChannels,
    TurnProps: ?Sized + 'static,
{
    let plan = if epoch_contract_id.is_some() {
        compile_durable_mount_provided(view.into_view())?
    } else {
        compile_mount_provided(view.into_view())?
    };
    let (system, user, binding_factories, provider_capabilities) = plan.into_parts();
    debug_assert!(
        user.children().is_empty(),
        "SystemView always places POM in the System document"
    );
    let resolved_system = resolve_system_document(system.clone());
    let rendered_system = Arc::<str>::from(render_pom_document(&resolved_system)?);
    let state = MountedEpochState {
        id: HarnessEpochId::fresh(),
        epoch_contract_id,
        system,
        resolved_system,
        rendered_system,
        binding_factories: Arc::new(binding_factories),
        provider_capabilities: Arc::new(provider_capabilities),
    };
    Ok(MountedEpoch {
        inner: Arc::new(ErasedMountedEpoch::new::<C, TurnProps, _>(state)),
        _contract: PhantomData,
    })
}

pub(crate) fn compile_user_view(view: UserView) -> Result<UserTurnPlan, ComponentError> {
    let plan = compile_component(view.into_view())?;
    let (system, user, hooks) = plan.into_parts();
    debug_assert!(
        system.children().is_empty(),
        "UserView always places POM in the User document"
    );
    debug_assert!(
        hooks.is_empty(),
        "NoBindings cannot construct a runtime hook"
    );
    Ok(UserTurnPlan { user })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{BindingAbortReason, Never, NoLiveEffects};

    struct FirstChannels;

    impl TurnChannels for FirstChannels {
        type Output = u32;
        type Live = Never;
        type Commit = Never;
        type Diagnostic = String;
    }

    struct SecondChannels;

    impl TurnChannels for SecondChannels {
        type Output = String;
        type Live = Never;
        type Commit = Never;
        type Diagnostic = u8;
    }

    struct FirstProps;
    struct SecondProps;

    fn first_system(_: SystemMountContext<'_, ()>) -> SystemView<FirstChannels, FirstProps> {
        system_view(super::super::component(()))
    }

    fn second_system(_: SystemMountContext<'_, ()>) -> SystemView<SecondChannels, SecondProps> {
        system_view(super::super::component(()))
    }

    #[tokio::test]
    async fn different_root_contracts_share_one_erased_epoch_storage_shape() {
        let first = mount_system_epoch(&(), first_system).unwrap();
        let second = mount_system_epoch(&(), second_system).unwrap();

        let storage: Vec<Arc<ErasedMountedEpoch>> =
            vec![Arc::clone(&first.inner), Arc::clone(&second.inner)];
        assert!(storage[0]
            .channel_types()
            .ensure::<FirstChannels, FirstProps>()
            .is_ok());
        assert!(storage[1]
            .channel_types()
            .ensure::<SecondChannels, SecondProps>()
            .is_ok());
        let mismatch = storage[0]
            .channel_types()
            .ensure::<SecondChannels, SecondProps>()
            .unwrap_err();
        assert_eq!(
            mismatch.field(),
            super::super::ChannelTypeField::RootChannels
        );

        let first_turn = first.begin_turn("first");
        let first_props = FirstProps;
        let first_prepared = first_turn
            .prepare_user(&first_props, |_| user_view(()))
            .unwrap();
        let first_attempt = first_prepared
            .start_provider_attempt(NoLiveEffects)
            .unwrap();
        assert_eq!(first_attempt.dispatcher_count(), 0);
        first_attempt.abort(BindingAbortReason::Cancelled).await;

        let second_turn = second.begin_turn("second");
        let second_props = SecondProps;
        let second_prepared = second_turn
            .prepare_user(&second_props, |_| user_view(()))
            .unwrap();
        let second_attempt = second_prepared
            .start_provider_attempt(NoLiveEffects)
            .unwrap();
        assert_eq!(second_attempt.dispatcher_count(), 0);
        second_attempt.abort(BindingAbortReason::Cancelled).await;
    }
}
