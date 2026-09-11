use std::{
    num::{NonZeroU128, NonZeroU64},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;

#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "the debug provider retains its feature-gated ProviderPort compatibility implementation"
)]
use super::{
    prompt_render::render_projection_prompt, ProviderEventStream, ProviderFault, ProviderFaultCode,
    ProviderPort, RenderedProjection,
};
use crate::component::execution::reaction::{
    Frame, FrameBasis, FrameCapabilities, FrameConstraints, FrameProfile, FrameRevision,
    ProviderFact, ProviderFactStream, ReactionPort, ReactionPortFault, ReactionPortFaultCode,
    ReactionPortFaultReason, SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
};

static NEXT_DEBUG_TARGET_ID: AtomicU64 = AtomicU64::new(1);
const DEBUG_TARGET_ID_DOMAIN: u128 = 2_u128 << 64;
const DEBUG_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const DEBUG_MAX_COMPONENT_BYTES: usize = 4 * 1024 * 1024;

fn next_debug_target_identity(counter: &AtomicU64) -> Result<TargetIdentity, ReactionPortFault> {
    let instance = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| debug_declaration_fault())?;
    Ok(TargetIdentity::new(
        NonZeroU128::new(DEBUG_TARGET_ID_DOMAIN | u128::from(instance))
            .expect("Debug target domain is non-zero"),
    ))
}

fn debug_frame_profile() -> FrameProfile {
    FrameProfile::new(
        FrameConstraints {
            max_frame_bytes: DEBUG_MAX_FRAME_BYTES,
            max_component_bytes: DEBUG_MAX_COMPONENT_BYTES,
            context_window_tokens: None,
            reserved_output_tokens: None,
        },
        FrameCapabilities::new(true),
    )
}

/// Captured provider-neutral prompts from a [`DebugProviderPort`].
#[derive(Clone, Default)]
pub struct DebugPromptCapture {
    snapshots: Arc<Mutex<Vec<String>>>,
    frames: Arc<Mutex<Vec<DebugFrameSnapshot>>>,
}

impl DebugPromptCapture {
    pub fn snapshots(&self) -> Vec<String> {
        self.snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn latest(&self) -> Option<String> {
        self.snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }

    /// Exact canonical Frames accepted through the structured ReactionPort.
    pub fn frame_snapshots(&self) -> Vec<DebugFrameSnapshot> {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn latest_frame(&self) -> Option<DebugFrameSnapshot> {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }
}

/// One exact structured Frame accepted by [`DebugProviderPort`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugFrameSnapshot {
    revision: FrameRevision,
    basis: FrameBasis,
    canonical_payload: Vec<u8>,
}

impl DebugFrameSnapshot {
    pub const fn revision(&self) -> FrameRevision {
        self.revision
    }

    pub const fn basis(&self) -> FrameBasis {
        self.basis
    }

    pub fn canonical_payload(&self) -> &[u8] {
        &self.canonical_payload
    }
}

/// ProviderPort for inspecting the complete provider-neutral prompt projection.
///
/// It performs no model I/O and returns normal EOF after capturing one prompt.
pub struct DebugProviderPort {
    capture: DebugPromptCapture,
    target: Result<TargetDeclaration, ReactionPortFault>,
    execution_mode: DebugExecutionMode,
}

impl DebugProviderPort {
    pub fn new() -> (Self, DebugPromptCapture) {
        let capture = DebugPromptCapture::default();
        (
            Self {
                capture: capture.clone(),
                target: next_debug_target_identity(&NEXT_DEBUG_TARGET_ID).map(|identity| {
                    TargetDeclaration::full(
                        identity,
                        TargetEpoch::new(NonZeroU64::MIN),
                        debug_frame_profile(),
                    )
                }),
                execution_mode: DebugExecutionMode::Unclaimed,
            },
            capture,
        )
    }
}

impl Default for DebugProviderPort {
    fn default() -> Self {
        Self::new().0
    }
}

#[async_trait]
#[cfg(feature = "legacy-provider-port")]
#[allow(
    deprecated,
    reason = "this impl preserves DebugProviderPort compatibility for ProviderPort callers"
)]
impl ProviderPort for DebugProviderPort {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        if matches!(self.execution_mode, DebugExecutionMode::FrameNative) {
            return Err(debug_legacy_mode_fault());
        }
        let prompt = render_projection_prompt(&projection)?;
        self.execution_mode = DebugExecutionMode::Legacy;
        self.capture
            .snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(prompt);
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[async_trait]
impl ReactionPort for DebugProviderPort {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        #[cfg(feature = "legacy-provider-port")]
        {
            if matches!(self.execution_mode, DebugExecutionMode::Legacy) {
                return Err(debug_declaration_fault());
            }
        }
        self.target.clone()
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        let declaration = self.target.clone().map_err(SubmitFault::Rejected)?;
        frame.check_handoff_precondition(&declaration)?;
        match self.execution_mode {
            DebugExecutionMode::Unclaimed => {
                self.execution_mode = DebugExecutionMode::FrameNative;
            }
            DebugExecutionMode::FrameNative => {}
            #[cfg(feature = "legacy-provider-port")]
            DebugExecutionMode::Legacy => {
                return Err(SubmitFault::Rejected(debug_declaration_fault()));
            }
        }
        let revision = frame.revision();
        let snapshot = DebugFrameSnapshot {
            revision,
            basis: frame.basis(),
            canonical_payload: frame.submission().canonical_bytes().to_vec(),
        };
        self.capture
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(snapshot);
        self.target = Ok(TargetDeclaration::resume(
            revision,
            declaration.profile().clone(),
        ));

        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderFact::ReactionCompleted { primary_text: None })
        })))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugExecutionMode {
    Unclaimed,
    #[cfg(feature = "legacy-provider-port")]
    Legacy,
    FrameNative,
}

