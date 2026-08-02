//! Public authoring and lifecycle boundary for a mounted durable harness.
//!
//! Normal callers compose one retained [`DurableSystem`], one renderer for
//! fresh User snapshots, and use opaque [`MountedAgent`] / [`MountedCall`]
//! handles. Provider, persistence, and host-runtime integration stays behind
//! explicit advanced adapter boundaries; ordinary authoring never names a
//! store, lease, revision, actor, or attempt typestate.

use std::{error::Error, fmt, marker::PhantomData, num::NonZeroUsize, sync::Arc};

use crate::{agent_view::AgentView, pom::Document, prompt_context::PromptContext, StorageString};

use super::{
    durable_epoch::RuntimeBinder, durable_system, mounted_agent::MountedEpochDefinition, pom_view,
    user_view, ChannelMap, ComponentError, ComponentKey, DurableCallId, DurableCallInputId,
    DurableSystem, EpochContractId, MountedCallOutcome, MountedCallResult, NoTurnChannels, PomView,
    ProviderDispatcherRegistry, TurnChannels, UserTurnContext, UserView,
};

/// Read-only host inputs available while capturing one owned User snapshot.
///
/// Capture is the only mounted-harness phase allowed to await application
/// services. It runs again for every context-replacement candidate and every
/// `Continue` turn. The synchronous User renderer receives only the returned
/// owned `TurnProps` value.
pub struct TurnCaptureContext<'a, I, ContextState, CallProps: ?Sized, Source: ?Sized> {
    pub(crate) context: &'a PromptContext<I, ContextState>,
    pub(crate) call_props: &'a CallProps,
    pub(crate) source: &'a Source,
    pub(crate) call_label: &'a str,
}

impl<'a, I, ContextState, CallProps, Source>
    TurnCaptureContext<'a, I, ContextState, CallProps, Source>
where
    CallProps: ?Sized,
    Source: ?Sized,
{
    /// Latest draft prompt context selected by the mounted owner.
    pub fn context(&self) -> &'a PromptContext<I, ContextState> {
        self.context
    }

    /// Immutable input retained for the entire logical call.
    pub fn call_props(&self) -> &'a CallProps {
        self.call_props
    }

    /// Application source retained for the entire logical call.
    pub fn source(&self) -> &'a Source {
        self.source
    }

    /// Diagnostic label supplied with the durable call input.
    pub fn call_label(&self) -> &'a str {
        self.call_label
    }
}

impl<I, ContextState, CallProps, Source> fmt::Debug
    for TurnCaptureContext<'_, I, ContextState, CallProps, Source>
where
    CallProps: ?Sized,
    Source: ?Sized,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnCaptureContext")
            .field("call_label", &self.call_label)
            .finish_non_exhaustive()
    }
}

/// Async host boundary that captures fresh, owned props for one User render.
///
/// `CallProps` and `Source` remain stable for a logical call. `TurnProps` is a
/// new owned snapshot on every capture attempt, so continuation and history
/// replacement cannot accidentally reuse stale application context.
#[async_trait::async_trait]
pub trait MountedTurnCapture: Send + Sync + 'static {
    /// Transcript item retained in the durable prompt/session history.
    ///
    /// This belongs to the capture implementation so callers do not need to
    /// spell a disconnected generic solely to bind capture to a harness.
    type Transcript: Send + Sync + 'static;
    type ContextState: Clone + Send + Sync + 'static;
    type CallProps: ?Sized + Send + Sync + 'static;
    type TurnProps: Send + Sync + 'static;
    type Source: ?Sized + Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<
            '_,
            Self::Transcript,
            Self::ContextState,
            Self::CallProps,
            Self::Source,
        >,
    ) -> Result<Self::TurnProps, Self::Error>;
}

/// Pure renderer for one application-selected User snapshot.
///
/// The renderer receives only the exact `Props` value selected by the host.
/// It must synchronously construct POM and must not perform I/O or mutate
/// session state. Capture of mutable application state belongs before this
/// boundary; provider, tool, and Live work belong after it.
pub trait UserTurnRenderer<Props: ?Sized + 'static>: Send + Sync + 'static {
    /// Render the User POM root for one preparation attempt.
    fn render(&self, context: UserTurnContext<'_, Props>) -> UserView;
}

/// Upper bound for one logical mounted call's `Wait`/`Continue` chain.
///
/// This belongs to the harness definition because `TurnFlow::Continue` is a
/// property of the mounted agent's behavior, not of an arbitrary caller. A
/// call may supply a smaller cap for its own deadline or quota, but it cannot
/// raise this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct TurnLoopPolicy {
    max_turns: NonZeroUsize,
}

impl TurnLoopPolicy {
    /// The conservative default: one committed turn per logical call.
    pub const ONE_TURN: Self = Self {
        max_turns: NonZeroUsize::MIN,
    };

    /// Permit at most `max_turns` committed turns in one logical call.
    pub const fn new(max_turns: NonZeroUsize) -> Self {
        Self { max_turns }
    }

    /// Maximum number of committed turns selected by the harness author.
    pub const fn max_turns(self) -> NonZeroUsize {
        self.max_turns
    }
}

impl Default for TurnLoopPolicy {
    fn default() -> Self {
        Self::ONE_TURN
    }
}

impl<Props, Renderer> UserTurnRenderer<Props> for Renderer
where
    Props: ?Sized + 'static,
    Renderer: for<'a> Fn(UserTurnContext<'a, Props>) -> UserView + Send + Sync + 'static,
{
    fn render(&self, context: UserTurnContext<'_, Props>) -> UserView {
        self(context)
    }
}

type UserFragmentRenderer<Props> =
    dyn for<'a> Fn(UserTurnContext<'a, Props>) -> PomView + Send + Sync + 'static;
type SharedPropsProjection<Parent, Child> =
    dyn for<'a> Fn(&'a Parent) -> &'a Child + Send + Sync + 'static;

/// A retained `#[view(component)]` invocation shared by the System and User
/// projections of one feature.
#[derive(Clone)]
struct FeatureComponentScope {
    name: StorageString,
    key: Option<ComponentKey>,
}

impl FeatureComponentScope {
    fn new(name: impl Into<StorageString>) -> Self {
        Self {
            name: name.into(),
            key: None,
        }
    }

    fn with_key(mut self, key: ComponentKey) -> Result<Self, ComponentError> {
        if self.key.is_some() {
            return Err(ComponentError::ComponentAlreadyKeyed);
        }
        self.key = Some(key);
        Ok(self)
    }

    fn wrap(&self, fragment: PomView) -> PomView {
        fragment.with_component_scope(self.name.clone(), self.key.clone())
    }
}

