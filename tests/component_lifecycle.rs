use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use agentview::{
    component::{
        advanced::{
            experimental::{keyed, system, MountPlanError},
            lifecycle::{
                mount_system_epoch, MountedEpoch, SystemMountContext, SystemMountError, SystemView,
            },
            provider::{
                provider_tool, provider_tool_with_context, ProviderDispatchContext,
                ProviderDispatchUpdate, ProviderDispatcher, ProviderDispatcherCx, ProviderToolCall,
                ProviderToolResponse,
            },
        },
        system_view, user_view, NoLiveEffects, PromptRole, UserTurnContext, UserView,
    },
    prelude::*,
};
use serde_json::json;

struct TestChannels;

impl TurnChannels for TestChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[derive(Debug)]
struct TestRuntime;

impl BindingInstance<TestChannels> for TestRuntime {}

#[derive(Debug)]
struct TestDispatcher;

#[async_trait::async_trait]
impl ProviderDispatcher<TestChannels> for TestDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<TestChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("ok"),
            StreamUpdate::new(),
        ))
    }
}

struct MountProps {
    root_renders: Arc<AtomicUsize>,
    child_renders: Arc<AtomicUsize>,
}

struct TurnProps {
    root_renders: Arc<AtomicUsize>,
    child_renders: Arc<AtomicUsize>,
    task: String,
}

struct FactoryProps {
    binding_instantiations: Arc<AtomicUsize>,
    dispatcher_instantiations: Arc<AtomicUsize>,
}

struct DuplicateFactoryProps {
    instantiations: Arc<AtomicUsize>,
}

fn xml_document(name: &str) -> Document {
    Document::from_xml(XmlNode::new(XmlName::try_from(name).unwrap()))
}

#[agentview::view(component)]
fn counted_system_child(counter: Arc<AtomicUsize>) -> PomView {
    counter.fetch_add(1, Ordering::SeqCst);
    view(xml_document("stable_policy"))
}

#[agentview::view(component)]
fn counted_user_child(counter: Arc<AtomicUsize>, task: String) -> PomView {
    counter.fetch_add(1, Ordering::SeqCst);
    let mut task_node = XmlNode::new(XmlName::try_from("task").unwrap());
    task_node.push(MixedContent::text(TextNode::new(task)));
    view(Document::from_xml(task_node))
}

fn render_system(cx: SystemMountContext<'_, MountProps>) -> SystemView<TestChannels, TurnProps> {
    cx.props().root_renders.fetch_add(1, Ordering::SeqCst);
    system_view(component((
        counted_system_child(Arc::clone(&cx.props().child_renders)),
        binding_factory(
            "selection",
            xml_document("select_intent"),
            RuntimeRoute::xml("select_intent").unwrap(),
            || TestRuntime,
        ),
        provider_tool(
            "move_piece",
            ProviderToolSpec::new(
                "move_piece",
                "Move one piece",
                json!({
                    "type": "object",
                    "properties": { "square": { "type": "string" } },
                    "required": ["square"]
                }),
            )
            .unwrap(),
            || TestDispatcher,
        ),
    )))
}

fn render_user(cx: UserTurnContext<'_, TurnProps>) -> UserView {
    cx.props().root_renders.fetch_add(1, Ordering::SeqCst);
    user_view(counted_user_child(
        Arc::clone(&cx.props().child_renders),
        cx.props().task.clone(),
    ))
}

fn render_counted_factory_system(
    cx: SystemMountContext<'_, FactoryProps>,
) -> SystemView<TestChannels> {
    let binding_instantiations = Arc::clone(&cx.props().binding_instantiations);
    let dispatcher_instantiations = Arc::clone(&cx.props().dispatcher_instantiations);
    system_view(component((
        binding_factory(
            "selection",
            xml_document("select_intent"),
            RuntimeRoute::xml("select_intent").unwrap(),
            move || {
                binding_instantiations.fetch_add(1, Ordering::SeqCst);
                TestRuntime
            },
        ),
        provider_tool(
            "move_piece",
            ProviderToolSpec::new("move_piece", "Move one piece", json!({ "type": "object" }))
                .unwrap(),
            move || {
                dispatcher_instantiations.fetch_add(1, Ordering::SeqCst);
                TestDispatcher
            },
        ),
    )))
}