fn debug_declaration_fault() -> ReactionPortFault {
    ReactionPortFault::terminal(
        ReactionPortFaultCode::Internal,
        ReactionPortFaultReason::Declaration,
    )
}

#[cfg(feature = "legacy-provider-port")]
fn debug_legacy_mode_fault() -> ProviderFault {
    ProviderFault::model_rejected("Debug provider is already using the Frame-native protocol")
        .with_code(ProviderFaultCode::RequestPreparation)
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use agentview_derive::view;

    use super::*;
    use crate::component::execution::{
        application::Application,
        reaction::{FrameSubmission, ProjectionSubmission, ToolCatalog},
    };
    #[cfg(feature = "legacy-provider-port")]
    use crate::component::execution::{RenderedProjection, RenderedProjectionNode};

    fn frame(declaration: &TargetDeclaration, sequence: u64) -> Frame {
        let epoch = declaration.continuity().epoch();
        let revision = FrameRevision::new(
            NonZeroU128::new(811).unwrap(),
            declaration.identity(),
            epoch,
            NonZeroU64::new(sequence).unwrap(),
        );
        let basis = declaration
            .continuity()
            .accepted_revision()
            .map(FrameBasis::DeltaFrom)
            .unwrap_or(FrameBasis::Full);
        Frame::from_compiled(
            revision,
            declaration.identity(),
            epoch,
            declaration.continuity().clone(),
            declaration.profile().clone(),
            basis,
            FrameSubmission::from_compiled(
                Vec::new(),
                Vec::new(),
                ProjectionSubmission::new(Vec::new()),
                ToolCatalog::new(Vec::new()).unwrap(),
                b"debug-canonical-frame".to_vec(),
            ),
        )
        .unwrap()
    }

    #[cfg(feature = "legacy-provider-port")]
    fn empty_projection() -> RenderedProjection {
        RenderedProjection::from_nodes(vec![RenderedProjectionNode::new("root", Vec::new())])
            .unwrap()
    }

    #[tokio::test]
    async fn structured_debug_port_captures_exact_full_then_delta_frames() {
        let (port, capture) = DebugProviderPort::new();
        let mut application =
            Application::mount(|| view! { debug { "exact frame" } }, port).unwrap();

        assert!(application.react().await.unwrap().is_continue());
        assert!(application.react().await.unwrap().is_continue());

        let snapshots = capture.frame_snapshots();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].basis(), FrameBasis::Full);
        assert!(matches!(snapshots[1].basis(), FrameBasis::DeltaFrom(_)));
        for snapshot in snapshots {
            let payload: serde_json::Value =
                serde_json::from_slice(snapshot.canonical_payload()).unwrap();
            assert_eq!(payload["version"], 1);
            assert!(payload.get("component").is_some());
            assert!(payload.get("replay").is_some());
            assert!(payload.get("staged_inputs").is_some());
        }
    }

    #[test]
    fn debug_target_profile_is_finite_stable_and_domain_separated() {
        let (mut first, _) = DebugProviderPort::new();
        let (mut second, _) = DebugProviderPort::new();
        let first_declaration = ReactionPort::declare(&mut first).unwrap();
        let second_declaration = ReactionPort::declare(&mut second).unwrap();

        assert_eq!(
            first_declaration.profile().constraints,
            FrameConstraints {
                max_frame_bytes: DEBUG_MAX_FRAME_BYTES,
                max_component_bytes: DEBUG_MAX_COMPONENT_BYTES,
                context_window_tokens: None,
                reserved_output_tokens: None,
            }
        );
        assert_eq!(
            ReactionPort::declare(&mut first).unwrap(),
            first_declaration
        );
        assert_ne!(first_declaration.identity(), second_declaration.identity());
        assert_eq!(
            first_declaration.identity().get().get() >> 64,
            DEBUG_TARGET_ID_DOMAIN >> 64
        );
    }

    #[test]
    fn debug_identity_exhaustion_is_a_stable_typed_declaration_fault() {
        let exhausted = AtomicU64::new(u64::MAX);

        for _ in 0..2 {
            let fault = next_debug_target_identity(&exhausted).unwrap_err();
            assert_eq!(
                fault.kind(),
                crate::component::execution::reaction::ReactionPortFaultKind::Terminal
            );
            assert_eq!(fault.code(), ReactionPortFaultCode::Internal);
            assert_eq!(fault.reason(), ReactionPortFaultReason::Declaration);
        }
    }

    #[tokio::test]
    async fn unpolled_submit_has_no_handoff_side_effect() {
        let (mut port, capture) = DebugProviderPort::new();
        let declaration = ReactionPort::declare(&mut port).unwrap();
        let submit = Box::pin(ReactionPort::submit(&mut port, frame(&declaration, 1)));

        drop(submit);

        assert!(capture.frame_snapshots().is_empty());
        assert_eq!(port.execution_mode, DebugExecutionMode::Unclaimed);
        assert_eq!(ReactionPort::declare(&mut port).unwrap(), declaration);
    }

    #[tokio::test]
    async fn first_submit_poll_captures_and_accepts_in_the_same_poll() {
        let (mut port, capture) = DebugProviderPort::new();
        let declaration = ReactionPort::declare(&mut port).unwrap();
        let expected_revision = frame(&declaration, 1).revision();
        let mut submit = Box::pin(ReactionPort::submit(&mut port, frame(&declaration, 1)));

        let stream = match futures::poll!(submit.as_mut()) {
            Poll::Ready(Ok(stream)) => stream,
            Poll::Ready(Err(error)) => panic!("Debug submit failed: {error:?}"),
            Poll::Pending => panic!("Debug crossing poll returned Pending"),
        };
        assert_eq!(capture.frame_snapshots().len(), 1);
        drop(stream);
        drop(submit);

        assert_eq!(port.execution_mode, DebugExecutionMode::FrameNative);
        assert_eq!(
            ReactionPort::declare(&mut port)
                .unwrap()
                .continuity()
                .accepted_revision(),
            Some(expected_revision)
        );
    }

    #[tokio::test]
    async fn stale_preconditions_do_not_capture_or_claim_native_mode() {
        let (mut port, capture) = DebugProviderPort::new();
        let initial = ReactionPort::declare(&mut port).unwrap();
        let stale = frame(&initial, 1);
        port.target = Ok(TargetDeclaration::full(
            initial.identity(),
            TargetEpoch::new(NonZeroU64::new(2).unwrap()),
            initial.profile().clone(),
        ));

        assert!(matches!(
            ReactionPort::submit(&mut port, stale).await,
            Err(SubmitFault::ContinuityChanged)
        ));
        assert!(capture.frame_snapshots().is_empty());
        assert_eq!(port.execution_mode, DebugExecutionMode::Unclaimed);

        let current = ReactionPort::declare(&mut port).unwrap();
        let stale_profile = frame(&current, 2);
        let mut changed_profile = current.profile().clone();
        changed_profile.constraints.max_frame_bytes += 1;
        port.target = Ok(TargetDeclaration::full(
            current.identity(),
            current.continuity().epoch(),
            changed_profile,
        ));
        assert!(matches!(
            ReactionPort::submit(&mut port, stale_profile).await,
            Err(SubmitFault::ProfileChanged)
        ));
        assert!(capture.frame_snapshots().is_empty());
        assert_eq!(port.execution_mode, DebugExecutionMode::Unclaimed);
    }

    #[tokio::test]
    #[cfg(feature = "legacy-provider-port")]
    #[allow(
        deprecated,
        reason = "this mode-fence test intentionally crosses legacy ProviderPort and native ReactionPort"
    )]
    async fn legacy_and_native_debug_modes_are_mutually_exclusive() {
        let (mut legacy_first, _) = DebugProviderPort::new();
        let legacy_stream = ProviderPort::execute(&mut legacy_first, empty_projection())
            .await
            .unwrap();
        drop(legacy_stream);
        assert_eq!(legacy_first.execution_mode, DebugExecutionMode::Legacy);
        let native_fault = ReactionPort::declare(&mut legacy_first).unwrap_err();
        assert_eq!(native_fault.reason(), ReactionPortFaultReason::Declaration);

        let (mut native_first, _) = DebugProviderPort::new();
        let declaration = ReactionPort::declare(&mut native_first).unwrap();
        let native_stream = ReactionPort::submit(&mut native_first, frame(&declaration, 1))
            .await
            .unwrap();
        drop(native_stream);
        assert_eq!(native_first.execution_mode, DebugExecutionMode::FrameNative);
        let legacy_fault = match ProviderPort::execute(&mut native_first, empty_projection()).await
        {
            Err(fault) => fault,
            Ok(_) => panic!("legacy Debug mode was accepted after native handoff"),
        };
        assert_eq!(legacy_fault.code(), ProviderFaultCode::RequestPreparation);
    }
}