enum FeatureUserTree<Props: ?Sized + 'static> {
    Empty,
    Leaf(Arc<UserFragmentRenderer<Props>>),
    Sequence(Vec<Self>),
    Scope {
        scope: FeatureComponentScope,
        child: Box<Self>,
    },
}

impl<Props> FeatureUserTree<Props>
where
    Props: ?Sized + 'static,
{
    fn new(
        render: impl for<'a> Fn(UserTurnContext<'a, Props>) -> PomView + Send + Sync + 'static,
    ) -> Self {
        Self::Leaf(Arc::new(render))
    }

    fn render(&self, context: UserTurnContext<'_, Props>) -> PomView {
        match self {
            Self::Empty => pom_view(()),
            Self::Leaf(render) => render(context),
            Self::Sequence(children) => pom_view(
                children
                    .iter()
                    .map(|child| child.render(context))
                    .collect::<Vec<_>>(),
            ),
            Self::Scope { scope, child } => scope.wrap(child.render(context)),
        }
    }

    fn compose(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, other) => other,
            (this, Self::Empty) => this,
            (Self::Sequence(mut left), Self::Sequence(right)) => {
                left.extend(right);
                Self::Sequence(left)
            }
            (Self::Sequence(mut left), right) => {
                left.push(right);
                Self::Sequence(left)
            }
            (left, Self::Sequence(mut right)) => {
                right.insert(0, left);
                Self::Sequence(right)
            }
            (left, right) => Self::Sequence(vec![left, right]),
        }
    }

    fn with_component_scope(self, scope: FeatureComponentScope) -> Self {
        if matches!(self, Self::Empty) {
            Self::Empty
        } else {
            Self::Scope {
                scope,
                child: Box::new(self),
            }
        }
    }

    fn set_outer_component_key(&mut self, key: ComponentKey) -> Result<(), ComponentError> {
        match self {
            Self::Empty => Ok(()),
            Self::Scope { scope, .. } => {
                *scope = scope.clone().with_key(key)?;
                Ok(())
            }
            Self::Leaf(_) | Self::Sequence(_) => Err(ComponentError::KeyRequiresComponent),
        }
    }

    fn project_props<Parent>(
        self,
        project: Arc<SharedPropsProjection<Parent, Props>>,
    ) -> FeatureUserTree<Parent>
    where
        Parent: ?Sized + 'static,
    {
        match self {
            Self::Empty => FeatureUserTree::Empty,
            Self::Leaf(render) => {
                FeatureUserTree::Leaf(Arc::new(move |context: UserTurnContext<'_, Parent>| {
                    render(UserTurnContext::new(project(context.props())))
                }))
            }
            Self::Sequence(children) => FeatureUserTree::Sequence(
                children
                    .into_iter()
                    .map(|child| child.project_props(Arc::clone(&project)))
                    .collect(),
            ),
            Self::Scope { scope, child } => FeatureUserTree::Scope {
                scope,
                child: Box::new(child.project_props(project)),
            },
        }
    }

    fn fragment_count(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Leaf(_) => 1,
            Self::Sequence(children) => children.iter().map(Self::fragment_count).sum(),
            Self::Scope { child, .. } => child.fragment_count(),
        }
    }
}

/// Reusable pure contribution to a mounted durable harness.
///
/// A feature owns its retained durable System/runtime tree together with one
/// retained per-turn User POM tree. Features compose in author order: their
/// System trees form the one retained durable epoch source, and their User
/// subtrees render in the same order for every fresh User snapshot. This lets a
/// streaming or tool feature govern both its stable contract and its changing
/// prompt-facing context without reopening a second prompt-authoring model.
///
/// A feature deliberately does not own [`MountedTurnCapture`], a provider,
/// persistence, or Live effects. Capture may await host services to produce an
/// owned root snapshot; the feature receives only that snapshot and remains
/// synchronous and side-effect free.
#[must_use]
pub struct MountedFeature<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    durable_system: DurableSystem<C, Props>,
    user_tree: FeatureUserTree<Props>,
    has_outer_component_scope: bool,
}

/// Prompt-only component with one retained System POM and a fresh User POM
/// for each captured turn.
///
/// This is the smallest mounted authoring surface. It is an alias for
/// [`MountedFeature`] with no output, Live, Commit, or diagnostic channels;
/// adding a provided component promotes the feature to an explicit channel
/// contract without changing its System/User lifecycle.
pub type PromptComponent<Props> = MountedFeature<NoTurnChannels, Props>;

/// Build the prompt-only form of a mounted component from typed POM values.
///
/// `system` is converted to POM when the retained component definition is
/// constructed and is rendered/attached only if a host creates the durable
/// epoch. `render_user` remains pure and is called with each fresh owned turn
/// snapshot selected by the host.
pub fn prompt_component<Props, System, User>(
    system: System,
    render_user: impl Fn(&Props) -> User + Send + Sync + 'static,
) -> PromptComponent<Props>
where
    Props: ?Sized + 'static,
    System: AgentView<Root = Document>,
    User: AgentView<Root = Document>,
{
    try_prompt_component(system, move |props| render_user(props).build_root())
}

/// Build a prompt-only component whose per-turn User POM authoring may fail.
///
/// This is the fallible counterpart to [`prompt_component`]. It also accepts
/// an already-built POM fragment such as [`Document`], which is useful when a
/// larger application already has a pure User-document builder.
pub fn try_prompt_component<Props, System, User, Error>(
    system: System,
    render_user: impl Fn(&Props) -> Result<User, Error> + Send + Sync + 'static,
) -> PromptComponent<Props>
where
    Props: ?Sized + 'static,
    System: AgentView<Root = Document>,
    User: super::PomChildren,
    Error: Into<ComponentError>,
{
    let system: DurableSystem<NoTurnChannels, Props> = match system.build_root() {
        Ok(document) => durable_system(document),
        Err(error) => DurableSystem::from_error(error.into()),
    };

    MountedFeature::try_new(system, move |turn| render_user(turn.props()))
}

struct FeatureUserRenderer<Props: ?Sized + 'static> {
    tree: FeatureUserTree<Props>,
}

impl<Props> UserTurnRenderer<Props> for FeatureUserRenderer<Props>
where
    Props: ?Sized + 'static,
{
    fn render(&self, context: UserTurnContext<'_, Props>) -> UserView {
        user_view(self.tree.render(context))
    }
}

