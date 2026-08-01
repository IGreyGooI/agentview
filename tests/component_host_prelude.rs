//! External-consumer proof for the mounted host import surface.

use agentview::component::host::prelude::*;

type PromptAgent = MountedAgent<NoTurnChannels, str, str>;
type PromptCall = MountedCall<NoTurnChannels>;

fn accepts_host_types(
    _agent: Option<PromptAgent>,
    _call: Option<PromptCall>,
    _snapshot: Option<MountedCallSnapshot>,
) {
}

fn requires_capture<C: MountedTurnCapture>() {}

fn requires_reducer<R>()
where
    R: SessionReducer<String, (), NoTurnChannels>,
{
}

fn requires_live_runtime<R: LiveEffectRuntime<Never>>() {}

#[test]
fn mounted_host_prelude_is_distinct_and_self_contained() {
    accepts_host_types(None, None, None);
    let _ = DurableSessionId::new("test/component-host-prelude").unwrap();
    let _ = DurableCallId::new("test-call").unwrap();
    let _ = EpochContractId::new("test/component-host-prelude/v1").unwrap();

    let _ = requires_capture::<NeverCapture>;
    let _ = requires_reducer::<NeverReducer>;
    let _ = requires_live_runtime::<NoLiveEffects>;
}

struct NeverCapture;

#[async_trait::async_trait]
impl MountedTurnCapture for NeverCapture {
    type Transcript = String;
    type ContextState = ();
    type CallProps = ();
    type TurnProps = ();
    type Source = ();
    type Error = std::convert::Infallible;

    async fn capture_turn_props(
        &self,
        _context: TurnCaptureContext<'_, String, (), (), ()>,
    ) -> Result<Self::TurnProps, Self::Error> {
        Ok(())
    }
}

struct NeverReducer;

impl SessionReducer<String, (), NoTurnChannels> for NeverReducer {
    type Error = std::convert::Infallible;

    fn reduce(
        &self,
        _session: &mut AgentSession<String, ()>,
        _context: SessionReduceContext<'_, String, NoTurnChannels>,
        _executor_commit: ExecutorCommit<String>,
    ) -> Result<TurnFlow, Self::Error> {
        Ok(TurnFlow::Wait)
    }
}
