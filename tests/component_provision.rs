use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use agentview::component::advanced::experimental::view;
use agentview::component::advanced::experimental::*;
use agentview::component::advanced::provider::{
    durable_provider_tool, durable_provider_tool_with_key, provider_tool, provider_tools,
    ProviderDispatchContext, ProviderDispatchUpdate, ProviderDispatcher, ProviderToolCall,
    ProviderToolResponse,
};
use agentview::component::PromptRole;
use agentview::prelude::*;
use serde_json::json;

struct LocalChannels;

#[derive(Debug)]
struct LocalOutput(u8);

#[derive(Debug)]
struct LocalLive(String);

impl TurnChannels for LocalChannels {
    type Output = LocalOutput;
    type Live = LocalLive;
    type Commit = Never;
    type Diagnostic = String;
}

struct RootChannels;

#[derive(Debug)]
struct RootOutput {
    _value: u8,
}

#[derive(Debug)]
struct RootLive {
    _value: String,
}

#[derive(Debug)]
struct RootDiagnostic {
    _message: String,
}

impl TurnChannels for RootChannels {
    type Output = RootOutput;
    type Live = RootLive;
    type Commit = Never;
    type Diagnostic = RootDiagnostic;
}

#[derive(Debug)]
struct SelectionRuntime {
    _state: Vec<u8>,
}

impl BindingInstance<LocalChannels> for SelectionRuntime {}

#[derive(Debug)]
struct PhraseRuntime {
    _state: String,
}

impl BindingInstance<LocalChannels> for PhraseRuntime {}

#[derive(Debug)]
struct MoveDispatcher {
    _pending: Vec<String>,
}

#[async_trait::async_trait]
impl ProviderDispatcher<LocalChannels> for MoveDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<LocalChannels>, Self::Error> {
        self._pending.push(
            call.invocation_id()
                .expect("runtime validates invocation identity before dispatch")
                .to_owned(),
        );
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("moved"),
            StreamUpdate::from_emission(TurnEmission::Output(LocalOutput(1))),
        ))
    }
}

#[derive(Debug)]
struct RootRuntime;

impl BindingInstance<RootChannels> for RootRuntime {}

#[derive(Debug)]
struct RootDispatcher;

#[async_trait::async_trait]
impl ProviderDispatcher<RootChannels> for RootDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        _call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<RootChannels>, Self::Error> {
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("ok"),
            StreamUpdate::new(),
        ))
    }
}

fn xml_document(name: &str) -> Document {
    Document::from_xml(XmlNode::new(XmlName::try_from(name).unwrap()))
}

fn move_tool_spec() -> ProviderToolSpec {
    ProviderToolSpec::new(
        "move_piece",
        "Move one piece",
        json!({
            "type": "object",
            "properties": { "square": { "type": "string" } },
            "required": ["square"]
        }),
    )
    .unwrap()
}

#[agentview::view(component)]
fn local_hybrid_component() -> Component<LocalChannels> {
    component((
        xml_document("hybrid_contract"),
        binding_factory(
            "selection",
            xml_document("select_intent"),
            RuntimeRoute::xml("select_intent").unwrap(),
            || SelectionRuntime { _state: Vec::new() },
        ),
        binding_factory(
            "phrase",
            xml_document("phrase"),
            RuntimeRoute::xml("phrase").unwrap(),
            || PhraseRuntime {
                _state: String::new(),
            },
        ),
        provider_tool("move_piece", move_tool_spec(), || MoveDispatcher {
            _pending: Vec::new(),
        }),
    ))
}

#[agentview::view(component)]
fn root_hybrid_component() -> Component<RootChannels> {
    component((
        system(local_hybrid_component().map_channels(TurnChannelMap::new(
            |LocalOutput(value)| RootOutput { _value: value },
            |LocalLive(value)| RootLive { _value: value },
            Never::absurd,
            |message| RootDiagnostic { _message: message },
        ))),
        user(xml_document("turn_task")),
    ))
}