impl<C, Props> MountedFeature<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    /// Construct one feature with a retained durable System contribution and
    /// one fresh User POM leaf.
    pub fn new(
        durable_system: DurableSystem<C, Props>,
        render_user: impl for<'a> Fn(UserTurnContext<'a, Props>) -> PomView + Send + Sync + 'static,
    ) -> Self {
        Self {
            durable_system,
            user_tree: FeatureUserTree::new(render_user),
            has_outer_component_scope: false,
        }
    }

    /// Construct one feature whose fresh User POM authoring may fail.
    ///
    /// The renderer remains synchronous and side-effect free. Its error is
    /// retained in the User component tree and reported by turn preparation;
    /// constructing the feature does not render the User document eagerly.
    pub fn try_new<User, Error>(
        durable_system: DurableSystem<C, Props>,
        render_user: impl for<'a> Fn(UserTurnContext<'a, Props>) -> Result<User, Error>
            + Send
            + Sync
            + 'static,
    ) -> Self
    where
        User: super::PomChildren,
        Error: Into<ComponentError>,
    {
        Self::new(durable_system, move |context| match render_user(context) {
            Ok(user) => pom_view(user),
            Err(error) => PomView::from_error(error.into()),
        })
    }

    /// Construct a feature that contributes only durable System/runtime data.
    pub fn system_only(durable_system: DurableSystem<C, Props>) -> Self {
        Self {
            durable_system,
            user_tree: FeatureUserTree::Empty,
            has_outer_component_scope: false,
        }
    }

    /// Construct a feature that contributes only fresh User POM.
    pub fn user_only(
        render_user: impl for<'a> Fn(UserTurnContext<'a, Props>) -> PomView + Send + Sync + 'static,
    ) -> Self {
        Self::new(durable_system(()), render_user)
    }

    /// Append another feature in System and User authoring order.
    pub fn compose(mut self, other: Self) -> Self {
        self.durable_system = durable_system((self.durable_system, other.durable_system));
        self.user_tree = self.user_tree.compose(other.user_tree);
        // Composition creates a retained subtree. A caller that needs a fresh
        // keyed component boundary writes an enclosing `#[view(component)]`.
        self.has_outer_component_scope = false;
        self
    }

    /// Append a harness-binding-owned System contribution before finalization.
    ///
    /// Ordinary component composition uses [`Self::compose`]. A runtime binding
    /// uses this crate-private path only while consuming the complete root, so
    /// its stable System contract is covered by the same epoch definition.
    pub(crate) fn append_harness_system(mut self, system: DurableSystem<C, Props>) -> Self {
        self.durable_system = self.durable_system.append(system);
        self
    }

    /// Attach a stable parent-provided key to this component invocation.
    ///
    /// Keys belong to an outer `#[view(component)]` result. A manually
    /// flattened feature has no invocation boundary, so it reports the same
    /// `KeyRequiresComponent` error as a POM/component fragment would.
    pub fn key(self, key: impl Into<StorageString>) -> Self {
        let key = match ComponentKey::new(key) {
            Ok(key) => key,
            Err(error) => return self.with_component_error(error),
        };
        if !self.has_outer_component_scope {
            return self.with_component_error(ComponentError::KeyRequiresComponent);
        }

        let mut feature = self;
        if let Err(error) = feature.durable_system.set_outer_component_key(key.clone()) {
            return feature.with_component_error(error);
        }
        if let Err(error) = feature.user_tree.set_outer_component_key(key) {
            return feature.with_component_error(error);
        }
        feature
    }

    /// Retain the macro-provided component scope in both projections.
    pub(crate) fn with_component_scope(mut self, name: impl Into<StorageString>) -> Self {
        let scope = FeatureComponentScope::new(name);
        self.durable_system = self.durable_system.with_component_scope(scope.name.clone());
        self.user_tree = self.user_tree.with_component_scope(scope);
        self.has_outer_component_scope = true;
        self
    }

    fn with_component_error(mut self, error: ComponentError) -> Self {
        self.durable_system = DurableSystem::from_error(error);
        self.user_tree = FeatureUserTree::Empty;
        self.has_outer_component_scope = false;
        self
    }

    /// Lift this feature's complete local channel contract into a parent
    /// harness contract.
    ///
    /// The map is applied to every durable runtime lane together. The retained
    /// System POM and fresh User tree are unchanged, because they are
    /// prompt-facing rather than emitted through turn channels.
    pub fn map_channels<Root>(self, map: impl ChannelMap<C, Root>) -> MountedFeature<Root, Props>
    where
        Root: TurnChannels,
    {
        let Self {
            durable_system,
            user_tree,
            has_outer_component_scope,
        } = self;
        MountedFeature {
            durable_system: durable_system.map_channels(map),
            user_tree,
            has_outer_component_scope,
        }
    }

    /// Project parent snapshots into the props required by this feature.
    ///
    /// The projection is borrowed and pure. It is used by the durable runtime
    /// only while creating fresh per-turn bindings/dispatchers and by each User
    /// leaf for that same snapshot; it never runs while System POM is
    /// rendered or while a durable epoch reopens.
    pub fn project_props<Parent>(
        self,
        project: impl for<'a> Fn(&'a Parent) -> &'a Props + Send + Sync + 'static,
    ) -> MountedFeature<C, Parent>
    where
        Parent: ?Sized + 'static,
    {
        let project: Arc<SharedPropsProjection<Parent, Props>> = Arc::new(project);
        let durable_system = self.durable_system.project_props({
            let project = Arc::clone(&project);
            move |props: &Parent| project(props)
        });
        let user_tree = self.user_tree.project_props(project);

        MountedFeature {
            durable_system,
            user_tree,
            has_outer_component_scope: self.has_outer_component_scope,
        }
    }

    /// Finish a pure feature tree as one mounted harness definition.
    ///
    /// The supplied contract id covers the complete durable System tree. User
    /// tree remains per-turn and does not change that epoch identity.
    pub fn into_harness(
        self,
        epoch_contract_id: EpochContractId,
    ) -> MountedHarnessDefinition<C, Props> {
        let epoch = DurableEpochDefinition::new(epoch_contract_id, self.durable_system);
        MountedHarnessDefinition::new(
            epoch,
            FeatureUserRenderer {
                tree: self.user_tree,
            },
        )
    }
}

impl<C, Props> fmt::Debug for MountedFeature<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedFeature")
            .field("user_fragment_count", &self.user_tree.fragment_count())
            .field("has_outer_component_scope", &self.has_outer_component_scope)
            .finish_non_exhaustive()
    }
}

/// Invalid metadata supplied for one owned mounted call input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MountedCallInputError {
    #[error(
        "invalid mounted call label `{value}`; labels must be non-empty and contain no control characters"
    )]
    InvalidLabel { value: String },
}

/// Owned application input for one logical mounted call.
///
/// The durable call and input identities are selected by the host before a
/// call starts. `Props` is the immutable host input selected for this call;
/// an advanced host binding may capture a distinct owned User snapshot from
/// it. `Source` carries host services or application data needed by that
/// capture/runtime work. The mounted owner consumes this value once when it
/// admits the call, so this type intentionally does not implement [`Clone`].
///
/// This value is pure data. Constructing it neither opens a provider epoch nor
/// renders a User view.
#[must_use]
pub struct MountedCallInput<Props: ?Sized + 'static, Source: ?Sized + 'static = ()> {
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    call_label: StorageString,
    props: Arc<Props>,
    source: Arc<Source>,
    max_context_replacements: usize,
    turn_cap: Option<NonZeroUsize>,
}

