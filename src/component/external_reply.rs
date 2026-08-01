//! Pure prompt and decoder declaration for externally supplied replies.
//!
//! An [`ExternalReply`] is a provided component rather than a host runtime.
//! It keeps the reply grammar that appears in the durable System POM together
//! with the stable decoder/action contract. Binding it to a prompt component
//! constructs a single harness value that the external controller can open;
//! callers cannot separately select a grammar or decoder for the same POM.

use std::{fmt, sync::Arc};

use crate::agent_view::AgentView;
use crate::StorageString;

use super::external::{ExternalFrameKind, ExternalReplyContract, ExternalReplyContractId};
use super::{
    durable_system, pom_view, user_view, ComponentError, DurableEpochDefinition, DurableSystem,
    EpochContractId, NoTurnChannels, PomChildren, PomView, UserTurnContext, UserView,
};

/// Pure per-turn result for an external component.
///
/// The POM document and reply ingress are authored together. `Actionable` binds
/// the harness's one [`ExternalReplyContract`] for this frame; `Passive` emits a
/// presentation with no action token. This metadata is compiled beside POM and
/// never serialized into the prompt text.
#[must_use]
pub struct ExternalUserView {
    kind: ExternalFrameKind,
    user: PomView,
}

impl ExternalUserView {
    pub fn actionable<User>(user: User) -> Result<Self, crate::pom::PomError>
    where
        User: AgentView<Root = crate::pom::Document>,
    {
        Ok(Self::actionable_pom(user.build_root()?))
    }

    pub fn actionable_pom(user: impl PomChildren) -> Self {
        Self {
            kind: ExternalFrameKind::Actionable,
            user: pom_view(user),
        }
    }

    /// A status/UI presentation that does not enter the model prompt lane and
    /// therefore cannot advance that lane's delta baseline.
    pub fn passive_presentation<User>(user: User) -> Result<Self, crate::pom::PomError>
    where
        User: AgentView<Root = crate::pom::Document>,
    {
        Ok(Self::passive_presentation_pom(user.build_root()?))
    }

    pub fn passive_presentation_pom(user: impl PomChildren) -> Self {
        Self {
            kind: ExternalFrameKind::Passive,
            user: pom_view(user),
        }
    }

    pub fn kind(&self) -> ExternalFrameKind {
        self.kind
    }

    pub(crate) fn into_parts(self) -> (ExternalFrameKind, UserView) {
        (self.kind, user_view(self.user))
    }
}

type ExternalUserRenderer<Props> = dyn for<'a> Fn(UserTurnContext<'a, Props>) -> Result<ExternalUserView, ComponentError>
    + Send
    + Sync
    + 'static;

/// Pure System/User authoring root for an externally driven component.
///
/// Unlike [`super::PromptComponent`], its User projection also declares
/// whether the current frame mounts the harness's external reply ingress.
#[must_use]
pub struct ExternalPromptComponent<Props: ?Sized + 'static> {
    durable_system: DurableSystem<NoTurnChannels, Props>,
    render_user: Arc<ExternalUserRenderer<Props>>,
}

impl<Props> ExternalPromptComponent<Props>
where
    Props: ?Sized + 'static,
{
    pub(crate) fn with_component_scope(mut self, name: impl Into<StorageString>) -> Self {
        let name = name.into();
        self.durable_system = self.durable_system.with_component_scope(name.clone());
        let render_user = Arc::clone(&self.render_user);
        self.render_user = Arc::new(move |context| {
            let ExternalUserView { kind, user } = render_user(context)?;
            Ok(ExternalUserView {
                kind,
                user: user.with_component_scope(name.clone(), None),
            })
        });
        self
    }

    pub(crate) fn from_error(error: ComponentError) -> Self {
        Self {
            durable_system: DurableSystem::from_error(error),
            render_user: Arc::new(|_| Ok(ExternalUserView::passive_presentation_pom(()))),
        }
    }
}

/// Build an external component whose per-turn projection is pure and
/// infallible.
pub fn external_prompt_component<Props, System>(
    system: System,
    render_user: impl Fn(&Props) -> ExternalUserView + Send + Sync + 'static,
) -> ExternalPromptComponent<Props>
where
    Props: ?Sized + 'static,
    System: AgentView<Root = crate::pom::Document>,
{
    let durable_system = match system.build_root() {
        Ok(document) => durable_system(document),
        Err(error) => DurableSystem::from_error(error.into()),
    };
    ExternalPromptComponent {
        durable_system,
        render_user: Arc::new(move |context| Ok(render_user(context.props()))),
    }
}

/// Fallible counterpart to [`external_prompt_component`].
pub fn try_external_prompt_component<Props, System, Error>(
    system: System,
    render_user: impl Fn(&Props) -> Result<ExternalUserView, Error> + Send + Sync + 'static,
) -> ExternalPromptComponent<Props>
where
    Props: ?Sized + 'static,
    System: AgentView<Root = crate::pom::Document>,
    Error: Into<ComponentError>,
{
    let durable_system = match system.build_root() {
        Ok(document) => durable_system(document),
        Err(error) => DurableSystem::from_error(error.into()),
    };
    ExternalPromptComponent {
        durable_system,
        render_user: Arc::new(move |context| render_user(context.props()).map_err(Into::into)),
    }
}

