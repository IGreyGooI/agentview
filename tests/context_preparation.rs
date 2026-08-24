use std::sync::Arc;

use agentview::prelude::*;
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
struct TestViewModel;

#[derive(Debug, Clone, PartialEq, Eq, AgentView)]
#[agent_view(kind = "test_view")]
struct TestView {
    #[view(diff)]
    value: usize,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct TestParagraph {
    #[view(text)]
    text: String,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct TestSystemDocument {
    #[view(block)]
    instructions: TestParagraph,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct TestUserDocument {
    #[view(name = "agent_context", diff)]
    context: TestView,

    #[view(block)]
    task: Option<TestParagraph>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TestContextState {
    marker: usize,
}

#[async_trait::async_trait]
impl AgentViewModel<Turn, ()> for TestViewModel {
    type Source = usize;
    type View = TestView;
    type ContextState = TestContextState;

    async fn build_system_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _source: &Self::Source,
    ) -> anyhow::Result<Document> {
        Ok(TestSystemDocument {
            instructions: TestParagraph {
                text: "system".to_owned(),
            },
        }
        .build_root()?)
    }

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        TestView { value: *source }
    }

    async fn build_user_document(
        &self,
        _ctx: &PromptContext<Turn, Self::ContextState>,
        _call_id: &str,
        task: StorageString,
        current_view: &Self::View,
    ) -> anyhow::Result<Document> {
        Ok(TestUserDocument {
            context: current_view.clone(),
            task: (!task.is_empty()).then(|| TestParagraph {
                text: task.to_string(),
            }),
        }
        .build_root()?)
    }

    async fn commit_turn(
        &self,
        ctx: &mut PromptContext<Turn, Self::ContextState>,
        request: &AgentTurnRequest<Turn>,
        executor_commit: ExecutorCommit<Turn>,
        _sink_output: &mut (),
    ) -> anyhow::Result<TurnFlow> {
        ctx.push_history(Turn::user(request.user.clone()));
        if request.call_id.as_ref() == "commit-fails" {
            anyhow::bail!("commit failed");
        }
        ctx.extend_history(executor_commit.append);
        Ok(TurnFlow::Wait)
    }
}

#[derive(Debug, Default)]
struct ExecutorState {
    prepare_calls: usize,
    replacement_calls: usize,
    execute_calls: usize,
    committed_history_lens: Vec<usize>,
    executed_requests: Vec<AgentTurnRequest<Turn>>,
}

#[derive(Clone)]
struct ReplacingExecutor {
    state: Arc<Mutex<ExecutorState>>,
    always_replace: bool,
}

impl ReplacingExecutor {
    fn replace_once() -> Self {
        Self {
            state: Arc::new(Mutex::new(ExecutorState::default())),
            always_replace: false,
        }
    }

    fn always_replace() -> Self {
        Self {
            state: Arc::new(Mutex::new(ExecutorState::default())),
            always_replace: true,
        }
    }
}

#[async_trait::async_trait]
impl LLMExecutor<Turn, TextTurnEvent> for ReplacingExecutor {
    async fn prepare_context(
        &self,
        request: &AgentTurnRequest<Turn>,
        budget: ContextPreparationBudget,
    ) -> anyhow::Result<ContextPreparation<Turn>> {
        let mut state = self.state.lock().await;
        state.prepare_calls += 1;
        state
            .committed_history_lens
            .push(budget.committed_history_len());

        let prepares_test_call = matches!(request.call_id.as_ref(), "test" | "provider-fails");
        if prepares_test_call && (self.always_replace || state.prepare_calls == 1) {
            if !budget.can_replace() {
                anyhow::bail!(
                    "agent turn `{}` exceeded context preparation replacement limit {}",
                    request.call_id,
                    budget.max_replacements()
                );
            }
            state.replacement_calls += 1;
            return Ok(ContextPreparation::ReplaceHistory {
                history: vec![Turn::user(format!("summary {}", state.prepare_calls))],
            });
        }

        Ok(ContextPreparation::Ready)
    }

    async fn execute_llm<S>(
        &self,
        request: AgentTurnRequest<Turn>,
        _sink: &mut S,
        _side_sinks: &mut Vec<Box<dyn TurnSink<TextTurnEvent, Output = ()>>>,
    ) -> anyhow::Result<ExecutorCommit<Turn>>
    where
        S: TurnSink<TextTurnEvent> + Send,
    {
        let mut state = self.state.lock().await;
        state.execute_calls += 1;
        state.executed_requests.push(request);
        if state
            .executed_requests
            .last()
            .is_some_and(|request| request.call_id.as_ref() == "provider-fails")
        {
            anyhow::bail!("provider failed");
        }
        Ok(ExecutorCommit::text("done"))
    }
}

fn test_agent<E>() -> Agent<TestViewModel, E, Turn, TextTurnEvent, ()>
where
    E: LLMExecutor<Turn, TextTurnEvent> + Clone,
{
    let mut ctx = PromptContext::<Turn, TestContextState>::new("system");
    ctx.push_history(Turn::user("old history"));
    ctx.push_working_set(Turn::user("working context"));
    ctx.context_state_mut().marker = 7;

    Agent::with_view(TestViewModel, "model", 64, ctx)
}

fn committed_context_xml(session: &AgentSession<Turn, TestContextState>) -> String {
    let role = XmlName::try_from("agent_context").unwrap();
    let value = session
        .user_document_cursor()
        .value(&role)
        .expect("a successful turn commits the agent_context slot")
        .clone();
    render_pom_document(&resolve_system_document(Document::from_xml(value))).unwrap()
}

#[tokio::test]
async fn replacement_reprepares_same_turn_with_full_view() {
    let executor = ReplacingExecutor::replace_once();
    let agent = test_agent::<ReplacingExecutor>();

    agent.call("seed").execute(&1, &executor).await.unwrap();
    *executor.state.lock().await = ExecutorState::default();

    agent
        .call("test")
        .with_user("continue")
        .with_max_context_preparations(3)
        .execute(&1, &executor)
        .await
        .unwrap();

    let state = executor.state.lock().await;
    assert_eq!(state.prepare_calls, 2);
    assert_eq!(state.execute_calls, 1);
    assert_eq!(state.committed_history_lens, vec![3, 1]);
    assert_eq!(state.executed_requests.len(), 1);
    assert_eq!(state.executed_requests[0].history.len(), 2);
    assert_eq!(&*state.executed_requests[0].history[0].text, "summary 1");
    assert_eq!(
        &*state.executed_requests[0].history[1].text,
        "working context"
    );
    assert!(state.executed_requests[0].user.contains("<agent_context"));
    assert!(state.executed_requests[0].user.contains("<value>1</value>"));
    drop(state);

    let session = agent.session().await;
    assert_eq!(&*session.context().history()[0].text, "summary 1");
    assert_eq!(&*session.context().working_set()[0].text, "working context");
    assert_eq!(session.context().context_state().marker, 7);
    assert!(committed_context_xml(&session).contains("<value>1</value>"));
}

#[tokio::test]
async fn unchanged_slot_is_omitted_while_current_task_is_still_sent() {
    let executor = ReplacingExecutor::replace_once();
    let agent = test_agent::<ReplacingExecutor>();

    agent.call("seed").execute(&1, &executor).await.unwrap();
    *executor.state.lock().await = ExecutorState::default();

    agent
        .call("unchanged")
        .with_user("continue")
        .execute(&1, &executor)
        .await
        .unwrap();

    let state = executor.state.lock().await;
    assert_eq!(state.executed_requests.len(), 1);
    assert_eq!(state.executed_requests[0].user, "continue");
    drop(state);

    let session = agent.session().await;
    assert!(committed_context_xml(&session).contains("<value>1</value>"));
}

#[tokio::test]
async fn fork_clones_the_slot_baseline_and_emits_a_delta_on_change() {
    let executor = ReplacingExecutor::replace_once();
    let agent = test_agent::<ReplacingExecutor>();
    agent.call("seed").execute(&1, &executor).await.unwrap();

    let fork = agent.forked().await;
    *executor.state.lock().await = ExecutorState::default();
    fork.call("fork-delta")
        .with_user("continue")
        .execute(&2, &executor)
        .await
        .unwrap();

    let state = executor.state.lock().await;
    let user = &state.executed_requests[0].user;
    assert!(user.starts_with("<agent_context rendering_mode=\"delta\" kind=\"test_view\">"));
    assert!(user.contains("<value>2</value>"));
    drop(state);

    let original_session = agent.session().await;
    assert!(committed_context_xml(&original_session).contains("<value>1</value>"));
    drop(original_session);
    let fork_session = fork.session().await;
    assert!(committed_context_xml(&fork_session).contains("<value>2</value>"));
}

#[tokio::test]
async fn replacement_limit_stops_before_fourth_rewrite() {
    let executor = ReplacingExecutor::always_replace();
    let agent = test_agent::<ReplacingExecutor>();

    agent.call("seed").execute(&1, &executor).await.unwrap();
    *executor.state.lock().await = ExecutorState::default();

    let error = agent
        .call("test")
        .with_user("continue")
        .with_max_context_preparations(10)
        .execute(&1, &executor)
        .await
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("preparation replacement limit 3"));

    let state = executor.state.lock().await;
    assert_eq!(state.prepare_calls, 4);
    assert_eq!(state.replacement_calls, 3);
    assert_eq!(state.execute_calls, 0);
    drop(state);

    let session = agent.session().await;
    assert_eq!(session.context().history().len(), 3);
    assert_eq!(&*session.context().history()[0].text, "old history");
    assert_eq!(session.context().context_state().marker, 7);
    assert!(committed_context_xml(&session).contains("<value>1</value>"));
}

#[tokio::test]
async fn provider_failure_discards_prepared_session_draft() {
    let executor = ReplacingExecutor::replace_once();
    let agent = test_agent::<ReplacingExecutor>();
    agent.call("seed").execute(&1, &executor).await.unwrap();
    *executor.state.lock().await = ExecutorState::default();

    let error = agent
        .call("provider-fails")
        .execute(&2, &executor)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("provider failed"));
    let session = agent.session().await;
    assert_eq!(session.context().history().len(), 3);
    assert_eq!(&*session.context().history()[0].text, "old history");
    assert_eq!(session.context().context_state().marker, 7);
    assert!(committed_context_xml(&session).contains("<value>1</value>"));
}

