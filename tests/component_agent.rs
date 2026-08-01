use std::sync::Arc;

use agentview::component::advanced::experimental::view;
use agentview::component::advanced::experimental::*;
use agentview::prelude::*;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, AgentView)]
#[agent_view(kind = "agent_context")]
struct TestView {
    #[view(diff)]
    value: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TestEffect {
    PreparedWithHistory(usize),
}

#[derive(Debug, Clone, Default)]
struct TestContextState {
    committed_effects: Vec<TestEffect>,
    loop_commits: usize,
}

#[derive(Debug, Clone, Default)]
struct TestComponentHarness;

#[derive(Debug)]
struct TestCallProps {
    intent_index: usize,
}

fn xml_name(value: &str) -> XmlName {
    XmlName::try_from(value).unwrap()
}

fn empty_xml_document(name: &str) -> Document {
    Document::from_xml(XmlNode::new(xml_name(name)))
}

#[async_trait::async_trait]
impl ComponentHarness for TestComponentHarness {
    type Source = usize;
    type View = TestView;
    type ContextState = TestContextState;
    type CallProps = TestCallProps;
    type Event = TextTurnEvent;
    type Output = StreamingOutcome<TestEffect, String>;
    type Binding = StreamingBinding<TestEffect, String>;

    async fn capture_view(&self, source: &Self::Source) -> Self::View {
        TestView { value: *source }
    }

    fn render(
        &self,
        input: ComponentTurnContext<'_, Turn, Self::View, Self::ContextState, Self::CallProps>,
    ) -> View<Self::Binding> {
        let history_len = input.context().history().len();
        let mut contract = XmlNode::new(xml_name("selection"));
        contract
            .push_attribute(xml_name("history_len"), history_len.to_string())
            .unwrap();
        contract
            .push_attribute(
                xml_name("intent_index"),
                input.props().intent_index.to_string(),
            )
            .unwrap();
        let stream = StreamingXml::<TestEffect, String>::new(contract)
            .init_state(history_len)
            .on_complete(|history_len, _| Ok(vec![TestEffect::PreparedWithHistory(*history_len)]))
            .into_view();

        let context = input.current_view().build_root().unwrap();
        let user_document = Document::build(|document| {
            document.xml_slot(DiffSlot::present(DiffStrategy::Recursive, context));
            document.xml(
                XmlNode::try_build("task", |task| {
                    task.text(TextNode::new(input.task()));
                    Ok(())
                })
                .unwrap(),
            );
            document.xml(
                XmlNode::try_build("intent_index", |node| {
                    node.text(TextNode::new(input.props().intent_index.to_string()));
                    Ok(())
                })
                .unwrap(),
            );
        });

        view((
            system((empty_xml_document("rules"), stream)),
            user(user_document),
        ))
    }

    async fn commit_turn(
        &self,
        context: &mut PromptContext<Turn, Self::ContextState>,
        request: &AgentTurnRequest<Turn>,
        executor_commit: ExecutorCommit<Turn>,
        output: &mut Self::Output,
    ) -> anyhow::Result<TurnFlow> {
        context.push_history(Turn::user(request.user.clone()));
        context.extend_history(executor_commit.append);
        context
            .context_state_mut()
            .committed_effects
            .extend(output.effects().iter().cloned());
        if request.call_id.as_ref() == "loop" && context.context_state().loop_commits == 0 {
            context.context_state_mut().loop_commits += 1;
            Ok(TurnFlow::Continue)
        } else {
            Ok(TurnFlow::Wait)
        }
    }
}

#[derive(Debug, Default)]
struct ExecutorState {
    prepare_calls: usize,
    execute_calls: usize,
    requests: Vec<AgentTurnRequest<Turn>>,
}

#[derive(Clone)]
struct TestExecutor {
    state: Arc<Mutex<ExecutorState>>,
    replace_once: bool,
    fail_execution: bool,
}

impl TestExecutor {
    fn ready() -> Self {
        Self {
            state: Arc::new(Mutex::new(ExecutorState::default())),
            replace_once: false,
            fail_execution: false,
        }
    }

    fn replace_once() -> Self {
        Self {
            replace_once: true,
            ..Self::ready()
        }
    }

    fn failing() -> Self {
        Self {
            fail_execution: true,
            ..Self::ready()
        }
    }
}

#[async_trait::async_trait]
impl LLMExecutor<Turn, TextTurnEvent> for TestExecutor {
    async fn prepare_context(
        &self,
        _request: &AgentTurnRequest<Turn>,
        _budget: ContextPreparationBudget,
    ) -> anyhow::Result<ContextPreparation<Turn>> {
        let mut state = self.state.lock().await;
        state.prepare_calls += 1;
        if self.replace_once && state.prepare_calls == 1 {
            return Ok(ContextPreparation::ReplaceHistory {
                history: vec![Turn::user("summary-a"), Turn::assistant("summary-b")],
            });
        }
        Ok(ContextPreparation::Ready)
    }