impl<Props, Source> MountedCallInput<Props, Source>
where
    Props: ?Sized + 'static,
    Source: ?Sized + 'static,
{
    /// Bind stable durable identities to one owned call snapshot.
    pub fn new(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        call_label: impl Into<StorageString>,
        props: Arc<Props>,
        source: Arc<Source>,
    ) -> Result<Self, MountedCallInputError> {
        let call_label = call_label.into();
        if call_label.is_empty() || call_label.chars().any(char::is_control) {
            return Err(MountedCallInputError::InvalidLabel {
                value: call_label.to_string(),
            });
        }
        Ok(Self {
            call_id,
            input_id,
            call_label,
            props,
            source,
            max_context_replacements: 0,
            turn_cap: None,
        })
    }

    /// Allow this many bounded preparation rewrites before the call fails.
    ///
    /// Both provider-requested history compaction and a full User-document
    /// resync consume this shared budget. For example, permitting one of each
    /// requires a value of `2`.
    pub fn with_max_preparation_rewrites(mut self, maximum: usize) -> Self {
        self.max_context_replacements = maximum;
        self
    }

    /// Compatibility alias for [`Self::with_max_preparation_rewrites`].
    #[deprecated(
        note = "this budget also covers User-document resync; use with_max_preparation_rewrites"
    )]
    pub fn with_max_context_replacements(self, maximum: usize) -> Self {
        self.with_max_preparation_rewrites(maximum)
    }

    /// Further restrict this call's number of committed turns.
    ///
    /// The final budget is the smaller of this value and the harness
    /// [`TurnLoopPolicy`]. This is intentionally a cap: callers cannot use an
    /// input to increase the harness's `Wait`/`Continue` budget.
    pub fn with_turn_cap(mut self, maximum: NonZeroUsize) -> Self {
        self.turn_cap = Some(maximum);
        self
    }

    /// Durable identity of the logical call.
    pub fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    /// Durable identity of the immutable input revision.
    pub fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    /// Diagnostic host label for this call.
    pub fn call_label(&self) -> &str {
        &self.call_label
    }

    /// Immutable host props selected for this call.
    pub fn props(&self) -> &Arc<Props> {
        &self.props
    }

    /// Owned application source associated with this call.
    pub fn source(&self) -> &Arc<Source> {
        &self.source
    }

    /// Maximum combined history-compaction and User-resync rewrites.
    pub fn max_preparation_rewrites(&self) -> usize {
        self.max_context_replacements
    }

    /// Compatibility alias for [`Self::max_preparation_rewrites`].
    #[deprecated(
        note = "this value also covers User-document resync; use max_preparation_rewrites"
    )]
    pub fn max_context_replacements(&self) -> usize {
        self.max_preparation_rewrites()
    }

    /// Optional caller-selected cap applied below the harness policy.
    pub fn turn_cap(&self) -> Option<NonZeroUsize> {
        self.turn_cap
    }

    /// Consume the payload for the mounted owner actor.
    ///
    /// This is crate-private so only an admitted mounted owner can take the
    /// fields apart. Advanced adapters receive the owned input but can retain
    /// its immutable `Arc` values through the public accessors.
    pub(crate) fn into_parts(
        self,
    ) -> (
        DurableCallId,
        DurableCallInputId,
        StorageString,
        Arc<Props>,
        Arc<Source>,
        usize,
        Option<NonZeroUsize>,
    ) {
        (
            self.call_id,
            self.input_id,
            self.call_label,
            self.props,
            self.source,
            self.max_context_replacements,
            self.turn_cap,
        )
    }
}

impl<Props, Source> fmt::Debug for MountedCallInput<Props, Source>
where
    Props: ?Sized + 'static,
    Source: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedCallInput")
            .field("call_id", &self.call_id)
            .field("input_id", &self.input_id)
            .field("call_label", &self.call_label)
            .finish_non_exhaustive()
    }
}

/// The only retained System source for one durable epoch.
///
/// This is the replaceable portion of a mounted harness. It deliberately
/// cannot carry host configuration, the User renderer, store, reducer,
/// request-id policy, or provider session. Its POM projection is linear and
/// may be consumed only by the winner of durable Create admission; reopen uses
/// the same value's POM-free runtime projection.
#[must_use]
pub struct DurableEpochDefinition<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    epoch_contract_id: EpochContractId,
    durable_system: Arc<DurableSystem<C, Props>>,
}

impl<C, Props> DurableEpochDefinition<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    /// Construct one immutable durable System epoch definition.
    ///
    /// `epoch_contract_id` is an author-declared semantic version of the
    /// retained System contract. A change to System POM, a durable runtime
    /// declaration, tool capability/schema, or its behavior requires a new
    /// id and an explicit host-driven epoch replacement. Per-turn User POM
    /// changes do not. Reusing an id because rendered text happens to look
    /// similar is not valid compatibility.
    pub fn new(
        epoch_contract_id: EpochContractId,
        durable_system: DurableSystem<C, Props>,
    ) -> Self {
        Self::from_shared(epoch_contract_id, Arc::new(durable_system))
    }

    pub(crate) fn from_shared(
        epoch_contract_id: EpochContractId,
        durable_system: Arc<DurableSystem<C, Props>>,
    ) -> Self {
        Self {
            epoch_contract_id,
            durable_system,
        }
    }

    pub fn epoch_contract_id(&self) -> &EpochContractId {
        &self.epoch_contract_id
    }

    pub fn durable_system(&self) -> &DurableSystem<C, Props> {
        &self.durable_system
    }

    pub(crate) fn durable_system_arc(&self) -> Arc<DurableSystem<C, Props>> {
        Arc::clone(&self.durable_system)
    }
}

impl<C, Props> fmt::Debug for DurableEpochDefinition<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableEpochDefinition")
            .field("epoch_contract_id", &self.epoch_contract_id)
            .finish_non_exhaustive()
    }
}

impl<C, Props> RuntimeBinder<C, Props> for DurableEpochDefinition<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + Send + Sync + 'static,
{
    fn durable_system(&self) -> Arc<DurableSystem<C, Props>> {
        self.durable_system_arc()
    }
}

impl<C, Props> MountedEpochDefinition<C, Props> for DurableEpochDefinition<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + Send + Sync + 'static,
{
    fn epoch_contract_id(&self) -> &EpochContractId {
        self.epoch_contract_id()
    }
}