#[test]
fn hybrid_component_splits_heterogeneous_factories_and_provider_capabilities() {
    let plan = compile_mount_provided(root_hybrid_component()).unwrap();
    assert_eq!(
        render_pom_document(&resolve_system_document(plan.system_document().clone())).unwrap(),
        "<hybrid_contract />\n\n<select_intent />\n\n<phrase />"
    );
    let (user, _) =
        resolve_user_document(plan.user_document().clone(), &UserDocumentCursor::default())
            .unwrap();
    assert_eq!(render_pom_document(&user).unwrap(), "<turn_task />");

    let factories = plan.binding_factories().factories();
    assert_eq!(factories.len(), 2);
    assert_eq!(factories[0].route().to_string(), "xml:select_intent");
    assert_eq!(factories[1].route().to_string(), "xml:phrase");
    assert!(factories[0].id().to_string().ends_with("::selection"));
    assert!(factories[1].id().to_string().ends_with("::phrase"));

    let capabilities = plan.provider_capabilities().capabilities();
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0].specs()[0].name(), "move_piece");
    assert!(capabilities[0].id().to_string().ends_with("::move_piece"));
}

#[test]
fn raw_binding_factory_keeps_prompt_contract_and_runtime_declaration_together() {
    let view: MountProvidedView<RootChannels> = system(binding_factory(
        "selection",
        xml_document("selection_contract"),
        RuntimeRoute::xml("selection").unwrap(),
        || RootRuntime,
    ));

    let plan = compile_mount_provided(view).unwrap();
    assert_eq!(
        render_pom_document(&resolve_system_document(plan.system_document().clone())).unwrap(),
        "<selection_contract />"
    );
    assert_eq!(plan.binding_factories().len(), 1);
    assert_eq!(
        plan.binding_factories().factories()[0].route().to_string(),
        "xml:selection"
    );
}

#[test]
fn duplicate_routes_fail_before_any_factory_is_instantiated() {
    let instantiations = Arc::new(AtomicUsize::new(0));
    let first_count = Arc::clone(&instantiations);
    let second_count = Arc::clone(&instantiations);
    let view: MountProvidedView<RootChannels> = system(view((
        binding_factory(
            "first",
            xml_document("first_duplicate_contract"),
            RuntimeRoute::xml("duplicate").unwrap(),
            move || {
                first_count.fetch_add(1, Ordering::SeqCst);
                RootRuntime
            },
        ),
        binding_factory(
            "second",
            xml_document("second_duplicate_contract"),
            RuntimeRoute::xml("duplicate").unwrap(),
            move || {
                second_count.fetch_add(1, Ordering::SeqCst);
                RootRuntime
            },
        ),
    )));

    assert!(matches!(
        compile_mount_provided(view),
        Err(MountPlanError::DuplicateBindingRoute { route, .. })
            if route.to_string() == "xml:duplicate"
    ));
    assert_eq!(instantiations.load(Ordering::SeqCst), 0);
}

#[test]
fn duplicate_provider_tool_names_across_groups_fail_before_dispatcher_instantiation() {
    let instantiations = Arc::new(AtomicUsize::new(0));
    let first_count = Arc::clone(&instantiations);
    let second_count = Arc::clone(&instantiations);
    let view: MountProvidedView<RootChannels> = system(view((
        provider_tool("first", move_tool_spec(), move || {
            first_count.fetch_add(1, Ordering::SeqCst);
            RootDispatcher
        }),
        provider_tool("second", move_tool_spec(), move || {
            second_count.fetch_add(1, Ordering::SeqCst);
            RootDispatcher
        }),
    )));

    assert!(matches!(
        compile_mount_provided(view),
        Err(MountPlanError::DuplicateProviderTool { name, .. }) if name == "move_piece"
    ));
    assert_eq!(instantiations.load(Ordering::SeqCst), 0);
}

