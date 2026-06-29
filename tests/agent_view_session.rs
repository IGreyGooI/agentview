use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentview::prelude::*;
use serde_json::json;
use tokio::time::timeout;

#[derive(Clone)]
struct CounterViewModel;

type CounterSource = Arc<Mutex<i32>>;

#[derive(Debug, Clone, PartialEq)]
struct CounterView {
    value: i32,
}

#[async_trait::async_trait]
impl PromptRenderable for CounterView {
    async fn render_full<'a>(
        &'a self,
        _templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        Ok(format!("counter = {}", self.value).into())
    }
}

#[async_trait::async_trait]
impl ContextView for CounterView {
    async fn render_delta<'a>(
        &'a self,
        prev: &'a Self,
        _templates: &'a TemplateEngine,
    ) -> anyhow::Result<Option<PromptFragment>> {
        if self == prev {
            Ok(None)
        } else {
            Ok(Some(
                format!("counter changed: {} -> {}", prev.value, self.value).into(),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CounterPrompt {
    text: String,
}

#[async_trait::async_trait]
impl PromptRenderable for CounterPrompt {
    async fn render_full<'a>(
        &'a self,
        _templates: &'a TemplateEngine,
    ) -> anyhow::Result<PromptFragment> {
        Ok(self.text.clone().into())
    }
}

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for CounterViewModel {
    type Source = CounterSource;
    type View = CounterView;
    type SystemPrompt = CounterPrompt;
    type TurnPrompt = CounterPrompt;
    type ContextState = ();

    async fn build_system_prompt(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Self::SystemPrompt> {
        Ok(CounterPrompt {
            text: "You control a counter.".to_owned(),
        })
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        CounterView {
            value: *source.lock().unwrap(),
        }
    }

    async fn build_turn_prompt(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        call_id: &str,
        task: String,
    ) -> anyhow::Result<Self::TurnPrompt> {
        Ok(CounterPrompt {
            text: format!("turn={call_id}; task={task}; reply with {{\"delta\": number}}"),
        })
    }

    async fn commit_turn(
        &self,
        _ctx: &mut PromptContext<Turn, Self::ContextState>,
        _request: &AgentTurnRequest<Turn>,
        _executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut (),
    ) -> anyhow::Result<TurnFlow> {
        Ok(TurnFlow::Wait)
    }
}

#[derive(Default)]
struct DeltaSink {
    delta: Option<i32>,
}

#[async_trait::async_trait]
impl TurnSink<ControlReply> for DeltaSink {
    type Output = Option<i32>;

    async fn on_event(&mut self, reply: ControlReply) {
        self.delta = reply
            .as_structured()
            .and_then(|value| value.get("delta"))
            .and_then(serde_json::Value::as_i64)
            .map(|delta| delta as i32);
    }

    async fn finish(self: Box<Self>) -> Self::Output {
        self.delta
    }
}

#[tokio::test]
async fn observe_returns_full_snapshot_with_turn_prompt() {
    let source = Arc::new(Mutex::new(0));
    let (mut session, _awake) = AgentViewSession::new(
        CounterViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    );

    let snapshot = session.observe("increment once").await.unwrap();

    assert_eq!(snapshot.view_epoch, 0);
    assert_eq!(snapshot.turn_id, "turn-1");
    assert_eq!(snapshot.view, CounterView { value: 0 });
    assert_eq!(
        snapshot.turn_prompt.text,
        "turn=turn-1; task=increment once; reply with {\"delta\": number}"
    );
}

#[tokio::test]
async fn act_applies_parsed_reply_and_returns_full_update() {
    let source = Arc::new(Mutex::new(0));
    let (mut session, _awake) = AgentViewSession::new(
        CounterViewModel,
        Arc::clone(&source),
        PromptContext::<Turn, ()>::without_system(),
    );
    let snapshot = session.observe("increment").await.unwrap();

    let update = session
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "delta": 3 })),
            DeltaSink::default(),
            |_, source, delta| {
                if let Some(delta) = delta {
                    *source.lock().unwrap() += delta;
                }
                Ok(())
            },
            "increment again",
        )
        .await
        .unwrap();

    assert_eq!(*source.lock().unwrap(), 3);
    assert_eq!(update.base_epoch, 0);
    assert_eq!(update.view_epoch, 1);
    let next = update.snapshot().unwrap();
    assert_eq!(next.turn_id, "turn-2");
    assert_eq!(next.view, CounterView { value: 3 });
}

#[tokio::test]
async fn act_rejects_stale_turn_id() {
    let source = Arc::new(Mutex::new(0));
    let (mut session, _awake) = AgentViewSession::new(
        CounterViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    );
    let _snapshot = session.observe("increment").await.unwrap();

    let err = session
        .act_with_sink(
            "turn-0",
            ControlReply::structured(json!({ "delta": 1 })),
            DeltaSink::default(),
            |_, source, delta| {
                if let Some(delta) = delta {
                    *source.lock().unwrap() += delta;
                }
                Ok(())
            },
            "increment again",
        )
        .await
        .unwrap_err();

    assert!(err.to_string().contains("stale turn id"));
}

#[tokio::test]
async fn hook_waits_for_app_awake_then_returns_full_snapshot() {
    let source = Arc::new(Mutex::new(0));
    let (mut session, awake) = AgentViewSession::new(
        CounterViewModel,
        Arc::clone(&source),
        PromptContext::<Turn, ()>::without_system(),
    );
    let snapshot = session.observe("watch").await.unwrap();

    tokio::spawn({
        let source = Arc::clone(&source);
        async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            *source.lock().unwrap() = 9;
            awake.awake();
        }
    });

    let next = timeout(
        Duration::from_secs(1),
        session.hook(snapshot.view_epoch, "watch again"),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(next.view_epoch, 1);
    assert_eq!(next.view, CounterView { value: 9 });
}