/// Pure authoring root for one mounted harness.
///
/// System and User have intentionally different lifetimes. The epoch is
/// retained across calls and recovery; the User renderer receives each fresh,
/// application-defined `Props` snapshot selected by the turn capture layer.
#[must_use]
pub struct MountedHarnessDefinition<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    epoch: Arc<DurableEpochDefinition<C, Props>>,
    user_renderer: Arc<dyn UserTurnRenderer<Props>>,
    turn_loop_policy: TurnLoopPolicy,
}

impl<C, Props> MountedHarnessDefinition<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    pub fn new(
        epoch: DurableEpochDefinition<C, Props>,
        user_renderer: impl UserTurnRenderer<Props>,
    ) -> Self {
        Self {
            epoch: Arc::new(epoch),
            user_renderer: Arc::new(user_renderer),
            turn_loop_policy: TurnLoopPolicy::default(),
        }
    }

    pub fn epoch(&self) -> &DurableEpochDefinition<C, Props> {
        &self.epoch
    }

    /// Select the maximum `Wait`/`Continue` chain length for this harness.
    ///
    /// The default is [`TurnLoopPolicy::ONE_TURN`]. A call input can only set a
    /// lower cap through [`MountedCallInput::with_turn_cap`].
    pub fn with_turn_loop_policy(mut self, policy: TurnLoopPolicy) -> Self {
        self.turn_loop_policy = policy;
        self
    }

    /// The fixed loop policy selected by the harness author.
    pub fn turn_loop_policy(&self) -> TurnLoopPolicy {
        self.turn_loop_policy
    }

    pub(crate) fn render_user(&self, context: UserTurnContext<'_, Props>) -> UserView {
        self.user_renderer.render(context)
    }

    /// Bind the pure System/User definition to the host's fresh-turn capture.
    ///
    /// The returned value is the executable harness definition consumed by a
    /// mounted owner. Capture remains outside the functional component tree,
    /// but it is no longer an implicit responsibility of a provider factory.
    pub fn with_capture<Capture>(
        self,
        capture: Capture,
    ) -> CapturedMountedHarnessDefinition<C, Capture>
    where
        Capture: MountedTurnCapture<TurnProps = Props>,
        Props: Send + Sync,
    {
        CapturedMountedHarnessDefinition {
            definition: self,
            capture: Arc::new(capture),
            transcript: PhantomData,
        }
    }

    pub(crate) fn epoch_arc(&self) -> Arc<DurableEpochDefinition<C, Props>> {
        Arc::clone(&self.epoch)
    }
}

impl<C, Props> fmt::Debug for MountedHarnessDefinition<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedHarnessDefinition")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// Executable mounted harness: one immutable POM definition plus one capture
/// policy fixed for the owner's lifetime.
///
/// Explicit System reconfiguration replaces only the epoch definition. It does
/// not replace this capture policy or the User renderer.
#[must_use]
pub struct CapturedMountedHarnessDefinition<
    C,
    Capture,
    I = <Capture as MountedTurnCapture>::Transcript,
> where
    C: TurnChannels,
    Capture: MountedTurnCapture<Transcript = I>,
{
    definition: MountedHarnessDefinition<C, Capture::TurnProps>,
    capture: Arc<Capture>,
    transcript: PhantomData<fn(I)>,
}

impl<C, Capture, I> CapturedMountedHarnessDefinition<C, Capture, I>
where
    C: TurnChannels,
    Capture: MountedTurnCapture<Transcript = I>,
{
    pub fn epoch(&self) -> &DurableEpochDefinition<C, Capture::TurnProps> {
        self.definition.epoch()
    }

    /// The fixed loop policy retained with this executable harness definition.
    pub fn turn_loop_policy(&self) -> TurnLoopPolicy {
        self.definition.turn_loop_policy()
    }

    pub(crate) fn epoch_arc(&self) -> Arc<DurableEpochDefinition<C, Capture::TurnProps>> {
        self.definition.epoch_arc()
    }

    pub(crate) async fn capture_turn_props(
        &self,
        context: TurnCaptureContext<
            '_,
            I,
            Capture::ContextState,
            Capture::CallProps,
            Capture::Source,
        >,
    ) -> Result<Capture::TurnProps, Capture::Error> {
        self.capture.capture_turn_props(context).await
    }

    pub(crate) fn render_user(&self, context: UserTurnContext<'_, Capture::TurnProps>) -> UserView {
        self.definition.render_user(context)
    }
}

impl<C, Capture, I> fmt::Debug for CapturedMountedHarnessDefinition<C, Capture, I>
where
    C: TurnChannels,
    Capture: MountedTurnCapture<Transcript = I>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedMountedHarnessDefinition")
            .field("epoch", self.epoch())
            .finish_non_exhaustive()
    }
}

/// Failure while opening a mounted durable owner.
///
/// Detailed adapter failures remain in the advanced integration layer. This
/// public projection tells a host whether it can retry, must reopen against a
/// newer epoch, or needs an explicit recovery operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MountedOpenError {
    #[error("mounted configuration is invalid: {message}")]
    Configuration { message: String },

    #[error("mounted epoch contract is incompatible: {message}")]
    ContractMismatch { message: String },

    #[error("mounted durable state requires explicit recovery: {message}")]
    RecoveryRequired { message: String },

    #[error("mounted owner is temporarily unavailable: {message}")]
    Unavailable { message: String },

    #[error("mounted open outcome is indeterminate: {message}")]
    Indeterminate { message: String },
}

impl MountedOpenError {
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration {
            message: message.into(),
        }
    }

    pub fn contract_mismatch(message: impl Into<String>) -> Self {
        Self::ContractMismatch {
            message: message.into(),
        }
    }

    pub fn recovery_required(message: impl Into<String>) -> Self {
        Self::RecoveryRequired {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn indeterminate(message: impl Into<String>) -> Self {
        Self::Indeterminate {
            message: message.into(),
        }
    }
}

/// Failure while admitting a mounted call.
///
/// A duplicate settled call is not an error: `start` returns an opaque call
/// handle whose `wait` result is [`MountedCallOutcome::Replayed`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MountedStartError {
    #[error("call `{call_id}` is already in progress")]
    CallInProgress { call_id: DurableCallId },

    #[error(
        "call `{call_id}` used input `{actual}`, but durable state requires input `{expected}`"
    )]
    CallInputMismatch {
        call_id: DurableCallId,
        expected: DurableCallInputId,
        actual: DurableCallInputId,
    },

    #[error("call `{call_id}` requires explicit durable recovery")]
    RecoveryRequired { call_id: DurableCallId },

    #[error("mounted owner must reopen before admitting a call: {message}")]
    ReopenRequired { message: String },

    #[error("mounted call admission was rejected: {message}")]
    Rejected { message: String },

    #[error("mounted call admission is temporarily unavailable: {message}")]
    Unavailable { message: String },

    #[error("mounted call admission outcome is indeterminate: {message}")]
    Indeterminate { message: String },
}

