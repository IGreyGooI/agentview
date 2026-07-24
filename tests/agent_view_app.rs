use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentview::prelude::*;
use serde_json::json;
use tokio::time::timeout;

#[derive(Clone)]
struct CounterViewModel;

type CounterSource = Arc<Mutex<i32>>;

#[derive(Debug, Clone, PartialEq, AgentView)]
#[agent_view(kind = "counter")]
struct CounterView {
    #[view(diff)]
    value: i32,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct CounterSystemParagraph {
    #[view(text)]
    instructions: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct CounterSystemDocument {
    #[view(block)]
    instructions: CounterSystemParagraph,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "counter_task")]
struct CounterTaskView {
    #[view(attr)]
    turn_id: String,

    #[view(element)]
    instruction: String,

    #[view(element)]
    reply_contract: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct CounterUserDocument {
    #[view(name = "agent_context", diff)]
    context: CounterView,

    #[view(xml)]
    task: CounterTaskView,
}

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for CounterViewModel {
    type Source = CounterSource;
    type View = CounterView;
    type ContextState = ();

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        Ok(CounterSystemDocument {
            instructions: CounterSystemParagraph {
                instructions: "You control a counter.",
            },
        }
        .build_root()?)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        CounterView {
            value: *source.lock().unwrap(),
        }
    }

    async fn build_user_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(CounterUserDocument {
            context: current_view.clone(),
            task: CounterTaskView {
                turn_id: call_id.to_owned(),
                instruction: task.to_string(),
                reply_contract: r#"{"delta": number}"#,
            },
        }
        .build_root()?)
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
async fn observe_returns_full_snapshot_with_user_document() {
    let source = Arc::new(Mutex::new(0));
    let (mut app, _awake) = AgentViewApp::new(
        CounterViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    );

    let snapshot = app.observe("increment once").await.unwrap();

    assert_eq!(snapshot.view_epoch, 0);
    assert_eq!(snapshot.turn_id, "turn-1");
    assert_eq!(snapshot.view, CounterView { value: 0 });
    assert_eq!(app.session().user_document_cursor().len(), 1);
    assert_eq!(
        render_pom_document(&snapshot.user_document).unwrap(),
        concat!(
            "<agent_context kind=\"counter\"><value>0</value></agent_context>\n\n",
            "<counter_task turn_id=\"turn-1\">",
            "<instruction>increment once</instruction>",
            "<reply_contract>{\"delta\": number}</reply_contract>",
            "</counter_task>"
        )
    );
}

#[tokio::test]
async fn render_failure_does_not_publish_cursor_or_turn_id() {
    let source = Arc::new(Mutex::new(0));
    let (mut app, _awake) = AgentViewApp::new(
        CounterViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    );

    let err = app.observe("invalid \u{0001} task").await.unwrap_err();

    assert!(
        err.to_string().contains("not allowed in XML content"),
        "unexpected error: {err:#}"
    );
    assert!(app.latest_turn_id().is_none());
    assert!(app.session().user_document_cursor().is_empty());

    let snapshot = app.observe("valid task").await.unwrap();
    assert_eq!(snapshot.turn_id, "turn-1");
}

#[tokio::test]
async fn act_applies_parsed_reply_and_returns_full_update() {
    let source = Arc::new(Mutex::new(0));
    let (mut app, _awake) = AgentViewApp::new(
        CounterViewModel,
        Arc::clone(&source),
        PromptContext::<Turn, ()>::without_system(),
    );
    let snapshot = app.observe("increment").await.unwrap();

    let update = app
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "delta": 3 })),
            DeltaSink::default(),
            |session: &mut AgentSession<Turn, ()>, source: &CounterSource, delta: Option<i32>| {
                if let Some(delta) = delta {
                    *source.lock().unwrap() += delta;
                }
                session.push_history(Turn::user("counter updated"));
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
    assert_eq!(app.session().user_document_cursor().len(), 1);
    let rendered = render_pom_document(&next.user_document).unwrap();
    assert!(rendered
        .starts_with("<agent_context rendering_mode=\"delta\" kind=\"counter\"><value>3</value>"));
    assert!(rendered.contains("<instruction>increment again</instruction>"));
    assert_eq!(app.session().context().history().len(), 1);
}

