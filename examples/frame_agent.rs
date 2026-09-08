use std::{ops::ControlFlow, sync::Arc};

use agentview::component::{
    execution::{Application, ApplicationFault, DebugPromptCapture, DebugProviderPort, ExitReason},
    prelude::*,
};
use tokio::sync::Notify;

#[cfg(test)]
use agentview::component::execution::FrameBasis;

#[cfg(test)]
#[path = "support/frame_workflow_golden.rs"]
mod frame_workflow_golden;

#[derive(Clone)]
struct AgentProps {
    release: Arc<Notify>,
}

#[component]
fn frame_agent_component(props: AgentProps) -> Component {
    let state = use_signal(|| String::from("initial"));
    let request = use_reaction_request();
    let release = Arc::clone(&props.release);
    let update = state.clone();
    use_future(move || async move {
        release.notified().await;
        update
            .set(String::from("published"))
            .expect("mounted Signal update");
        request.request().expect("mounted reaction request");
    });
    let value = state.with(Clone::clone).expect("mounted Signal read");
    view! { frame_agent { "{value}" } }
}

async fn run_agent() -> Result<ControlFlow<ExitReason, DebugPromptCapture>, ApplicationFault> {
    let release = Arc::new(Notify::new());
    let root_release = Arc::clone(&release);
    let (port, capture) = DebugProviderPort::new();
    let mut application = Application::mount(
        move || {
            frame_agent_component(AgentProps {
                release: Arc::clone(&root_release),
            })
        },
        port,
    )?;

    let result = async {
        match application.react().await? {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(reason) => return Ok(ControlFlow::Break(reason)),
        }
        release.notify_one();
        application.wait_for_reaction_request().await?;
        application.react().await
    }
    .await;
    let shutdown = application.shutdown().await;
    let flow = result?;
    shutdown?;
    Ok(match flow {
        ControlFlow::Continue(()) => ControlFlow::Continue(capture),
        ControlFlow::Break(reason) => ControlFlow::Break(reason),
    })
}

#[tokio::main]
async fn main() {
    match run_agent().await {
        Ok(ControlFlow::Continue(_)) | Ok(ControlFlow::Break(_)) => {}
        Err(error) => {
            eprintln!("frame agent failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_agent_driver_matches_the_shared_exact_first_frame_golden() {
        let (port, capture) = DebugProviderPort::new();
        let mut application =
            Application::mount(frame_workflow_golden::shared_frame_workflow_root, port).unwrap();

        assert_eq!(
            application.react().await.unwrap(),
            ControlFlow::Continue(())
        );
        let frame = capture.latest_frame().unwrap();
        let basis = frame.basis();
        let payload = frame.canonical_payload().to_vec();
        application.shutdown().await.unwrap();

        assert_eq!(basis, FrameBasis::Full);
        frame_workflow_golden::assert_exact_first_frame(&payload);
    }

    #[tokio::test]
    async fn component_future_requests_a_second_delta_reaction_after_publishing_state() {
        let capture = match run_agent().await.unwrap() {
            ControlFlow::Continue(capture) => capture,
            ControlFlow::Break(reason) => panic!("frame agent exited early: {reason:?}"),
        };
        let frames = capture.frame_snapshots();

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].basis(), FrameBasis::Full);
        assert!(matches!(frames[1].basis(), FrameBasis::DeltaFrom(_)));
        assert!(String::from_utf8(frames[1].canonical_payload().to_vec())
            .unwrap()
            .contains("published"));
    }
}