impl MountedStartError {
    pub fn reopen_required(message: impl Into<String>) -> Self {
        Self::ReopenRequired {
            message: message.into(),
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn indeterminate(message: impl Into<String>) -> Self {
        Self::Indeterminate {
            message: message.into(),
        }
    }
}

/// Failure observed while waiting for a durably admitted mounted call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MountedWaitError {
    #[error("mounted call was cancelled")]
    Cancelled,

    #[error("mounted call completion was already observed")]
    AlreadyObserved,

    #[error("mounted call preparation failed before provider execution: {message}")]
    PreparationFailed { message: String },

    #[error("mounted provider reply was rejected by the session contract: {message}")]
    Rejected { message: String },

    #[error("mounted call requires explicit durable recovery: {message}")]
    RecoveryRequired { message: String },

    #[error("mounted owner must reopen before this call can continue: {message}")]
    ReopenRequired { message: String },

    #[error("mounted call is temporarily unavailable: {message}")]
    Unavailable { message: String },

    #[error("mounted call outcome is indeterminate: {message}")]
    Indeterminate { message: String },
}

impl MountedWaitError {
    /// The admitted call failed before the provider began execution.
    ///
    /// The mounted owner has released its pre-provider reservation, so a host
    /// may submit the same durable call/input identity again after addressing
    /// the reported preparation failure. This does not imply that application
    /// capture itself was side-effect free or that retrying is automatic.
    pub fn preparation_failed(message: impl Into<String>) -> Self {
        Self::PreparationFailed {
            message: message.into(),
        }
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self::Rejected {
            message: message.into(),
        }
    }

    pub fn recovery_required(message: impl Into<String>) -> Self {
        Self::RecoveryRequired {
            message: message.into(),
        }
    }

    pub fn reopen_required(message: impl Into<String>) -> Self {
        Self::ReopenRequired {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn indeterminate(message: impl Into<String>) -> Self {
        Self::Indeterminate {
            message: message.into(),
        }
    }
}

/// Failure while reloading an already-open mounted owner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MountedReloadError {
    #[error("mounted epoch contract changed and this owner must reopen: {message}")]
    ContractMismatch { message: String },

    #[error("mounted durable state requires explicit recovery: {message}")]
    RecoveryRequired { message: String },

    #[error("mounted owner must reopen: {message}")]
    ReopenRequired { message: String },

    #[error("mounted reload is temporarily unavailable: {message}")]
    Unavailable { message: String },

    #[error("mounted reload outcome is indeterminate: {message}")]
    Indeterminate { message: String },
}

impl MountedReloadError {
    pub fn contract_mismatch(message: impl Into<String>) -> Self {
        Self::ContractMismatch {
            message: message.into(),
        }
    }

    pub fn recovery_required(message: impl Into<String>) -> Self {
        Self::RecoveryRequired {
            message: message.into(),
        }
    }

    pub fn reopen_required(message: impl Into<String>) -> Self {
        Self::ReopenRequired {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn indeterminate(message: impl Into<String>) -> Self {
        Self::Indeterminate {
            message: message.into(),
        }
    }
}

/// Result of one call-scoped cancellation request.
///
/// `Requested` and `AlreadyRequested` are returned only after the driver has
/// joined the active provider/attempt cleanup for this exact call. Dropping a
/// `wait` future never produces this result or requests cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountedCallCancellation {
    Requested,
    AlreadyRequested,
    AlreadyFinished,
}

/// Public reason why a durable call requires host reconciliation.
///
/// This projection intentionally omits the store lease, publication request,
/// revision, and provider cursor used to fence the recovery transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountedCallRecoveryReason {
    LeaseExpired,
    PublicationNotCommitted,
    PublicationCandidateCollision,
    CancellationIndeterminate,
    CancellationCleanupUnacknowledged,
    CancellationProviderJoinTimedOut,
    CancellationPendingPublication,
    CancellationEpochMismatch,
    CancellationCursorInvalid,
    CancellationSettlementConflict,
    SessionReductionIndeterminate,
}

/// Read-only lifecycle of one durable mounted call.
///
/// These states are application-facing observations, not mutation authority.
/// In particular, `Running` and `RecoveryRequired` do not expose the private
/// lease needed to change authoritative state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountedCallLifecycle {
    /// The call is durably reserved while the owner captures and prepares User.
    Preparing,
    /// Provider, tool, or Live execution may have started.
    Running,
    /// A committed turn requested another freshly captured User turn.
    AwaitingContinuation,
    /// The logical call completed and has a replayable durable result.
    Settled { result: MountedCallResult },
    /// The host paused the call at a stable boundary.
    Paused,
    /// The call was durably stopped without accepting another turn.
    Stopped,
    /// Automatic replay is unsafe until the mount-owned recovery policy
    /// reconciles provider and publication state.
    RecoveryRequired { reason: MountedCallRecoveryReason },
}

/// Lease-free lifecycle snapshot returned by [`MountedAgent::lookup`].
///
/// A same-process active actor reports progress only after durable admission;
/// otherwise the snapshot is projected from the mount-owned store. Like any
/// observation it may become stale immediately and grants no mutation fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedCallSnapshot {
    call_id: DurableCallId,
    input_id: DurableCallInputId,
    next_turn_index: u64,
    lifecycle: MountedCallLifecycle,
}

/// Result of reattaching to a durable call by id.
///
/// A live actor can only be attached through the mounted owner instance that
/// admitted it, and at most one attached [`MountedCall`] exists for that actor
/// at a time. `Observed` is deliberately read-only: it covers calls that
/// already have an attached handle, terminal calls, and calls owned by another
/// owner instance or process. It cannot be used to signal cancellation without
/// a later fenced external-control operation.
#[non_exhaustive]
pub enum MountedCallReattachment<C>
where
    C: TurnChannels,
{
    NotFound,
    Attached { call: MountedCall<C> },
    Observed { snapshot: MountedCallSnapshot },
}

impl<C> fmt::Debug for MountedCallReattachment<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("MountedCallReattachment::NotFound"),
            Self::Attached { call } => formatter
                .debug_struct("MountedCallReattachment::Attached")
                .field("call_id", &call.call_id())
                .finish_non_exhaustive(),
            Self::Observed { snapshot } => formatter
                .debug_struct("MountedCallReattachment::Observed")
                .field("snapshot", snapshot)
                .finish(),
        }
    }
}

impl MountedCallSnapshot {
    pub fn call_id(&self) -> &DurableCallId {
        &self.call_id
    }

    pub fn input_id(&self) -> &DurableCallInputId {
        &self.input_id
    }