fn render_duplicate_factory_system(
    cx: SystemMountContext<'_, DuplicateFactoryProps>,
) -> SystemView<TestChannels> {
    let first = Arc::clone(&cx.props().instantiations);
    let second = Arc::clone(&cx.props().instantiations);
    system_view(component((
        binding_factory(
            "first",
            xml_document("first_duplicate_contract"),
            RuntimeRoute::xml("duplicate").unwrap(),
            move || {
                first.fetch_add(1, Ordering::SeqCst);
                TestRuntime
            },
        ),
        binding_factory(
            "second",
            xml_document("second_duplicate_contract"),
            RuntimeRoute::xml("duplicate").unwrap(),
            move || {
                second.fetch_add(1, Ordering::SeqCst);
                TestRuntime
            },
        ),
    )))
}

fn render_invalid_system(_: SystemMountContext<'_, ()>) -> SystemView<TestChannels> {
    system_view(component(Document::build(|blocks| {
        blocks.code_block(Some("not a language".into()), TextNode::new("body"));
    })))
}

#[test]
fn system_mounts_once_while_user_renders_for_every_preparation_attempt() {
    let system_root_renders = Arc::new(AtomicUsize::new(0));
    let system_child_renders = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &MountProps {
            root_renders: Arc::clone(&system_root_renders),
            child_renders: Arc::clone(&system_child_renders),
        },
        render_system,
    )
    .unwrap();

    assert_eq!(system_root_renders.load(Ordering::SeqCst), 1);
    assert_eq!(system_child_renders.load(Ordering::SeqCst), 1);
    assert_eq!(
        epoch.rendered_system(),
        "<stable_policy />\n\n<select_intent />"
    );
    assert_eq!(
        render_pom_document(epoch.resolved_system_document()).unwrap(),
        epoch.rendered_system()
    );
    assert_eq!(epoch.binding_factories().len(), 1);
    assert_eq!(epoch.provider_capabilities().len(), 1);
    assert_eq!(
        epoch.provider_capabilities().capabilities()[0].specs()[0].name(),
        "move_piece"
    );

    let user_root_renders = Arc::new(AtomicUsize::new(0));
    let user_child_renders = Arc::new(AtomicUsize::new(0));
    let mut user_cursor = UserDocumentCursor::default();
    for (index, task) in ["first", "after-compaction", "continue"]
        .into_iter()
        .enumerate()
    {
        let plan = epoch
            .prepare_user(
                &TurnProps {
                    root_renders: Arc::clone(&user_root_renders),
                    child_renders: Arc::clone(&user_child_renders),
                    task: task.to_owned(),
                },
                render_user,
            )
            .unwrap();
        let (resolved, next_cursor) =
            resolve_user_document(plan.into_document(), &user_cursor).unwrap();
        user_cursor = next_cursor;
        assert_eq!(
            render_pom_document(&resolved).unwrap(),
            format!("<task>{task}</task>")
        );
        assert_eq!(user_root_renders.load(Ordering::SeqCst), index + 1);
        assert_eq!(user_child_renders.load(Ordering::SeqCst), index + 1);
        assert_eq!(system_root_renders.load(Ordering::SeqCst), 1);
        assert_eq!(system_child_renders.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn mounting_collects_declarations_without_instantiating_attempt_state() {
    let binding_instantiations = Arc::new(AtomicUsize::new(0));
    let dispatcher_instantiations = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &FactoryProps {
            binding_instantiations: Arc::clone(&binding_instantiations),
            dispatcher_instantiations: Arc::clone(&dispatcher_instantiations),
        },
        render_counted_factory_system,
    )
    .unwrap();

    assert_eq!(binding_instantiations.load(Ordering::SeqCst), 0);
    assert_eq!(dispatcher_instantiations.load(Ordering::SeqCst), 0);
    let empty_user = epoch
        .prepare_user(&(), |_| user_view(()))
        .expect("an empty User view is valid");
    assert!(empty_user.user_document().children().is_empty());
    assert_eq!(binding_instantiations.load(Ordering::SeqCst), 0);
    assert_eq!(dispatcher_instantiations.load(Ordering::SeqCst), 0);

    let turn = epoch.begin_turn("factory-test");
    let prepared = turn.prepare_user(&(), |_| user_view(())).unwrap();
    let _attempt = prepared.start_streaming_attempt(NoLiveEffects).unwrap();
    assert_eq!(binding_instantiations.load(Ordering::SeqCst), 1);
    assert_eq!(dispatcher_instantiations.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_mount_never_creates_an_epoch_or_attempt_state() {
    let instantiations = Arc::new(AtomicUsize::new(0));
    let result = mount_system_epoch(
        &DuplicateFactoryProps {
            instantiations: Arc::clone(&instantiations),
        },
        render_duplicate_factory_system,
    );

    assert!(matches!(
        result,
        Err(SystemMountError::Plan(
            MountPlanError::DuplicateBindingRoute { .. }
        ))
    ));
    assert_eq!(instantiations.load(Ordering::SeqCst), 0);
}

#[test]
fn render_failure_prevents_the_epoch_from_becoming_visible() {
    assert!(matches!(
        mount_system_epoch(&(), render_invalid_system),
        Err(SystemMountError::Render(
            PomRenderError::InvalidCodeBlockLanguage { .. }
        ))
    ));
}

fn render_slotted_system(_: SystemMountContext<'_, ()>) -> SystemView<TestChannels> {
    let policy = XmlNode::try_build("policy", |children| {
        children.text(TextNode::new("stable"));
        Ok(())
    })
    .unwrap();
    let document = Document::try_build(|children| {
        children.xml_slot(DiffSlot::present(DiffStrategy::Replace, policy));
        Ok(())
    })
    .unwrap();
    system_view(component(document))
}

#[test]
fn mounted_epoch_preserves_authored_resolved_and_rendered_system_forms() {
    let epoch = mount_system_epoch(&(), render_slotted_system).unwrap();

    assert!(matches!(
        epoch.system_document().children().iter().next(),
        Some(ContentRef::DiffSlot(_))
    ));
    assert!(matches!(
        epoch.resolved_system_document().children().iter().next(),
        Some(ContentRef::Node(_))
    ));
    assert_eq!(epoch.rendered_system(), "<policy>stable</policy>");
}

#[test]
fn failed_user_candidate_can_rerender_without_touching_system() {
    let system_root_renders = Arc::new(AtomicUsize::new(0));
    let system_child_renders = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &MountProps {
            root_renders: Arc::clone(&system_root_renders),
            child_renders: Arc::clone(&system_child_renders),
        },
        render_system,
    )
    .unwrap();
    let user_renders = Arc::new(AtomicUsize::new(0));
    let turn_props = TurnProps {
        root_renders: Arc::new(AtomicUsize::new(0)),
        child_renders: Arc::new(AtomicUsize::new(0)),
        task: "unused".to_owned(),
    };

    let failed_count = Arc::clone(&user_renders);
    let failed = epoch.prepare_user(&turn_props, move |_| {
        failed_count.fetch_add(1, Ordering::SeqCst);
        user_view(view((
            keyed("Task", "same", xml_document("first")),
            keyed("Task", "same", xml_document("second")),
        )))
    });
    assert!(matches!(
        failed,
        Err(ComponentError::DuplicateComponentKey { .. })
    ));

    let captured_task = "retry".to_owned();
    let retried_count = Arc::clone(&user_renders);
    let retried = epoch
        .prepare_user(&turn_props, move |_| {
            retried_count.fetch_add(1, Ordering::SeqCst);
            let mut task = XmlNode::new(XmlName::try_from("task").unwrap());
            task.push(MixedContent::text(TextNode::new(captured_task)));
            user_view(Document::from_xml(task))
        })
        .unwrap();

    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
    assert_eq!(system_root_renders.load(Ordering::SeqCst), 1);
    assert_eq!(system_child_renders.load(Ordering::SeqCst), 1);
    let (resolved, _) =
        resolve_user_document(retried.into_document(), &UserDocumentCursor::default()).unwrap();
    assert_eq!(
        render_pom_document(&resolved).unwrap(),
        "<task>retry</task>"
    );
}

#[test]
fn mounted_epoch_is_shareable_and_contexts_do_not_require_props_traits() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MountedEpoch<TestChannels>>();

    fn render_str(cx: SystemMountContext<'_, str>) -> SystemView<TestChannels> {
        let mut policy = XmlNode::new(XmlName::try_from("policy").unwrap());
        policy.push(MixedContent::text(TextNode::new(cx.props())));
        system_view(component(Document::from_xml(policy)))
    }

    let epoch = mount_system_epoch("unsized", render_str).unwrap();
    assert_eq!(epoch.rendered_system(), "<policy>unsized</policy>");
}

#[agentview::view(component)]
fn preplaced_system_child() -> Component<TestChannels> {
    component(system(xml_document("legacy_child")))
}

fn render_preplaced_system(_: SystemMountContext<'_, ()>) -> SystemView<TestChannels> {
    system_view(preplaced_system_child())
}

#[test]
fn system_view_rejects_a_child_that_preplaces_system_role() {
    assert!(matches!(
        mount_system_epoch(&(), render_preplaced_system),
        Err(SystemMountError::Plan(MountPlanError::Component(
            ComponentError::NestedRole {
                outer: PromptRole::System,
                inner: PromptRole::System,
                ..
            }
        )))
    ));
}

struct JourneyChannels;

impl TurnChannels for JourneyChannels {
    type Output = u32;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = String;
}

struct JourneyMountProps {
    system_renders: Arc<AtomicUsize>,
}

struct JourneyTurnProps {
    user_renders: Arc<AtomicUsize>,
    allowed: u32,
    task: String,
}

#[derive(Debug)]
struct JourneyTools {
    allowed: u32,
}

#[async_trait::async_trait]
impl ProviderDispatcher<JourneyChannels> for JourneyTools {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<JourneyChannels>, Self::Error> {
        assert_eq!(call.name(), "allowed_intent");
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success(json!({ "allowed": self.allowed })),
            StreamUpdate::new(),
        ))
    }
}