pub(crate) struct ExternalHarnessRuntime<Props: ?Sized + 'static> {
    epoch: DurableEpochDefinition<NoTurnChannels, Props>,
    render_user: Arc<ExternalUserRenderer<Props>>,
}

impl<Props> ExternalHarnessRuntime<Props>
where
    Props: ?Sized + 'static,
{
    pub(crate) fn epoch(&self) -> &DurableEpochDefinition<NoTurnChannels, Props> {
        &self.epoch
    }

    pub(crate) fn render_user(
        &self,
        context: UserTurnContext<'_, Props>,
    ) -> Result<ExternalUserView, ComponentError> {
        (self.render_user)(context)
    }
}

/// Pure provided component for one externally supplied reply grammar.
///
/// `System` is rendered only if the host wins durable epoch creation. The
/// decoder is synchronous and pure; domain mutation, reply delivery, and
/// persistence remain responsibilities of the external host port. An external
/// harness has exactly one reply ingress; a grammar with several commands
/// should decode them into one typed action enum rather than attach multiple
/// independent reply components.
#[must_use]
pub struct ExternalReply<Contract>
where
    Contract: ExternalReplyContract,
{
    system: PomView,
    contract: Contract,
}

impl<Contract> ExternalReply<Contract>
where
    Contract: ExternalReplyContract,
{
    /// Construct one reply component from its grammar-owning pure contract.
    ///
    /// The contract id covers both the prompt-facing grammar and the decoder
    /// semantics. Change it whenever either could cause an in-flight reply to
    /// be interpreted differently. `system` is intentionally obtained from
    /// the contract rather than accepted as a second argument, so the binding
    /// site cannot accidentally pair this decoder with unrelated grammar.
    pub fn new(contract: Contract) -> Self {
        let system = match contract.system().build_root() {
            Ok(document) => pom_view(document),
            Err(error) => PomView::from_error(ComponentError::from(error)),
        };
        Self { system, contract }
    }

    /// Stable semantic identity of this grammar and decoder pair.
    pub fn contract_id(&self) -> &ExternalReplyContractId {
        self.contract.contract_id()
    }

    /// Attach this provided component to an external prompt component.
    ///
    /// The reply grammar is appended to the feature's retained System tree in
    /// author order. The resulting value is the only input accepted by
    /// [`super::external::MountedExternalController::open`].
    pub fn into_harness<Props>(
        self,
        component: ExternalPromptComponent<Props>,
        epoch_contract_id: EpochContractId,
    ) -> MountedExternalHarnessDefinition<Props, Contract>
    where
        Props: ?Sized + 'static,
    {
        let durable_system = durable_system((
            component.durable_system,
            durable_system::<NoTurnChannels, Props>(self.system),
        ));
        MountedExternalHarnessDefinition {
            runtime: Arc::new(ExternalHarnessRuntime {
                epoch: DurableEpochDefinition::new(epoch_contract_id, durable_system),
                render_user: component.render_user,
            }),
            contract: self.contract,
        }
    }
}

impl<Contract> fmt::Debug for ExternalReply<Contract>
where
    Contract: ExternalReplyContract,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalReply")
            .field("contract_id", self.contract_id())
            .finish_non_exhaustive()
    }
}

/// One prompt-only mounted harness whose external reply grammar and decoder
/// are bound together.
///
/// This is intentionally consumed by the advanced external controller. It
/// exposes no host capability and performs no I/O by itself.
#[must_use]
pub struct MountedExternalHarnessDefinition<Props: ?Sized + 'static, Contract>
where
    Contract: ExternalReplyContract,
{
    runtime: Arc<ExternalHarnessRuntime<Props>>,
    contract: Contract,
}

impl<Props, Contract> MountedExternalHarnessDefinition<Props, Contract>
where
    Props: ?Sized + 'static,
    Contract: ExternalReplyContract,
{
    /// Stable durable epoch identity selected by the harness author.
    pub fn epoch_contract_id(&self) -> &EpochContractId {
        self.runtime.epoch().epoch_contract_id()
    }

    /// Stable semantic identity of the reply grammar and decoder.
    pub fn reply_contract_id(&self) -> &ExternalReplyContractId {
        self.contract.contract_id()
    }

    pub(crate) fn into_parts(self) -> (Arc<ExternalHarnessRuntime<Props>>, Contract) {
        (self.runtime, self.contract)
    }
}

impl<Props, Contract> fmt::Debug for MountedExternalHarnessDefinition<Props, Contract>
where
    Props: ?Sized + 'static,
    Contract: ExternalReplyContract,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MountedExternalHarnessDefinition")
            .field("epoch_contract_id", self.epoch_contract_id())
            .field("reply_contract_id", self.reply_contract_id())
            .finish_non_exhaustive()
    }
}