    /// Turn index that would execute next if the lifecycle permits resumption.
    pub fn next_turn_index(&self) -> u64 {
        self.next_turn_index
    }

    pub fn lifecycle(&self) -> &MountedCallLifecycle {
        &self.lifecycle
    }

    pub(crate) fn new(
        call_id: DurableCallId,
        input_id: DurableCallInputId,
        next_turn_index: u64,
        lifecycle: MountedCallLifecycle,
    ) -> Self {
        Self {
            call_id,
            input_id,
            next_turn_index,
            lifecycle,
        }
    }
}

/// Failure while reading one call from the mount-owned durable store.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MountedCallLookupError {
    #[error("mounted owner must reopen before call lookup can continue: {message}")]
    ReopenRequired { message: String },

    #[error("mounted call lookup is temporarily unavailable: {message}")]
    Unavailable { message: String },

    #[error("mounted call lookup result is indeterminate: {message}")]
    Indeterminate { message: String },
}

impl MountedCallLookupError {
    pub fn reopen_required(message: impl Into<String>) -> Self {
        Self::ReopenRequired {
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn indeterminate(message: impl Into<String>) -> Self {
        Self::Indeterminate {
            message: message.into(),
        }
    }
}

/// Host-owned runtime bindings for one mounted owner open.
///
/// A component author declares only durable POM and typed capability
/// contracts. The host supplies the process-local implementations that make
/// those contracts executable here. This value is intentionally not retained
/// in a durable epoch artifact: it is rebuilt and validated on every open or
/// reopen before the System can render or attach.
///
/// The first binding family is provider dispatcher implementations. Future
/// host-owned binding families belong in this value rather than becoming
/// concrete-factory-only `open_*` methods.
#[derive(Clone)]
pub struct MountedHostBindings<C, Props: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    provider_dispatchers: ProviderDispatcherRegistry<C, Props>,
}

impl<C, Props> fmt::Debug for MountedHostBindings<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedHostBindings")
            .field("provider_dispatchers", &self.provider_dispatchers)
            .finish()
    }
}

impl<C, Props> MountedHostBindings<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    /// Construct an empty host binding set.
    ///
    /// This is appropriate only when the System has no pure provider
    /// capability contracts. Such a contract without a matching dispatcher
    /// still fails closed during open preflight.
    pub fn new() -> Self {
        Self {
            provider_dispatchers: ProviderDispatcherRegistry::new(),
        }
    }

    /// Construct bindings from the host's provider dispatcher registry.
    pub fn with_provider_dispatchers(
        provider_dispatchers: ProviderDispatcherRegistry<C, Props>,
    ) -> Self {
        Self {
            provider_dispatchers,
        }
    }

    /// Inspect the provider dispatcher registry selected by this host.
    pub fn provider_dispatchers(&self) -> &ProviderDispatcherRegistry<C, Props> {
        &self.provider_dispatchers
    }

    pub(crate) fn into_provider_dispatchers(self) -> ProviderDispatcherRegistry<C, Props> {
        self.provider_dispatchers
    }
}

impl<C, Props> Default for MountedHostBindings<C, Props>
where
    C: TurnChannels,
    Props: ?Sized + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Advanced factory and driver contracts behind the opaque mounted facade.
///
/// These are integration APIs. They may coordinate providers, persistence, or
/// compatibility runtimes, but they must preserve the public lifecycle
/// semantics: Create renders/attaches System once, reopen does not render or
/// resend System, `start` acknowledges durable admission, and `cancel` joins
/// call-scoped cleanup. Normal component authors should not import this module.
pub(crate) mod advanced {
    use std::sync::Arc;

    use super::{
        CapturedMountedHarnessDefinition, DurableCallId, MountedCallCancellation, MountedCallInput,
        MountedCallLookupError, MountedCallOutcome, MountedCallSnapshot, MountedHostBindings,
        MountedOpenError, MountedReloadError, MountedStartError, MountedTurnCapture,
        MountedWaitError, TurnChannels,
    };

    pub(crate) enum MountedCallReattachmentDriver<C>
    where
        C: TurnChannels,
    {
        NotFound,
        Attached(Arc<dyn MountedCallDriver<C>>),
        Observed(MountedCallSnapshot),
    }
    /// Opens one opaque mounted owner from an immutable harness definition.
    #[async_trait::async_trait]
    pub trait MountedAgentFactory<C, Capture, I>: Send + Sync + 'static
    where
        C: TurnChannels,
        Capture: MountedTurnCapture<Transcript = I>,
        I: Send + Sync + 'static,
    {
        async fn open(
            &self,
            definition: CapturedMountedHarnessDefinition<C, Capture, I>,
            bindings: MountedHostBindings<C, Capture::TurnProps>,
        ) -> Result<
            Arc<dyn MountedAgentDriver<C, Capture::CallProps, Capture::Source>>,
            MountedOpenError,
        >;
    }

    /// Advanced runtime endpoint retained by [`super::MountedAgent`].
    #[async_trait::async_trait]
    pub trait MountedAgentDriver<C, CallProps, Source>: Send + Sync + 'static
    where
        C: TurnChannels,
        CallProps: ?Sized + 'static,
        Source: ?Sized + 'static,
    {
        /// Consume one input only after the driver can return an admission
        /// result. A successful duplicate returns a call whose terminal value
        /// is `Replayed`; it must not invoke provider work.
        async fn start(
            &self,
            input: MountedCallInput<CallProps, Source>,
        ) -> Result<Arc<dyn MountedCallDriver<C>>, MountedStartError>;

        /// Read one durable call without exposing store or lease internals.
        async fn lookup(
            &self,
            call_id: &DurableCallId,
        ) -> Result<Option<MountedCallSnapshot>, MountedCallLookupError>;

        /// Reattach only to an actor owned by this process. A durable call
        /// without a local actor is returned as an observational snapshot.
        async fn reattach(
            &self,
            call_id: &DurableCallId,
        ) -> Result<MountedCallReattachmentDriver<C>, MountedCallLookupError>;

        /// Rehydrate authoritative session/cursor/provider state without
        /// remounting or retransmitting System.
        async fn reload(&self) -> Result<(), MountedReloadError>;
    }

    /// Advanced terminal endpoint retained by [`super::MountedCall`].
    #[async_trait::async_trait]
    pub trait MountedCallDriver<C>: Send + Sync + 'static
    where
        C: TurnChannels,
    {
        fn call_id(&self) -> &DurableCallId;

        /// Observe the terminal durable result for the opaque call handle.
        ///
        /// The public [`super::MountedCall`] holds an exclusive mutable borrow while
        /// delegating here, so one handle cannot observe concurrently. Dropping this
        /// future must not cancel the underlying work or discard a later observation
        /// through the same handle.
        async fn wait(&self) -> Result<MountedCallOutcome<C>, MountedWaitError>;

        /// Request cancellation for this call only, then join its cleanup.
        async fn cancel(&self) -> MountedCallCancellation;
    }
}

