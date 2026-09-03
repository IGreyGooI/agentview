use std::{
    collections::VecDeque,
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use agentview::{
    component::execution::{
        Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
        ProviderFact, ProviderFactStream, ProviderOutputKey, ReactionPort, ReactionPortFault,
        ReactionPortFaultCode, ReactionPortFaultReason, SubmitFault, TargetDeclaration,
        TargetEpoch, TargetIdentity,
    },
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;

static NEXT_SCRIPTED_TARGET: AtomicU64 = AtomicU64::new(1);
const SCRIPTED_TARGET_DOMAIN: u128 = 4_u128 << 64;
const TEXT_OUTPUT: ProviderOutputKey = ProviderOutputKey::new(1);

#[derive(Debug, Clone)]
pub struct AcceptedFrame {
    pub basis: FrameBasis,
    pub text: String,
}

#[derive(Clone, Default)]
pub struct ScriptedCapture {
    frames: Arc<Mutex<Vec<AcceptedFrame>>>,
    accepted: Arc<Mutex<Option<FrameRevision>>>,
}

impl ScriptedCapture {
    pub fn frames(&self) -> Vec<AcceptedFrame> {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn submission_count(&self) -> usize {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    pub fn accepted_revision(&self) -> Option<FrameRevision> {
        *self
            .accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub struct ScriptedReaction {
    facts: Vec<ProviderFact>,
}

impl ScriptedReaction {
    pub fn text(deltas: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let deltas = deltas.into_iter().map(Into::into).collect::<Vec<String>>();
        let sealed = deltas.concat();
        Self::text_with_seal(deltas, sealed)
    }

    #[cfg(test)]
    pub fn sealed(text: impl Into<String>) -> Self {
        Self::text_with_seal(std::iter::empty::<String>(), text.into())
    }

    #[cfg(test)]
    pub fn mismatched_text_seal(
        deltas: impl IntoIterator<Item = impl Into<String>>,
        sealed: impl Into<String>,
    ) -> Self {
        Self::text_with_seal(deltas, sealed)
    }

    fn text_with_seal(
        deltas: impl IntoIterator<Item = impl Into<String>>,
        sealed: impl Into<String>,
    ) -> Self {
        let mut facts = deltas
            .into_iter()
            .map(|delta| ProviderFact::TextDelta {
                output: TEXT_OUTPUT,
                phase: None,
                delta: delta.into(),
            })
            .collect::<Vec<_>>();
        facts.push(ProviderFact::TextSealed {
            output: TEXT_OUTPUT,
            phase: None,
            text: sealed.into(),
        });
        facts.push(ProviderFact::ReactionCompleted {
            primary_text: Some(TEXT_OUTPUT),
        });
        Self { facts }
    }

    pub fn empty() -> Self {
        Self {
            facts: vec![ProviderFact::ReactionCompleted { primary_text: None }],
        }
    }
}

pub struct ScriptedProvider {
    identity: TargetIdentity,
    epoch: TargetEpoch,
    profile: FrameProfile,
    scripts: VecDeque<ScriptedReaction>,
    capture: ScriptedCapture,
    terminal_fault: Option<ReactionPortFault>,
    #[cfg(test)]
    fail_next_capture: bool,
}

impl ScriptedProvider {
    pub fn new(
        scripts: impl IntoIterator<Item = ScriptedReaction>,
    ) -> Result<(Self, ScriptedCapture), ReactionPortFault> {
        let identity = next_scripted_target_identity(&NEXT_SCRIPTED_TARGET)?;
        let capture = ScriptedCapture::default();
        Ok((
            Self {
                identity,
                epoch: TargetEpoch::new(NonZeroU64::MIN),
                profile: FrameProfile::new(
                    FrameConstraints {
                        max_frame_bytes: 1024 * 1024,
                        max_component_bytes: 256 * 1024,
                        context_window_tokens: None,
                        reserved_output_tokens: None,
                    },
                    FrameCapabilities::new(true),
                ),
                scripts: scripts.into_iter().collect(),
                capture: capture.clone(),
                terminal_fault: None,
                #[cfg(test)]
                fail_next_capture: false,
            },
            capture,
        ))
    }

    fn declaration(&self) -> Result<TargetDeclaration, ReactionPortFault> {
        if let Some(fault) = self.terminal_fault {
            return Err(fault);
        }
        let accepted = *self
            .capture
            .accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(match accepted {
            Some(revision) => TargetDeclaration::resume(revision, self.profile.clone()),
            None => TargetDeclaration::full(self.identity, self.epoch, self.profile.clone()),
        })
    }

    #[cfg(test)]
    fn new_with_retryable_capture_failure(
        scripts: impl IntoIterator<Item = ScriptedReaction>,
    ) -> Result<(Self, ScriptedCapture), ReactionPortFault> {
        let (mut provider, capture) = Self::new(scripts)?;
        provider.fail_next_capture = true;
        Ok((provider, capture))
    }
}

#[async_trait]
impl ReactionPort for ScriptedProvider {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        self.declaration()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let declaration = self.declaration()?;
        frame.check_handoff_precondition(&declaration)?;
        if self.scripts.is_empty() {
            return Err(SubmitFault::Rejected(ReactionPortFault::retryable(
                ReactionPortFaultCode::Unavailable,
                ReactionPortFaultReason::Other,
            )));
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_capture) {
            return Err(SubmitFault::Rejected(ReactionPortFault::retryable(
                ReactionPortFaultCode::Unavailable,
                ReactionPortFaultReason::RequestPreparation,
            )));
        }
        let revision = frame.revision();
        let text = match frame
            .submission()
            .projection()
            .items()
            .iter()
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom)),
                _ => None,
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(documents) => documents.join("\n"),
            Err(_) => {
                let fault = ReactionPortFault::terminal(
                    ReactionPortFaultCode::Internal,
                    ReactionPortFaultReason::RequestPreparation,
                );
                self.terminal_fault = Some(fault);
                return Err(SubmitFault::Rejected(fault));
            }
        };

        let script = self
            .scripts
            .pop_front()
            .expect("script availability checked before capture");
        self.capture
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(AcceptedFrame {
                basis: frame.basis(),
                text,
            });
        *self
            .capture
            .accepted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(revision);
        Ok(Box::pin(futures::stream::iter(
            script.facts.into_iter().map(Ok),
        )))
    }
}

fn next_scripted_target_identity(counter: &AtomicU64) -> Result<TargetIdentity, ReactionPortFault> {
    let instance = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| declaration_fault())?;
    Ok(TargetIdentity::new(
        NonZeroU128::new(SCRIPTED_TARGET_DOMAIN | u128::from(instance))
            .ok_or_else(declaration_fault)?,
    ))
}

fn declaration_fault() -> ReactionPortFault {
    ReactionPortFault::terminal(
        ReactionPortFaultCode::Internal,
        ReactionPortFaultReason::Declaration,
    )
}

#[cfg(test)]
mod tests {
    use agentview::component::{
        execution::{
            Application, ApplicationFaultKind, ApplicationFaultReason, ApplicationFaultStage,
        },
        prelude::*,
    };

    use super::*;

    #[derive(Clone)]
    struct CaptureRetryProps {
        completions: Arc<Mutex<Vec<String>>>,
    }

    #[component]
    fn capture_retry_application(props: CaptureRetryProps) -> Component {
        let completions = Arc::clone(&props.completions);
        use_provider_event_handler(ProviderEvent::TEXT, move |event| {
            let completions = Arc::clone(&completions);
            async move {
                if let TextTurnEvent::TextComplete(text) = event {
                    completions
                        .lock()
                        .expect("completion capture lock")
                        .push(text);
                }
                Ok::<(), std::convert::Infallible>(())
            }
        });
        view! {
            capture_retry { "stable projection" }
        }
    }

    #[component]
    fn unrenderable_capture_application() -> Component {
        view! {
            unrenderable_capture { "bad\u{0001}value" }
        }
    }

    #[tokio::test]
    async fn retryable_capture_failure_preserves_script_capture_and_declaration() {
        let completions = Arc::new(Mutex::new(Vec::new()));
        let props = CaptureRetryProps {
            completions: Arc::clone(&completions),
        };
        let (provider, capture) = ScriptedProvider::new_with_retryable_capture_failure([
            ScriptedReaction::text(["first-script"]),
            ScriptedReaction::text(["second-script"]),
        ])
        .unwrap();
        let mut application =
            Application::mount(move || capture_retry_application(props.clone()), provider).unwrap();

        let fault = application.react().await.unwrap_err();
        assert_eq!(fault.stage(), ApplicationFaultStage::Submit);
        assert_eq!(fault.kind(), ApplicationFaultKind::Retryable);
        assert_eq!(
            fault.reason(),
            ApplicationFaultReason::Port(ReactionPortFaultReason::RequestPreparation)
        );
        assert_eq!(capture.submission_count(), 0);
        assert_eq!(capture.accepted_revision(), None);
        assert!(completions.lock().unwrap().is_empty());

        application.react().await.unwrap();
        assert_eq!(capture.submission_count(), 1);
        assert_eq!(capture.frames()[0].basis, FrameBasis::Full);
        assert!(capture.accepted_revision().is_some());
        assert_eq!(
            *completions.lock().unwrap(),
            vec![String::from("first-script")]
        );

        application.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn deterministic_render_failure_is_terminal_and_sticky() {
        let (provider, capture) =
            ScriptedProvider::new([ScriptedReaction::text(["must-not-run"])]).unwrap();
        let mut application =
            Application::mount(unrenderable_capture_application, provider).unwrap();

        let submit_fault = application.react().await.unwrap_err();
        assert_eq!(submit_fault.stage(), ApplicationFaultStage::Submit);
        assert_eq!(submit_fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(
            submit_fault.reason(),
            ApplicationFaultReason::Port(ReactionPortFaultReason::RequestPreparation)
        );
        assert_eq!(capture.submission_count(), 0);
        assert_eq!(capture.accepted_revision(), None);

        let declaration_fault = application.react().await.unwrap_err();
        assert_eq!(
            declaration_fault.stage(),
            ApplicationFaultStage::Declaration
        );
        assert_eq!(declaration_fault.kind(), ApplicationFaultKind::Terminal);
        assert_eq!(declaration_fault.reason(), submit_fault.reason());
        assert_eq!(capture.submission_count(), 0);
        assert_eq!(capture.accepted_revision(), None);

        application.shutdown().await.unwrap();
    }

    #[test]
    fn scripted_instances_are_unique_and_use_the_reserved_high_domain() {
        let (mut first, _) = ScriptedProvider::new(std::iter::empty()).unwrap();
        let (mut second, _) = ScriptedProvider::new(std::iter::empty()).unwrap();
        let first_identity = first.declare().unwrap().identity();
        let second_identity = second.declare().unwrap().identity();
        let domain = first_identity.get().get() >> 64;

        assert_ne!(first_identity, second_identity);
        const CHAT_DOMAIN: u128 = 1;
        const DEBUG_DOMAIN: u128 = 2;
        const EXTERNAL_DOMAIN: u128 = 3;

        assert_eq!(domain, 4);
        assert_ne!(domain, CHAT_DOMAIN);
        assert_ne!(domain, DEBUG_DOMAIN);
        assert_ne!(domain, EXTERNAL_DOMAIN);
        assert_eq!(second_identity.get().get() >> 64, 4);
    }

    #[test]
    fn scripted_allocator_exhaustion_returns_typed_declaration_fault() {
        let exhausted = AtomicU64::new(u64::MAX);
        let fault = next_scripted_target_identity(&exhausted).unwrap_err();

        assert_eq!(
            fault.kind(),
            agentview::component::execution::ReactionPortFaultKind::Terminal
        );
        assert_eq!(fault.code(), ReactionPortFaultCode::Internal);
        assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
    }
}