#[tokio::test]
async fn commit_failure_discards_mutated_session_draft() {
    let executor = ReplacingExecutor::replace_once();
    let agent = test_agent::<ReplacingExecutor>();
    agent.call("seed").execute(&1, &executor).await.unwrap();
    *executor.state.lock().await = ExecutorState::default();

    let error = agent
        .call("commit-fails")
        .execute(&2, &executor)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("commit failed"));
    let session = agent.session().await;
    assert_eq!(session.context().history().len(), 3);
    assert_eq!(&*session.context().history()[0].text, "old history");
    assert_eq!(session.context().context_state().marker, 7);
    assert!(committed_context_xml(&session).contains("<value>1</value>"));
}

#[derive(Debug, Default)]
struct SerialExecutorState {
    active: usize,
    max_active: usize,
}

#[derive(Clone, Default)]
struct SerialExecutor {
    state: Arc<Mutex<SerialExecutorState>>,
}

#[async_trait::async_trait]
impl LLMExecutor<Turn, TextTurnEvent> for SerialExecutor {
    async fn execute_llm<S>(
        &self,
        _request: AgentTurnRequest<Turn>,
        _sink: &mut S,
        _side_sinks: &mut Vec<Box<dyn TurnSink<TextTurnEvent, Output = ()>>>,
    ) -> anyhow::Result<ExecutorCommit<Turn>>
    where
        S: TurnSink<TextTurnEvent> + Send,
    {
        {
            let mut state = self.state.lock().await;
            state.active += 1;
            state.max_active = state.max_active.max(state.active);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        self.state.lock().await.active -= 1;
        Ok(ExecutorCommit::text("done"))
    }
}

#[tokio::test]
async fn turns_on_the_same_agent_are_serialized() {
    let executor = SerialExecutor::default();
    let agent = Arc::new(test_agent::<SerialExecutor>());

    let first = {
        let agent = agent.clone();
        let executor = executor.clone();
        async move { agent.call("first").execute(&1, &executor).await }
    };
    let second = {
        let agent = agent.clone();
        let executor = executor.clone();
        async move { agent.call("second").execute(&1, &executor).await }
    };

    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();

    assert_eq!(executor.state.lock().await.max_active, 1);
}

#[derive(Clone, Default)]
struct BlockingExecutor {
    started: Arc<Notify>,
    release: Arc<Notify>,
    prepare_calls: Arc<Mutex<usize>>,
}

#[async_trait::async_trait]
impl LLMExecutor<Turn, TextTurnEvent> for BlockingExecutor {
    async fn prepare_context(
        &self,
        _request: &AgentTurnRequest<Turn>,
        _budget: ContextPreparationBudget,
    ) -> anyhow::Result<ContextPreparation<Turn>> {
        let mut prepare_calls = self.prepare_calls.lock().await;
        *prepare_calls += 1;
        if *prepare_calls == 1 {
            return Ok(ContextPreparation::ReplaceHistory {
                history: vec![Turn::user("blocked summary")],
            });
        }
        Ok(ContextPreparation::Ready)
    }

