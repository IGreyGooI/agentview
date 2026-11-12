//! Deterministic request scripts and typed captures for the offline example.

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
        ContextWindow, Frame, FrameCapabilities, FrameConstraints, FrameProfile, ProviderFact,
        ProviderFactStream, ProviderOutputKey, ProviderToolCall, ReactionPort, ReactionPortFault,
        SubmitFault, TargetDeclaration, TargetEpoch, TargetIdentity,
    },
    transcript::CanonicalInputItem,
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::Notify;

static NEXT_TARGET: AtomicU64 = AtomicU64::new(100_000);
const OUTPUT: ProviderOutputKey = ProviderOutputKey::new(1);

pub enum Script {
    Completed,
    Pending,
    Adjust(i64),
}

#[derive(Default)]
pub struct Capture {
    scripts: Mutex<VecDeque<Script>>,
    pub frames: Mutex<Vec<Frame>>,
    pub submitted: Notify,
}

impl Capture {
    pub async fn wait_for_frames(&self, count: usize) {
        loop {
            let submitted = self.submitted.notified();
            tokio::pin!(submitted);
            submitted.as_mut().enable();
            if self.frames.lock().unwrap().len() >= count {
                return;
            }
            submitted.await;
        }
    }

    pub fn staged_results(&self, frame: usize) -> Vec<CanonicalInputItem> {
        self.frames.lock().unwrap()[frame]
            .submission()
            .staged_inputs()
            .to_vec()
    }
}

pub struct Port {
    declaration: TargetDeclaration,
    capture: Arc<Capture>,
}

impl Port {
    pub fn scripted(scripts: impl IntoIterator<Item = Script>) -> (Self, Arc<Capture>) {
        let capture = Arc::new(Capture {
            scripts: Mutex::new(scripts.into_iter().collect()),
            ..Capture::default()
        });
        let profile = FrameProfile::new(
            FrameConstraints {
                max_frame_bytes: 1_048_576,
                max_component_bytes: 262_144,
                context_window: ContextWindow::NotApplicable,
                reserved_output_tokens: None,
            },
            FrameCapabilities::new(true)
                .with_native_tool_calls(true)
                .with_text_delta_output(true),
        );
        (
            Self {
                declaration: declaration(profile),
                capture: Arc::clone(&capture),
            },
            capture,
        )
    }
}

fn declaration(profile: FrameProfile) -> TargetDeclaration {
    TargetDeclaration::full(
        TargetIdentity::new(
            NonZeroU128::new(u128::from(NEXT_TARGET.fetch_add(1, Ordering::Relaxed))).unwrap(),
        ),
        TargetEpoch::new(NonZeroU64::MIN),
        profile,
    )
}

#[async_trait]
impl ReactionPort for Port {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault> {
        Ok(self.declaration.clone())
    }

    fn fork_temporary(&mut self) -> Result<Box<dyn ReactionPort>, ReactionPortFault> {
        Ok(Box::new(Self {
            declaration: declaration(self.declaration.profile().clone()),
            capture: Arc::clone(&self.capture),
        }))
    }

    async fn submit<'a>(&'a mut self, frame: Frame) -> Result<ProviderFactStream<'a>, SubmitFault> {
        frame.check_handoff_precondition(&self.declaration)?;
        let script = self.capture.scripts.lock().unwrap().pop_front().unwrap();
        self.declaration =
            TargetDeclaration::resume(frame.revision(), frame.prepared_profile().clone());
        self.capture.frames.lock().unwrap().push(frame);
        self.capture.submitted.notify_waiters();
        let mut facts = Vec::new();
        if let Script::Adjust(amount) = script {
            facts.push(ProviderFact::ToolCall {
                output: OUTPUT,
                ordinal: 1,
                call: ProviderToolCall::new(
                    format!("adjust-{amount}"),
                    "adjust",
                    serde_json::json!({"amount": amount}).to_string(),
                )
                .unwrap(),
            });
        }
        if !matches!(script, Script::Pending) {
            facts.push(ProviderFact::ReactionCompleted { primary_text: None });
        }
        let pending = usize::from(matches!(script, Script::Pending));
        Ok(Box::pin(
            futures::stream::iter(facts.into_iter().map(Ok))
                .chain(futures::stream::pending().take(pending)),
        ))
    }
}