    async fn execute_llm<S>(
        &self,
        request: AgentTurnRequest<Turn>,
        sink: &mut S,
        _side_sinks: &mut Vec<Box<dyn TurnSink<TextTurnEvent, Output = ()>>>,
    ) -> anyhow::Result<ExecutorCommit<Turn>>
    where
        S: TurnSink<TextTurnEvent> + Send,
    {
        let mut state = self.state.lock().await;
        state.execute_calls += 1;
        state.requests.push(request);
        drop(state);

        if self.fail_execution {
            anyhow::bail!("provider failed");
        }

        sink.on_event(TextTurnEvent::TextDelta("<selection />".to_owned()))
            .await;
        sink.on_event(TextTurnEvent::TextComplete("<selection />".to_owned()))
            .await;
        Ok(ExecutorCommit::text("done"))
    }
}

fn test_agent() -> ComponentAgent<TestComponentHarness, TestExecutor> {
    let mut context = PromptContext::<Turn, TestContextState>::without_system();
    context.push_history(Turn::user("old history"));
    ComponentAgent::with_component(TestComponentHarness, "model", 64, context)
}

#[tokio::test]
async fn root_component_authors_both_prompts_and_commits_typed_stream_output() {
    let executor = TestExecutor::ready();
    let agent = test_agent();

    let output = agent
        .call("component")
        .with_user("choose")
        .with_props(TestCallProps { intent_index: 7 })
        .execute_component(&7, &executor)
        .await
        .unwrap();

    assert_eq!(output.effects(), &[TestEffect::PreparedWithHistory(1)]);
    let state = executor.state.lock().await;
    assert_eq!(state.execute_calls, 1);
    assert!(state.requests[0].system.starts_with("<rules />"));
    assert!(state.requests[0]
        .system
        .contains("<selection history_len=\"1\" intent_index=\"7\" />"));
    assert!(state.requests[0].user.contains("<agent_context"));
    assert!(state.requests[0].user.contains("<task>choose</task>"));
    assert!(state.requests[0]
        .user
        .contains("<intent_index>7</intent_index>"));
    drop(state);

    let session = agent.session().await;
    assert_eq!(
        session.context().context_state().committed_effects,
        vec![TestEffect::PreparedWithHistory(1)]
    );
    assert_eq!(session.context().history().len(), 3);
}

#[tokio::test]
async fn context_replacement_discards_the_old_component_plan_before_binding() {
    let executor = TestExecutor::replace_once();
    let agent = test_agent();

    let output = agent
        .call("component")
        .with_props(TestCallProps { intent_index: 11 })
        .execute_component(&7, &executor)
        .await
        .unwrap();

    assert_eq!(output.effects(), &[TestEffect::PreparedWithHistory(2)]);
    let state = executor.state.lock().await;
    assert_eq!(state.prepare_calls, 2);
    assert_eq!(state.execute_calls, 1);
    assert!(state.requests[0]
        .system
        .contains("<selection history_len=\"2\" intent_index=\"11\" />"));
}

#[tokio::test]
async fn provider_failure_does_not_commit_component_output_or_cursor() {
    let ready = TestExecutor::ready();
    let agent = test_agent();
    agent
        .call("seed")
        .with_props(TestCallProps { intent_index: 1 })
        .execute_component(&1, &ready)
        .await
        .unwrap();

    let failing = TestExecutor::failing();
    let error = agent
        .call("fail")
        .with_props(TestCallProps { intent_index: 2 })
        .execute_component(&2, &failing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("provider failed"));

    let session = agent.session().await;
    assert_eq!(
        session.context().context_state().committed_effects,
        vec![TestEffect::PreparedWithHistory(1)]
    );
    let role = xml_name("agent_context");
    let committed = session.user_document_cursor().value(&role).unwrap();
    let rendered = render_pom_document(&resolve_system_document(Document::from_xml(
        committed.clone(),
    )))
    .unwrap();
    assert!(rendered.contains("<value>1</value>"));
    assert!(!rendered.contains("<value>2</value>"));
}

#[tokio::test]
async fn component_loop_rebinds_each_turns_matching_hook_plan() {
    let executor = TestExecutor::ready();
    let agent = test_agent();

    agent
        .call("loop")
        .with_props(TestCallProps { intent_index: 13 })
        .with_max_loops(3)
        .execute_component_loop(&7, &executor)
        .await
        .unwrap();

    let state = executor.state.lock().await;
    assert_eq!(state.execute_calls, 2);
    assert!(state.requests[0]
        .system
        .contains("<selection history_len=\"1\" intent_index=\"13\" />"));
    assert!(state.requests[1]
        .system
        .contains("<selection history_len=\"3\" intent_index=\"13\" />"));
    drop(state);

    let session = agent.session().await;
    assert_eq!(
        session.context().context_state().committed_effects,
        vec![
            TestEffect::PreparedWithHistory(1),
            TestEffect::PreparedWithHistory(3),
        ]
    );
}

#[tokio::test]
async fn typed_props_change_each_call_without_entering_prompt_context_or_diff_state() {
    let executor = TestExecutor::ready();
    let agent = test_agent();

    for intent_index in [3, 9] {
        agent
            .call("props")
            .with_props(TestCallProps { intent_index })
            .execute_component(&7, &executor)
            .await
            .unwrap();
    }

    let state = executor.state.lock().await;
    assert!(state.requests[0].system.contains("intent_index=\"3\""));
    assert!(state.requests[0]
        .user
        .contains("<intent_index>3</intent_index>"));
    assert!(state.requests[1].system.contains("intent_index=\"9\""));
    assert!(state.requests[1]
        .user
        .contains("<intent_index>9</intent_index>"));
    drop(state);

    let session = agent.session().await;
    assert!(session
        .user_document_cursor()
        .value(&xml_name("intent_index"))
        .is_none());
}

#[tokio::test]
async fn component_call_without_required_props_fails_before_provider_execution() {
    let executor = TestExecutor::ready();
    let agent = test_agent();

    let error = agent
        .call("missing-props")
        .execute_component(&7, &executor)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("with_props"));
    assert_eq!(executor.state.lock().await.execute_calls, 0);
}