fn journey_system(
    cx: SystemMountContext<'_, JourneyMountProps>,
) -> SystemView<JourneyChannels, JourneyTurnProps> {
    cx.props().system_renders.fetch_add(1, Ordering::SeqCst);
    let tool = ProviderToolSpec::new(
        "allowed_intent",
        "Read the intent allowed for this prepared turn",
        json!({ "type": "object", "properties": {} }),
    )
    .unwrap();
    system_view(component((
        xml_document("journey_policy"),
        StreamingXml::<TurnEmission<JourneyChannels>, String>::new(XmlNode::new(
            XmlName::try_from("select_intent").unwrap(),
        ))
        .state_with(|| ())
        .on_complete(|_, _| vec![TurnEmission::Output(7)])
        .into_component(),
        provider_tool_with_context(
            "journey_tools",
            tool,
            |cx: &ProviderDispatcherCx<'_, JourneyTurnProps, JourneyChannels>| {
                Ok(JourneyTools {
                    allowed: cx.props().allowed,
                })
            },
        ),
    )))
}

fn journey_user(cx: UserTurnContext<'_, JourneyTurnProps>) -> UserView {
    cx.props().user_renders.fetch_add(1, Ordering::SeqCst);
    let mut task = XmlNode::new(XmlName::try_from("task").unwrap());
    task.push(MixedContent::text(TextNode::new(&cx.props().task)));
    user_view(Document::from_xml(task))
}

