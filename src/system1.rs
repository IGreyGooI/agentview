//! Independent, one-reaction structured judgments using the Decision Component.
#![allow(
    clippy::disallowed_types,
    reason = "short synchronous result staging; no guard crosses an await"
)]

use std::sync::{Arc, Mutex};

use crate::component::prelude::view;
use crate::component::{
    authoring::{Decision, DecisionQuestionSpec},
    execution::{
        Application, ApplicationFault, DecisionFailure, DecisionSource, FrameRevision,
        QuestionOutcome, ReactionOutcome, ReactionPort,
    },
    ApplicationId, EventContext, ReactionId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgmentOrigin {
    pub application_id: ApplicationId,
    pub reaction_id: ReactionId,
    pub frame_revision: Option<FrameRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgment<T> {
    pub value: T,
    pub source: DecisionSource,
    pub origin: JudgmentOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JudgmentErrorKind {
    #[error("judgment failed: {0}")]
    Decision(DecisionFailure),
    #[error("judgment runtime failed")]
    Application,
    #[error("judgment reaction was interrupted")]
    Interrupted,
    #[error("judgment reaction exited before completing")]
    Exited,
    #[error("judgment result protocol is invalid")]
    Protocol,
    #[error("judgment runtime could not shut down cleanly")]
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("judgment failed: {kind}")]
pub struct JudgmentError {
    pub origin: Option<JudgmentOrigin>,
    pub kind: JudgmentErrorKind,
    pub application_fault: Option<ApplicationFault>,
}

/// Run one Decision in a fresh, independent Application and await cleanup.
///
/// `provider` must be a fresh session supporting the decision capability. No
/// caller history is imported. Success is published only after the entire
/// reaction and consuming shutdown succeed; no confirmation reaction runs.
pub async fn judge<P, Q>(
    provider: P,
    state: serde_json::Value,
    question: Q,
) -> Result<Judgment<Q::Answer>, JudgmentError>
where
    P: ReactionPort + 'static,
    Q: DecisionQuestionSpec,
{
    let state_view = state.to_string();
    let staged = Arc::new(Mutex::new(
        None::<(EventContext, QuestionOutcome<Q::Answer>)>,
    ));
    let output = staged.clone();
    let root = move || {
        let output = output.clone();
        let decision = Decision::new("judgment", question.clone())
            .on_result(move |context, outcome| {
                *output.lock().unwrap_or_else(|p| p.into_inner()) = Some((context, outcome));
                Ok::<(), std::convert::Infallible>(())
            })
            .build();
        view! {
            #[user]
            { state_view.clone() }
            { decision }
        }
    };
    let app = Application::mount(root, provider).map_err(|fault| JudgmentError {
        origin: None,
        kind: JudgmentErrorKind::Application,
        application_fault: Some(fault),
    })?;
    let application_id = app.application_id();
    let mut owner = JudgmentOwner {
        application: Some(app),
        runtime: tokio::runtime::Handle::current(),
    };
    let reaction = match owner
        .application
        .as_mut()
        .expect("owned application")
        .react(false)
    {
        Ok(reaction) => reaction,
        Err(fault) => {
            let _ = owner.shutdown().await;
            return Err(JudgmentError {
                origin: None,
                kind: JudgmentErrorKind::Application,
                application_fault: Some(fault),
            });
        }
    };
    let mut origin = JudgmentOrigin {
        application_id,
        reaction_id: reaction.id(),
        frame_revision: None,
    };
    let outcome = reaction.await;
    let cleanup = owner.shutdown().await;
    let pending = staged.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some((context, _)) = &pending {
        if context.application_id() == origin.application_id
            && context.reaction_id() == Some(origin.reaction_id)
        {
            origin.frame_revision = context.frame_revision();
        }
    }
    let error = |kind, fault| JudgmentError {
        origin: Some(origin.clone()),
        kind,
        application_fault: fault,
    };
    match outcome {
        Err(fault) => return Err(error(JudgmentErrorKind::Application, Some(fault))),
        Ok(ReactionOutcome::Interrupted) => {
            return Err(error(JudgmentErrorKind::Interrupted, None))
        }
        Ok(ReactionOutcome::Exited(_)) => return Err(error(JudgmentErrorKind::Exited, None)),
        Ok(ReactionOutcome::Completed) => {}
    }
    cleanup.map_err(|fault| error(JudgmentErrorKind::Shutdown, Some(fault)))?;
    let (context, result) = pending.ok_or_else(|| error(JudgmentErrorKind::Protocol, None))?;
    if context.application_id() != application_id
        || context.reaction_id() != Some(origin.reaction_id)
    {
        return Err(error(JudgmentErrorKind::Protocol, None));
    }
    let value = result
        .result
        .map_err(|failure| error(JudgmentErrorKind::Decision(failure), None))?;
    Ok(Judgment {
        value,
        source: result.source,
        origin,
    })
}

/// Cancelling the caller transfers the unique Application into its consuming
/// cleanup immediately. Dropping the cleanup waiter does not cancel its owner.
struct JudgmentOwner<P: ReactionPort + 'static> {
    application: Option<Application<P>>,
    runtime: tokio::runtime::Handle,
}

impl<P: ReactionPort + 'static> JudgmentOwner<P> {
    async fn shutdown(mut self) -> Result<(), ApplicationFault> {
        self.application
            .take()
            .expect("owned application")
            .shutdown()
            .await
    }
}

impl<P: ReactionPort + 'static> Drop for JudgmentOwner<P> {
    fn drop(&mut self) {
        if let Some(application) = self.application.take() {
            let _entered = self.runtime.enter();
            drop(application.shutdown());
        }
    }
}