    async fn execute_llm<S>(
        &self,
        _request: AgentTurnRequest<Turn>,
        _sink: &mut S,
        _side_sinks: &mut Vec<Box<dyn TurnSink<TextTurnEvent, Output = ()>>>,
    ) -> anyhow::Result<ExecutorCommit<Turn>>
    where
        S: TurnSink<TextTurnEvent> + Send,
    {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ExecutorCommit::text("done"))
    }
}

#[tokio::test]
async fn reads_and_forks_see_the_committed_session_during_a_turn() {
    let executor = BlockingExecutor::default();
    let agent = Arc::new(test_agent::<BlockingExecutor>());
    let turn = tokio::spawn({
        let agent = Arc::clone(&agent);
        let executor = executor.clone();
        async move { agent.call("blocked").execute(&2, &executor).await }
    });
    executor.started.notified().await;

    {
        let session = agent.session().await;
        assert_eq!(session.context().history().len(), 1);
        assert!(session.user_document_cursor().is_empty());
    }
    let fork = agent.forked().await;
    {
        let forked_session = fork.session().await;
        assert_eq!(forked_session.context().history().len(), 1);
        assert!(forked_session.user_document_cursor().is_empty());
    }

    executor.release.notify_one();
    turn.await.unwrap().unwrap();
    let session = agent.session().await;
    assert_eq!(session.context().history().len(), 3);
    assert!(committed_context_xml(&session).contains("<value>2</value>"));
}

