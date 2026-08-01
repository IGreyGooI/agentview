//! Host-owned binding boundary for per-attempt Live runtimes.

use std::{convert::Infallible, error::Error, fmt};

use super::{
    DurableCallId, DurableCallInputId, DurableSessionId, EpochContractId, LiveEffectRuntime,
    ProviderAttemptIdentity, TurnChannels,
};

/// Read-only host inputs used to bind one fresh Live runtime.
///
/// This boundary runs after the final User snapshot has been selected and
/// before provider streaming starts. It deliberately lives outside the
/// functional component tree: reducers receive only `TurnProps`, while the
/// host may bind application services from `Source` and stable call metadata
/// into an owned runtime. The returned runtime must be `'static`, so it cannot
/// retain any of these borrowed values.
pub struct LiveEffectRuntimeBindingContext<'a, CallProps: ?Sized, Source: ?Sized, TurnProps: ?Sized>
{
    session_id: &'a DurableSessionId,
    epoch_contract_id: &'a EpochContractId,
    call_id: &'a DurableCallId,
    input_id: &'a DurableCallInputId,
    turn_index: u64,
    call_props: &'a CallProps,
    source: &'a Source,
    turn_props: &'a TurnProps,
    attempt: &'a ProviderAttemptIdentity,
}

impl<'a, CallProps, Source, TurnProps>
    LiveEffectRuntimeBindingContext<'a, CallProps, Source, TurnProps>
where
    CallProps: ?Sized,
    Source: ?Sized,
    TurnProps: ?Sized,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: &'a DurableSessionId,
        epoch_contract_id: &'a EpochContractId,
        call_id: &'a DurableCallId,
        input_id: &'a DurableCallInputId,
        turn_index: u64,
        call_props: &'a CallProps,
        source: &'a Source,
        turn_props: &'a TurnProps,
        attempt: &'a ProviderAttemptIdentity,
    ) -> Self {
        Self {
            session_id,
            epoch_contract_id,
            call_id,
            input_id,
            turn_index,
            call_props,
            source,
            turn_props,
            attempt,
        }
    }

    pub fn session_id(&self) -> &'a DurableSessionId {
        self.session_id
    }

    pub fn epoch_contract_id(&self) -> &'a EpochContractId {
        self.epoch_contract_id
    }

    pub fn call_id(&self) -> &'a DurableCallId {
        self.call_id
    }

    pub fn input_id(&self) -> &'a DurableCallInputId {
        self.input_id
    }

    pub fn turn_index(&self) -> u64 {
        self.turn_index
    }

    pub fn call_props(&self) -> &'a CallProps {
        self.call_props
    }

    pub fn source(&self) -> &'a Source {
        self.source
    }

    pub fn turn_props(&self) -> &'a TurnProps {
        self.turn_props
    }

    pub fn attempt(&self) -> &'a ProviderAttemptIdentity {
        self.attempt
    }
}

impl<CallProps, Source, TurnProps> fmt::Debug
    for LiveEffectRuntimeBindingContext<'_, CallProps, Source, TurnProps>
where
    CallProps: ?Sized,
    Source: ?Sized,
    TurnProps: ?Sized,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveEffectRuntimeBindingContext")
            .field("session_id", self.session_id)
            .field("epoch_contract_id", self.epoch_contract_id)
            .field("call_id", self.call_id)
            .field("input_id", self.input_id)
            .field("turn_index", &self.turn_index)
            .field("attempt", self.attempt)
            .finish_non_exhaustive()
    }
}

/// Host-owned factory for a fresh per-attempt [`LiveEffectRuntime`].
///
/// Component authors declare typed Live values; a mounted host implements this
/// trait to bind those values to application services. Binding is synchronous
/// and side-effect free. External I/O belongs in the returned runtime's
/// `apply` and `abort` methods.
pub trait LiveEffectRuntimeFactory<C, CallProps: ?Sized, Source: ?Sized, TurnProps: ?Sized>:
    Send + Sync + 'static
where
    C: TurnChannels,
{
    type Runtime: LiveEffectRuntime<C::Live>;
    type Error: Error + Send + Sync + 'static;

    fn bind(
        &self,
        context: LiveEffectRuntimeBindingContext<'_, CallProps, Source, TurnProps>,
    ) -> Result<Self::Runtime, Self::Error>;
}

/// Preserve the existing zero-argument factory shape for context-free hosts.
impl<C, CallProps, Source, TurnProps, Factory, Runtime>
    LiveEffectRuntimeFactory<C, CallProps, Source, TurnProps> for Factory
where
    C: TurnChannels,
    CallProps: ?Sized,
    Source: ?Sized,
    TurnProps: ?Sized,
    Factory: Fn() -> Runtime + Send + Sync + 'static,
    Runtime: LiveEffectRuntime<C::Live>,
{
    type Runtime = Runtime;
    type Error = Infallible;

    fn bind(
        &self,
        _context: LiveEffectRuntimeBindingContext<'_, CallProps, Source, TurnProps>,
    ) -> Result<Self::Runtime, Self::Error> {
        Ok(self())
    }
}
