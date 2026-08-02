//! Harness binding for externally supplied replies.
//!
//! An [`ExternalReply`] keeps the reply grammar that appears in the durable
//! System POM together with its stable decoder/action contract. It is selected
//! only while a generic prompt component is finalized as an external harness;
//! the component itself remains reusable by other harness runtimes.

use std::{fmt, sync::Arc};

use crate::agent_view::AgentView;

use super::external::{ExternalReplyContract, ExternalReplyContractId};
use super::{
    durable_system, pom_view, ComponentError, EpochContractId, MountedHarnessDefinition,
    NoTurnChannels, PomView, PromptComponent,
};

/// Pure reply-contract bundle selected by one external harness.
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
    /// Construct one reply binding from its grammar-owning pure contract.
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
    definition: Arc<MountedHarnessDefinition<NoTurnChannels, Props>>,
    contract: Contract,
}

impl<Props, Contract> MountedExternalHarnessDefinition<Props, Contract>
where
    Props: ?Sized + 'static,
    Contract: ExternalReplyContract,
{
    /// Select external control while consuming one complete generic component
    /// root as a harness definition.
    ///
    /// The component authoring type has no external mode or finalizer. This
    /// advanced harness boundary appends the reply grammar to the retained
    /// System tree before the epoch contract is finalized, then binds that
    /// grammar to its decoder and action route.
    pub fn new(
        component: PromptComponent<Props>,
        reply: ExternalReply<Contract>,
        epoch_contract_id: EpochContractId,
    ) -> Self {
        let component =
            component.append_harness_system(durable_system::<NoTurnChannels, Props>(reply.system));
        Self {
            definition: Arc::new(component.into_harness(epoch_contract_id)),
            contract: reply.contract,
        }
    }

    /// Stable durable epoch identity selected by the harness author.
    pub fn epoch_contract_id(&self) -> &EpochContractId {
        self.definition.epoch().epoch_contract_id()
    }

    /// Stable semantic identity of the reply grammar and decoder.
    pub fn reply_contract_id(&self) -> &ExternalReplyContractId {
        self.contract.contract_id()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Arc<MountedHarnessDefinition<NoTurnChannels, Props>>,
        Contract,
    ) {
        (self.definition, self.contract)
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