#[test]
fn empty_provider_tool_group_fails_during_authoring_before_sync_factory_runs() {
    let instantiations = Arc::new(AtomicUsize::new(0));
    let factory_count = Arc::clone(&instantiations);

    // Factories are synchronous and inert: I/O and resources needing async
    // cleanup belong to ProviderDispatcher::dispatch and ::abort instead.
    let component: Component<RootChannels> = provider_tools(
        "empty_group",
        std::iter::empty::<ProviderToolSpec>(),
        move || {
            factory_count.fetch_add(1, Ordering::SeqCst);
            RootDispatcher
        },
    );

    assert!(matches!(
        compile_mount_provided(component),
        Err(MountPlanError::Component(ComponentError::InvalidBindingContract { message }))
            if message == "a provider tool group requires at least one tool spec"
    ));
    assert_eq!(instantiations.load(Ordering::SeqCst), 0);
}

#[test]
fn duplicate_tool_names_inside_one_group_fail_during_authoring_before_factory_runs() {
    let instantiations = Arc::new(AtomicUsize::new(0));
    let factory_count = Arc::clone(&instantiations);
    let component: Component<RootChannels> = provider_tools(
        "duplicate_group",
        [move_tool_spec(), move_tool_spec()],
        move || {
            factory_count.fetch_add(1, Ordering::SeqCst);
            RootDispatcher
        },
    );

    assert!(matches!(
        compile_mount_provided(component),
        Err(MountPlanError::Component(ComponentError::InvalidBindingContract { message }))
            if message == "provider tool group contains duplicate tool name `move_piece`"
    ));
    assert_eq!(instantiations.load(Ordering::SeqCst), 0);
}

#[test]
fn runtime_declarations_are_rejected_in_user_or_unplaced_subtrees() {
    let user_view: MountProvidedView<RootChannels> = user(binding_factory(
        "user_binding",
        xml_document("user_binding"),
        RuntimeRoute::xml("user_binding").unwrap(),
        || RootRuntime,
    ));
    assert!(matches!(
        compile_mount_provided(user_view),
        Err(MountPlanError::RuntimeDeclarationOutsideSystem {
            role: Some(PromptRole::User),
            ..
        })
    ));

    let unplaced_component: Component<RootChannels> = binding_factory(
        "unplaced_binding",
        xml_document("unplaced_binding"),
        RuntimeRoute::xml("unplaced_binding").unwrap(),
        || RootRuntime,
    );
    assert!(matches!(
        compile_mount_provided(unplaced_component),
        Err(MountPlanError::Component(
            ComponentError::UnplacedPom { .. }
        ))
    ));
}

#[test]
fn factory_and_capability_keys_share_one_component_namespace() {
    let view: MountProvidedView<RootChannels> = system(view((
        binding_factory(
            "duplicate",
            xml_document("selection"),
            RuntimeRoute::xml("selection").unwrap(),
            || RootRuntime,
        ),
        provider_tool("duplicate", move_tool_spec(), || RootDispatcher),
    )));

    assert!(matches!(
        compile_mount_provided(view),
        Err(MountPlanError::Component(
            ComponentError::DuplicateBindingKey { .. }
        ))
    ));
}

#[test]
fn durable_declaration_ids_are_unique_across_binding_and_capability_leaves() {
    let root = durable_system((
        durable_binding_factory_with_key(
            "selection",
            RuntimeContract::new("player.selection", "v1").unwrap(),
            xml_document("selection"),
            RuntimeRoute::xml("selection").unwrap(),
            || RootRuntime,
        ),
        durable_provider_tool_with_key(
            "move_piece",
            RuntimeContract::new("player.selection", "v1").unwrap(),
            (),
            move_tool_spec(),
            || RootDispatcher,
        ),
    ))
    .into_one_shot_component();
    let view: MountProvidedView<RootChannels> = system(root);

    assert!(matches!(
        compile_mount_provided(view),
        Err(MountPlanError::DuplicateRuntimeDeclaration {
            declaration_id,
            ..
        }) if declaration_id == "player.selection"
    ));
}

#[test]
fn provider_tool_schema_must_be_an_object() {
    assert!(matches!(
        ProviderToolSpec::new("bad_tool", "bad", json!(["not", "an", "object"])),
        Err(ComponentError::InvalidBindingContract { .. })
    ));
}