#[tokio::test]
async fn component_first_journey_combines_system_user_streaming_and_native_tools() {
    let system_renders = Arc::new(AtomicUsize::new(0));
    let user_renders = Arc::new(AtomicUsize::new(0));
    let epoch = mount_system_epoch(
        &JourneyMountProps {
            system_renders: Arc::clone(&system_renders),
        },
        journey_system,
    )
    .unwrap();

    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(epoch.binding_factories().len(), 1);
    assert_eq!(epoch.provider_capabilities().len(), 1);

    let first_props = JourneyTurnProps {
        user_renders: Arc::clone(&user_renders),
        allowed: 3,
        task: "first".to_owned(),
    };
    let first_turn = epoch.begin_turn("first");
    let first = first_turn.prepare_user(&first_props, journey_user).unwrap();
    let mut attempt = first.start_streaming_attempt(NoLiveEffects).unwrap();
    assert_eq!(attempt.binding_count(), 1);
    assert_eq!(attempt.provider_tool_count(), 1);

    let tool = attempt
        .call_tool(ProviderToolCall::new(
            "journey-tool-1",
            "allowed_intent",
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(
        tool.result().response(),
        &ProviderToolResponse::success(json!({ "allowed": 3 }))
    );

    let update = attempt
        .on_event(TextTurnEvent::TextDelta("<select_intent />".to_owned()))
        .await
        .unwrap();
    assert!(matches!(update.emissions(), [TurnEmission::Output(7)]));
    let _ = attempt.abort(BindingAbortReason::Cancelled).await;

    let second_props = JourneyTurnProps {
        user_renders: Arc::clone(&user_renders),
        allowed: 5,
        task: "after-compaction".to_owned(),
    };
    let second_turn = epoch.begin_turn("second");
    let second = second_turn
        .prepare_user(&second_props, journey_user)
        .unwrap();
    let (resolved, _) = resolve_user_document(
        second.user_document().clone(),
        &UserDocumentCursor::default(),
    )
    .unwrap();

    assert_eq!(
        render_pom_document(&resolved).unwrap(),
        "<task>after-compaction</task>"
    );
    assert_eq!(system_renders.load(Ordering::SeqCst), 1);
    assert_eq!(user_renders.load(Ordering::SeqCst), 2);
}