#[tokio::test]
async fn failed_next_snapshot_consumes_the_applied_turn_id() {
    let source = Arc::new(Mutex::new(0));
    let (mut app, _awake) = AgentViewApp::new(
        CounterViewModel,
        Arc::clone(&source),
        PromptContext::<Turn, ()>::without_system(),
    );
    let snapshot = app.observe("increment").await.unwrap();

    let err = app
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::structured(json!({ "delta": 1 })),
            DeltaSink::default(),
            |_, source, delta| {
                if let Some(delta) = delta {
                    *source.lock().unwrap() += delta;
                }
                Ok(())
            },
            "invalid \u{0001} task",
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("not allowed in XML content"),
        "unexpected error: {err:#}"
    );
    assert_eq!(*source.lock().unwrap(), 1);
    assert!(app.latest_turn_id().is_none());

    let replay_err = app
        .act_with_sink(
            &snapshot.turn_id,
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

    assert!(replay_err.to_string().contains("no active turn"));
    assert_eq!(*source.lock().unwrap(), 1);

    let recovery = app.observe("recover").await.unwrap();
    assert_eq!(recovery.turn_id, "turn-2");
    assert_eq!(recovery.view, CounterView { value: 1 });
}

#[tokio::test]
async fn act_rejects_stale_turn_id() {
    let source = Arc::new(Mutex::new(0));
    let (mut app, _awake) = AgentViewApp::new(
        CounterViewModel,
        source,
        PromptContext::<Turn, ()>::without_system(),
    );
    let _snapshot = app.observe("increment").await.unwrap();

    let err = app
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
    let (mut app, awake) = AgentViewApp::new(
        CounterViewModel,
        Arc::clone(&source),
        PromptContext::<Turn, ()>::without_system(),
    );
    let snapshot = app.observe("watch").await.unwrap();

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
        app.hook(snapshot.view_epoch, "watch again"),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(next.view_epoch, 1);
    assert_eq!(next.view, CounterView { value: 9 });
    assert_eq!(app.session().user_document_cursor().len(), 1);
    assert!(render_pom_document(&next.user_document)
        .unwrap()
        .contains("<value>9</value>"));
}

#[derive(Clone)]
struct RetryViewModel;

#[derive(Clone)]
struct RetrySource {
    items: Arc<Mutex<Vec<String>>>,
    race_once: Arc<AtomicBool>,
    awake: Arc<Mutex<Option<ViewAwakeHandle>>>,
}

#[derive(Debug, Clone, PartialEq, AgentView)]
#[agent_view(kind = "retry_context")]
struct RetryView {
    #[view(diff(append))]
    items: Vec<String>,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct RetryUserDocument {
    #[view(name = "agent_context", diff)]
    context: RetryView,
}

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for RetryViewModel {
    type Source = RetrySource;
    type View = RetryView;
    type ContextState = ();

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        Ok(Document::new(BlockChildren::new()))
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        let items = source.items.lock().unwrap().clone();
        if source.race_once.swap(false, Ordering::SeqCst) {
            source.items.lock().unwrap().push("c".to_owned());
            source
                .awake
                .lock()
                .unwrap()
                .as_ref()
                .expect("test installs the awake handle")
                .awake();
        }
        RetryView { items }
    }

    async fn build_user_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _call_id: &str,
        _task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(RetryUserDocument {
            context: current_view.clone(),
        }
        .build_root()?)
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

#[tokio::test]
async fn epoch_retry_does_not_commit_the_discarded_candidate_cursor() {
    let source = RetrySource {
        items: Arc::new(Mutex::new(vec!["a".to_owned()])),
        race_once: Arc::new(AtomicBool::new(false)),
        awake: Arc::new(Mutex::new(None)),
    };
    let (mut app, awake) = AgentViewApp::new(
        RetryViewModel,
        source.clone(),
        PromptContext::<Turn, ()>::without_system(),
    );
    *source.awake.lock().unwrap() = Some(awake);

    app.observe("seed").await.unwrap();
    *source.items.lock().unwrap() = vec!["a".to_owned(), "b".to_owned()];
    source.race_once.store(true, Ordering::SeqCst);

    let snapshot = app.observe("retry").await.unwrap();
    let rendered = render_pom_document(&snapshot.user_document).unwrap();

    assert_eq!(snapshot.turn_id, "turn-2");
    assert_eq!(
        snapshot.view.items,
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
    );
    assert_eq!(rendered.matches("<insert>").count(), 2, "{rendered}");
    assert!(rendered.contains("<insert><item>b</item></insert>"));
    assert!(rendered.contains("<insert><item>c</item></insert>"));
}