#[tokio::test]
async fn external_session_mutation_waits_for_the_turn_then_is_preserved() {
    let executor = BlockingExecutor::default();
    let agent = Arc::new(test_agent::<BlockingExecutor>());
    let turn = tokio::spawn({
        let agent = Arc::clone(&agent);
        let executor = executor.clone();
        async move { agent.call("blocked").execute(&2, &executor).await }
    });
    executor.started.notified().await;

    let mut writer = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            agent.session_mut().await.context_state_mut().marker = 9;
        }
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut writer)
            .await
            .is_err()
    );

    executor.release.notify_one();
    turn.await.unwrap().unwrap();
    writer.await.unwrap();

    let session = agent.session().await;
    assert_eq!(session.context().history().len(), 3);
    assert_eq!(session.context().context_state().marker, 9);
    assert!(committed_context_xml(&session).contains("<value>2</value>"));
}

#[tokio::test]
async fn cancelling_a_turn_discards_its_session_draft_and_releases_writers() {
    let executor = BlockingExecutor::default();
    let agent = Arc::new(test_agent::<BlockingExecutor>());
    let turn = tokio::spawn({
        let agent = Arc::clone(&agent);
        let executor = executor.clone();
        async move { agent.call("blocked").execute(&2, &executor).await }
    });
    executor.started.notified().await;

    turn.abort();
    assert!(turn.await.unwrap_err().is_cancelled());

    let mut session = tokio::time::timeout(std::time::Duration::from_secs(1), agent.session_mut())
        .await
        .unwrap();
    assert_eq!(session.context().history().len(), 1);
    assert_eq!(&*session.context().history()[0].text, "old history");
    assert!(session.user_document_cursor().is_empty());
    session.context_state_mut().marker = 11;
}

struct BlockingCommitObserver {
    started: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl AgentTurnObserver for BlockingCommitObserver {
    async fn on_agent_turn_event(&self, event: AgentTurnEvent) {
        if matches!(event, AgentTurnEvent::TurnCommitted { .. }) {
            self.started.notify_one();
            self.release.notified().await;
        }
    }
}

#[tokio::test]
async fn committed_session_publication_has_no_cancellable_observer_await_after_it() {
    let executor = ReplacingExecutor::replace_once();
    let observer = Arc::new(BlockingCommitObserver {
        started: Notify::new(),
        release: Notify::new(),
    });
    let agent = test_agent::<ReplacingExecutor>()
        .with_observer(observer.clone() as AgentTurnObserverHandle);

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        agent.call("seed").execute(&2, &executor),
    )
    .await;

    if result.is_ok() {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            observer.started.notified(),
        )
        .await
        .unwrap();
    }
    observer.release.notify_one();

    assert!(matches!(result, Ok(Ok(()))));
    let session = agent.session().await;
    assert_eq!(session.context().history().len(), 3);
    assert!(committed_context_xml(&session).contains("<value>2</value>"));
}