/// Opaque durable mounted owner.
///
/// Construct this through an AgentView-owned mounted factory. The handle
/// intentionally has no access to the store, lease, revision, provider
/// request, or runtime actor used underneath.
#[must_use = "a mounted agent owns a durable session endpoint"]
pub struct MountedAgent<C, CallProps: ?Sized + 'static = (), Source: ?Sized + 'static = ()>
where
    C: TurnChannels,
{
    driver: Arc<dyn advanced::MountedAgentDriver<C, CallProps, Source>>,
}

impl<C, CallProps, Source> MountedAgent<C, CallProps, Source>
where
    C: TurnChannels,
    CallProps: ?Sized + 'static,
    Source: ?Sized + 'static,
{
    /// Open or reopen the durable owner represented by `definition`.
    ///
    /// The factory is an explicit advanced integration boundary. A successful
    /// return means it has either durably created the System epoch or rebound
    /// an existing epoch without executing its System POM projection.
    /// Compatibility open for a harness that has no host-provided bindings.
    ///
    /// New host integrations that bind pure provided-component contracts use
    /// [`Self::open_with_bindings`] through a factory's public
    /// `open_with_bindings` entry point.
    #[cfg(test)]
    pub(crate) async fn open<I, Capture, Factory>(
        factory: &Factory,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
    ) -> Result<Self, MountedOpenError>
    where
        I: Send + Sync + 'static,
        Capture: MountedTurnCapture<Transcript = I, CallProps = CallProps, Source = Source>,
        Factory: advanced::MountedAgentFactory<C, Capture, I>,
    {
        Self::open_with_bindings(factory, definition, MountedHostBindings::new()).await
    }

    pub(crate) async fn open_with_bindings<I, Capture, Factory>(
        factory: &Factory,
        definition: CapturedMountedHarnessDefinition<C, Capture, I>,
        bindings: MountedHostBindings<C, Capture::TurnProps>,
    ) -> Result<Self, MountedOpenError>
    where
        I: Send + Sync + 'static,
        Capture: MountedTurnCapture<Transcript = I, CallProps = CallProps, Source = Source>,
        Factory: advanced::MountedAgentFactory<C, Capture, I>,
    {
        let driver = factory.open(definition, bindings).await?;
        Ok(Self { driver })
    }

    /// Admit one owned durable call.
    ///
    /// This returns only after the advanced driver has durably accepted the
    /// call or determined that it was already settled. Waiting is optional and
    /// never controls ownership of the accepted work.
    pub async fn start(
        &self,
        input: MountedCallInput<CallProps, Source>,
    ) -> Result<MountedCall<C>, MountedStartError> {
        let driver = self.driver.start(input).await?;
        Ok(MountedCall { driver })
    }

    /// Read the current lifecycle observation for `call_id`.
    ///
    /// `None` means this mounted session has never admitted the id. The
    /// returned snapshot may become stale and is observational only; recovery and cancellation use
    /// separate mount-owned control operations so callers never receive a
    /// persistence lease or revision fence.
    pub async fn lookup(
        &self,
        call_id: &DurableCallId,
    ) -> Result<Option<MountedCallSnapshot>, MountedCallLookupError> {
        self.driver.lookup(call_id).await
    }

    /// Reattach through the admitting owner after a call's previous handle was
    /// dropped, or observe the durable state when that local actor is not
    /// available through this owner instance.
    ///
    /// A live actor has at most one attached [`MountedCall`] at a time. Calling
    /// this while that handle still exists returns `Observed` rather than a
    /// second handle.
    ///
    /// This method never recaptures User props and never starts provider,
    /// tool, reducer, Live, or publication work. Cross-process control requires
    /// the advanced fenced recovery controller; an `Observed` result grants no
    /// cancellation authority.
    pub async fn reattach(
        &self,
        call_id: &DurableCallId,
    ) -> Result<MountedCallReattachment<C>, MountedCallLookupError> {
        match self.driver.reattach(call_id).await? {
            advanced::MountedCallReattachmentDriver::NotFound => {
                Ok(MountedCallReattachment::NotFound)
            }
            advanced::MountedCallReattachmentDriver::Attached(driver) => {
                Ok(MountedCallReattachment::Attached {
                    call: MountedCall { driver },
                })
            }
            advanced::MountedCallReattachmentDriver::Observed(snapshot) => {
                Ok(MountedCallReattachment::Observed { snapshot })
            }
        }
    }

    /// Reload authoritative state and the provider cursor for this owner.
    ///
    /// A reload never causes another System render or System transport.
    pub async fn reload(&self) -> Result<(), MountedReloadError> {
        self.driver.reload().await
    }

    pub(crate) fn from_driver(
        driver: Arc<dyn advanced::MountedAgentDriver<C, CallProps, Source>>,
    ) -> Self {
        Self { driver }
    }

    /// Transfer the internal driver to an AgentView-owned factory.
    ///
    /// This stays crate-private so normal callers cannot replace the durable
    /// lifecycle owner after a successful open.
    pub(crate) fn into_driver(self) -> Arc<dyn advanced::MountedAgentDriver<C, CallProps, Source>> {
        self.driver
    }
}

impl<C, CallProps, Source> fmt::Debug for MountedAgent<C, CallProps, Source>
where
    C: TurnChannels,
    CallProps: ?Sized + 'static,
    Source: ?Sized + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedAgent")
            .finish_non_exhaustive()
    }
}

/// Opaque handle for one admitted mounted call.
#[must_use = "an admitted mounted call should be waited or explicitly cancelled"]
pub struct MountedCall<C>
where
    C: TurnChannels,
{
    driver: Arc<dyn advanced::MountedCallDriver<C>>,
}

impl<C> MountedCall<C>
where
    C: TurnChannels,
{
    /// Durable identity of this logical call.
    pub fn call_id(&self) -> &DurableCallId {
        self.driver.call_id()
    }

    /// Observe this call's terminal result once.
    ///
    /// The mutable borrow prevents concurrent observations through one handle.
    /// Dropping this future leaves both the durable owner and this handle intact;
    /// a later call to `wait` can resume the unconsumed terminal observation.
    pub async fn wait(&mut self) -> Result<MountedCallOutcome<C>, MountedWaitError> {
        self.driver.wait().await
    }

    /// Cancel exactly this call and wait for provider/attempt cleanup to join.
    pub async fn cancel(&self) -> MountedCallCancellation {
        self.driver.cancel().await
    }
}

impl<C> fmt::Debug for MountedCall<C>
where
    C: TurnChannels,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedCall")
            .field("call_id", self.call_id())
            .finish_non_exhaustive()
    }
}