#[test]
fn xml_runtime_route_must_be_a_valid_pom_name() {
    assert!(matches!(
        RuntimeRoute::xml("not a tag"),
        Err(ComponentError::InvalidBindingContract { message })
            if message.contains("invalid XML runtime route")
    ));
}

#[agentview::view(component)]
fn mounted_component_stream(initializations: Arc<AtomicUsize>) -> DurableComponent<RootChannels> {
    StreamingXml::<TurnEmission<RootChannels>, RootDiagnostic>::new(XmlNode::new(
        XmlName::try_from("component_stream").unwrap(),
    ))
    .state_with(move || {
        initializations.fetch_add(1, Ordering::SeqCst);
    })
    .into_durable_component(RuntimeContract::new("player.component-stream", "v1").unwrap())
}

#[agentview::view(component)]
fn mounted_component_root(initializations: Arc<AtomicUsize>) -> DurableSystem<RootChannels> {
    durable_system((
        xml_document("component_policy"),
        mounted_component_stream(initializations),
        durable_provider_tool(
            RuntimeContract::new("player.move-piece", "v1").unwrap(),
            (),
            move_tool_spec(),
            || RootDispatcher,
        ),
    ))
}

#[test]
fn unified_component_carrier_defers_children_and_keeps_runtime_lazy() {
    let initializations = Arc::new(AtomicUsize::new(0));
    let root = mounted_component_root(Arc::clone(&initializations)).into_one_shot_component();
    let plan = compile_mount_provided(system(root)).unwrap();

    assert_eq!(initializations.load(Ordering::SeqCst), 0);
    assert_eq!(plan.binding_factories().len(), 1);
    assert_eq!(plan.provider_capabilities().len(), 1);
    assert_eq!(
        render_pom_document(&resolve_system_document(plan.system_document().clone())).unwrap(),
        "<component_policy />\n\n<component_stream />"
    );
}

#[agentview::view(component)]
fn local_component_carrier() -> Component<LocalChannels> {
    component(binding_factory(
        "selection",
        xml_document("local_component_contract"),
        RuntimeRoute::xml("local_component").unwrap(),
        || SelectionRuntime { _state: Vec::new() },
    ))
}

#[agentview::view(component)]
fn root_component_carrier() -> Component<RootChannels> {
    component(system(local_component_carrier().map_channels(
        TurnChannelMap::new(
            |LocalOutput(value)| RootOutput { _value: value },
            |LocalLive(value)| RootLive { _value: value },
            Never::absurd,
            |message| RootDiagnostic { _message: message },
        ),
    )))
}

#[test]
fn unified_component_maps_one_local_contract_at_the_parent_boundary() {
    let plan = compile_mount_provided(root_component_carrier()).unwrap();

    assert_eq!(plan.binding_factories().len(), 1);
    assert_eq!(
        plan.binding_factories().factories()[0].route().to_string(),
        "xml:local_component"
    );
    assert_eq!(
        render_pom_document(&resolve_system_document(plan.system_document().clone())).unwrap(),
        "<local_component_contract />"
    );
}

#[agentview::view(component)]
fn fallible_component_contract(route: String) -> Component<RootChannels> {
    let route = RuntimeRoute::xml(route)?;
    let tool = ProviderToolSpec::new(
        "inspect",
        "Inspect one value",
        json!({ "type": "object", "properties": {} }),
    )?;
    component(system((
        binding_factory(
            "fallible_stream",
            xml_document("fallible_stream"),
            route,
            || RootRuntime,
        ),
        provider_tool("fallible_tool", tool, || RootDispatcher),
    )))
}

#[test]
fn component_macro_propagates_fallible_authoring_without_a_second_carrier() {
    let valid =
        compile_mount_provided(fallible_component_contract("fallible_stream".to_owned())).unwrap();
    assert_eq!(valid.binding_factories().len(), 1);
    assert_eq!(valid.provider_capabilities().len(), 1);

    assert!(matches!(
        compile_mount_provided(fallible_component_contract("not a route".to_owned())),
        Err(MountPlanError::Component(ComponentError::InvalidBindingContract { message }))
            if message.contains("invalid XML runtime route")
    ));
}
