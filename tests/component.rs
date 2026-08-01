use agentview::component::{advanced::experimental::*, pom_view, ComponentError, PomView};
use agentview::prelude::{
    render_pom_document, resolve_system_document, resolve_user_document, Document,
    UserDocumentCursor, XmlName, XmlNode,
};
use agentview::AgentView as _;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn xml_document(name: &str) -> Document {
    Document::from_xml(XmlNode::new(XmlName::try_from(name).unwrap()))
}

fn render_system(document: &Document) -> String {
    render_pom_document(&resolve_system_document(document.clone())).unwrap()
}

fn render_user(document: &Document) -> String {
    let (document, _) =
        resolve_user_document(document.clone(), &UserDocumentCursor::default()).unwrap();
    render_pom_document(&document).unwrap()
}

#[test]
fn component_tree_compiles_two_documents_and_a_separate_binding_plan() {
    let tree: View<&'static str> = view((
        system((
            xml_document("rules"),
            mount(
                "SelectIntent",
                view((
                    xml_document("select_intent"),
                    binding("open", "select-intent-open"),
                )),
            ),
        )),
        user((xml_document("agent_context"), xml_document("task"))),
    ));

    let plan = compile_component(tree).unwrap();

    assert_eq!(
        render_system(plan.system_document()),
        "<rules />\n\n<select_intent />"
    );
    assert_eq!(
        render_user(plan.user_document()),
        "<agent_context />\n\n<task />"
    );
    assert_eq!(plan.hooks().len(), 1);
    let mounted = &plan.hooks().bindings()[0];
    assert_eq!(mounted.id().component().as_str(), "root/SelectIntent#0");
    assert_eq!(mounted.id().key().as_str(), "open");
    assert_eq!(*mounted.binding(), "select-intent-open");
}

#[test]
fn role_wrappers_only_place_pom_and_do_not_hide_bindings() {
    let tree: View<u8> = system(mount(
        "StreamingContract",
        view((xml_document("contract"), binding("stream", 7))),
    ));

    let plan = compile_component(tree).unwrap();
    assert_eq!(render_system(plan.system_document()), "<contract />");
    assert_eq!(render_user(plan.user_document()), "");
    assert_eq!(*plan.hooks().bindings()[0].binding(), 7);
}

#[test]
fn explicit_keys_are_stable_across_role_and_fragment_wrappers() {
    let tree: View<&'static str> = view((
        system(keyed(
            "Tool",
            "primary",
            view((xml_document("tool_contract"), binding("open", "a"))),
        )),
        user(keyed(
            "Tool",
            "secondary",
            view((xml_document("tool_task"), binding("open", "b"))),
        )),
    ));

    let plan = compile_component(tree).unwrap();
    let ids = plan
        .hooks()
        .bindings()
        .iter()
        .map(|binding| binding.id().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec!["root/Tool[primary]::open", "root/Tool[secondary]::open"]
    );
}

#[test]
fn duplicate_child_keys_fail_even_when_roles_differ() {
    let tree: View<()> = view((
        system(keyed("Tool", "same", xml_document("first"))),
        user(keyed("Tool", "same", xml_document("second"))),
    ));

    assert!(matches!(
        compile_component(tree),
        Err(ComponentError::DuplicateComponentKey { parent, key })
            if parent.as_str() == "root" && key.as_str() == "same"
    ));
}

#[test]
fn duplicate_binding_keys_fail_inside_one_component_only() {
    let duplicate: View<u8> = mount("Tool", view((binding("open", 1), binding("open", 2))));
    assert!(matches!(
        compile_component(duplicate),
        Err(ComponentError::DuplicateBindingKey { component, key })
            if component.as_str() == "root/Tool#0" && key.as_str() == "open"
    ));

    let separate: View<u8> = view((
        mount("First", binding("open", 1)),
        mount("Second", binding("open", 2)),
    ));
    let plan = compile_component(separate).unwrap();
    assert_eq!(plan.hooks().len(), 2);
}

#[test]
fn pom_requires_one_explicit_role_placement() {
    assert!(matches!(
        compile_component::<()>(xml_document("unplaced")),
        Err(ComponentError::UnplacedPom { component }) if component.as_str() == "root"
    ));

    let nested: View<()> = system(user(xml_document("nested")));
    assert!(matches!(
        compile_component(nested),
        Err(ComponentError::NestedRole { component, .. }) if component.as_str() == "root"
    ));
}

#[test]
fn compilation_always_returns_two_documents_even_when_empty() {
    let plan = compile_component::<()>(ComponentNode::empty()).unwrap();
    assert_eq!(render_system(plan.system_document()), "");
    assert_eq!(render_user(plan.user_document()), "");
    assert!(plan.hooks().is_empty());
}

#[test]
fn binding_mapping_preserves_component_and_prompt_structure() {
    let node = mount(
        "Mapped",
        view((xml_document("contract"), binding("open", "hook"))),
    )
    .unwrap()
    .map_binding(str::len);
    let plan = compile_component(system(node)).unwrap();

    assert_eq!(render_system(plan.system_document()), "<contract />");
    assert_eq!(*plan.hooks().bindings()[0].binding(), 4);
    assert_eq!(
        plan.hooks().bindings()[0].id().to_string(),
        "root/Mapped#0::open"
    );
}

#[agentview::view(component)]
fn streaming_contract(binding_name: &'static str) -> View<&'static str> {
    view((
        xml_document("streaming_contract"),
        binding("open", binding_name),
    ))
}

#[agentview::view(component)]
fn authored_root() -> View<&'static str> {
    view((
        system(streaming_contract("handler").key("primary")),
        user(xml_document("task")),
    ))
}

#[test]
fn component_attribute_mounts_functions_and_keeps_explicit_props() {
    let plan = compile_component(authored_root()).unwrap();

    assert_eq!(
        render_system(plan.system_document()),
        "<streaming_contract />"
    );
    assert_eq!(render_user(plan.user_document()), "<task />");
    let binding = &plan.hooks().bindings()[0];
    assert_eq!(*binding.binding(), "handler");
    assert!(binding
        .id()
        .component()
        .as_str()
        .ends_with("component::authored_root#0/component::streaming_contract[primary]"));
}

#[derive(Debug, PartialEq, Eq)]
struct AlphaBinding(&'static str);

#[derive(Debug, PartialEq, Eq)]
struct BetaBinding(u8);

#[derive(Debug, PartialEq, Eq)]
enum AppBinding {
    Alpha(AlphaBinding),
    Beta(BetaBinding),
}

#[derive(agentview::AgentView)]
#[agent_view(kind = "derived_contract")]
struct DerivedContract {
    version: u8,
}

#[agentview::view(component)]
fn pure_contract() -> PomView {
    view(xml_document("pure_contract"))
}

#[agentview::view(component)]
fn conditional_pure_contract(primary: bool) -> PomView {
    if primary {
        pure_contract()
    } else {
        pom_view(xml_document("fallback_contract"))
    }
}

#[agentview::view(component)]
fn alpha_contract() -> ProvidedView<AlphaBinding> {
    view((
        xml_document("alpha_contract"),
        binding("runtime", AlphaBinding("alpha")),
    ))
}

#[agentview::view(component)]
fn beta_contract(value: u8) -> ProvidedView<BetaBinding> {
    view((
        xml_document("beta_contract"),
        binding("runtime", BetaBinding(value)),
    ))
}

#[agentview::view(component)]
fn derived_pure_contract() -> PomView {
    let contract = DerivedContract { version: 1 }.build_root()?;
    view(Document::from_xml(contract))
}

#[agentview::view(component)]
fn derived_provided_contract() -> ProvidedView<AlphaBinding> {
    let contract = DerivedContract { version: 2 }.build_root()?;
    view((
        Document::from_xml(contract),
        binding("runtime", AlphaBinding("derived")),
    ))
}

#[agentview::view(component)]
fn mixed_contract(include_alpha: bool) -> ProvidedView<AppBinding> {
    let optional_alpha = include_alpha.then(|| alpha_contract().map_binding(AppBinding::Alpha));
    let beta_contracts = vec![
        beta_contract(1).map_binding(AppBinding::Beta),
        beta_contract(2).map_binding(AppBinding::Beta),
    ];

    view((pure_contract(), optional_alpha, beta_contracts))
}

#[agentview::view(component)]
fn keyed_mixed_contract(include_alpha: bool) -> ProvidedView<AppBinding> {
    let optional_alpha =
        include_alpha.then(|| alpha_contract().map_binding(AppBinding::Alpha).key("alpha"));
    let beta_contracts = vec![
        beta_contract(1)
            .map_binding(AppBinding::Beta)
            .key("beta-one"),
        beta_contract(2)
            .map_binding(AppBinding::Beta)
            .key("beta-two"),
    ];

    view((pure_contract(), optional_alpha, beta_contracts))
}

#[test]
fn pom_view_is_role_agnostic_and_contains_no_bindings() {
    let system_plan = compile_component::<()>(system(pure_contract())).unwrap();
    assert_eq!(
        render_system(system_plan.system_document()),
        "<pure_contract />"
    );
    assert!(system_plan.hooks().is_empty());

    let user_plan = compile_component::<()>(user(pure_contract())).unwrap();
    assert_eq!(render_user(user_plan.user_document()), "<pure_contract />");
    assert!(user_plan.hooks().is_empty());
}

#[test]
fn pom_view_branches_use_explicit_pom_view_outside_the_final_expression() {
    let primary = compile_component::<()>(system(conditional_pure_contract(true))).unwrap();
    assert_eq!(
        render_system(primary.system_document()),
        "<pure_contract />"
    );

    let fallback = compile_component::<()>(system(conditional_pure_contract(false))).unwrap();
    assert_eq!(
        render_system(fallback.system_document()),
        "<fallback_contract />"
    );
}

#[test]
fn pom_errors_propagate_from_derived_views_in_both_component_kinds() {
    let pure_plan = compile_component::<()>(system(derived_pure_contract())).unwrap();
    assert_eq!(
        render_system(pure_plan.system_document()),
        r#"<derived_contract version="1" />"#
    );

    let provided_plan = compile_component(system(derived_provided_contract())).unwrap();
    assert_eq!(
        render_system(provided_plan.system_document()),
        r#"<derived_contract version="2" />"#
    );
    assert_eq!(
        provided_plan.hooks().bindings()[0].binding(),
        &AlphaBinding("derived")
    );
}

#[test]
fn legacy_nested_pom_view_keeps_unambiguous_binding_inference() {
    let tree: View<()> = system(mount("PureChild", view(xml_document("pure_child"))));
    let plan = compile_component(tree).unwrap();

    assert_eq!(render_system(plan.system_document()), "<pure_child />");
    assert!(plan.hooks().is_empty());
}

#[test]
fn pom_and_mapped_provided_views_compose_in_source_order() {
    let plan = compile_component(system(mixed_contract(true))).unwrap();

    assert_eq!(
        render_system(plan.system_document()),
        "<pure_contract />\n\n<alpha_contract />\n\n<beta_contract />\n\n<beta_contract />"
    );
    let bindings = plan
        .hooks()
        .bindings()
        .iter()
        .map(|mounted| mounted.binding())
        .collect::<Vec<_>>();
    assert_eq!(
        bindings,
        vec![
            &AppBinding::Alpha(AlphaBinding("alpha")),
            &AppBinding::Beta(BetaBinding(1)),
            &AppBinding::Beta(BetaBinding(2)),
        ]
    );
    assert!(plan.hooks().bindings()[0]
        .id()
        .component()
        .as_str()
        .contains("component::alpha_contract#1"));
    assert!(plan.hooks().bindings()[1]
        .id()
        .component()
        .as_str()
        .contains("component::beta_contract#2"));
    assert!(plan.hooks().bindings()[2]
        .id()
        .component()
        .as_str()
        .contains("component::beta_contract#3"));
}

#[test]
fn omitted_unkeyed_sibling_preserves_order_but_shifts_positional_identity() {
    let plan = compile_component(system(mixed_contract(false))).unwrap();

    assert_eq!(
        render_system(plan.system_document()),
        "<pure_contract />\n\n<beta_contract />\n\n<beta_contract />"
    );
    assert_eq!(plan.hooks().len(), 2);
    assert_eq!(
        plan.hooks()
            .bindings()
            .iter()
            .map(|mounted| mounted.binding())
            .collect::<Vec<_>>(),
        vec![
            &AppBinding::Beta(BetaBinding(1)),
            &AppBinding::Beta(BetaBinding(2)),
        ]
    );
    assert!(plan.hooks().bindings()[0]
        .id()
        .component()
        .as_str()
        .contains("component::beta_contract#1"));
    assert!(plan.hooks().bindings()[1]
        .id()
        .component()
        .as_str()
        .contains("component::beta_contract#2"));
}

#[test]
fn keyed_dynamic_siblings_keep_identity_when_an_optional_child_is_omitted() {
    let with_alpha = compile_component(system(keyed_mixed_contract(true))).unwrap();
    let without_alpha = compile_component(system(keyed_mixed_contract(false))).unwrap();

    let with_ids = with_alpha
        .hooks()
        .bindings()
        .iter()
        .map(|mounted| mounted.id().component().as_str())
        .collect::<Vec<_>>();
    let without_ids = without_alpha
        .hooks()
        .bindings()
        .iter()
        .map(|mounted| mounted.id().component().as_str())
        .collect::<Vec<_>>();

    assert!(with_ids[0].ends_with("component::alpha_contract[alpha]"));
    assert!(with_ids[1].ends_with("component::beta_contract[beta-one]"));
    assert!(with_ids[2].ends_with("component::beta_contract[beta-two]"));
    assert!(without_ids[0].ends_with("component::beta_contract[beta-one]"));
    assert!(without_ids[1].ends_with("component::beta_contract[beta-two]"));
}

#[agentview::view(component)]
fn counted_pom(counter: Arc<AtomicUsize>, tag: &'static str) -> PomView {
    counter.fetch_add(1, Ordering::SeqCst);
    view(xml_document(tag))
}

#[agentview::view(component)]
fn counted_binding(counter: Arc<AtomicUsize>) -> ProvidedView<u8> {
    counter.fetch_add(1, Ordering::SeqCst);
    view((xml_document("counted_binding"), binding("runtime", 7)))
}

#[agentview::view(component)]
fn deferred_nested_child(counter: Arc<AtomicUsize>) -> ProvidedView<&'static str> {
    counter.fetch_add(1, Ordering::SeqCst);
    view((
        xml_document("deferred_nested_child"),
        binding("runtime", "child"),
    ))
}

#[agentview::view(component)]
fn deferred_nested_parent(
    parent_counter: Arc<AtomicUsize>,
    child_counter: Arc<AtomicUsize>,
) -> ProvidedView<&'static str> {
    parent_counter.fetch_add(1, Ordering::SeqCst);
    view(deferred_nested_child(child_counter).key("child"))
}

#[agentview::view(component)]
fn owned_props_contract(value: String, shared: Arc<str>) -> PomView {
    assert_eq!(value, "owned value");
    assert_eq!(shared.as_ref(), "shared value");
    view(xml_document("owned_props"))
}

#[test]
fn deferred_component_calls_and_composition_do_not_execute_bodies() {
    let system_counter = Arc::new(AtomicUsize::new(0));
    let user_counter = Arc::new(AtomicUsize::new(0));

    let _tree: View<()> = view((
        system(counted_pom(Arc::clone(&system_counter), "system_contract")),
        user(counted_pom(Arc::clone(&user_counter), "user_context")),
    ));

    assert_eq!(system_counter.load(Ordering::SeqCst), 0);
    assert_eq!(user_counter.load(Ordering::SeqCst), 0);
}

#[test]
fn compiling_reachable_deferred_components_executes_each_body_once() {
    let parent_counter = Arc::new(AtomicUsize::new(0));
    let child_counter = Arc::new(AtomicUsize::new(0));
    let tree: View<&'static str> = system(deferred_nested_parent(
        Arc::clone(&parent_counter),
        Arc::clone(&child_counter),
    ));

    assert_eq!(parent_counter.load(Ordering::SeqCst), 0);
    assert_eq!(child_counter.load(Ordering::SeqCst), 0);

    let plan = compile_component(tree).unwrap();

    assert_eq!(parent_counter.load(Ordering::SeqCst), 1);
    assert_eq!(child_counter.load(Ordering::SeqCst), 1);
    assert_eq!(
        render_system(plan.system_document()),
        "<deferred_nested_child />"
    );
}

#[test]
fn optional_none_deferred_child_is_never_executed() {
    let counter = Arc::new(AtomicUsize::new(0));
    let optional = Some(counted_pom(Arc::clone(&counter), "optional_child")).filter(|_| false);

    assert!(optional.is_none());
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    let plan = compile_component::<()>(system(view((xml_document("always"), optional)))).unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert_eq!(render_system(plan.system_document()), "<always />");
}

#[test]
fn mapping_a_deferred_binding_does_not_run_the_body_or_mapper_early() {
    let body_counter = Arc::new(AtomicUsize::new(0));
    let mapper_counter = Arc::new(AtomicUsize::new(0));
    let mapper_counter_for_map = Arc::clone(&mapper_counter);
    let mapped = counted_binding(Arc::clone(&body_counter)).map_binding(move |value| {
        mapper_counter_for_map.fetch_add(1, Ordering::SeqCst);
        format!("mapped-{value}")
    });

    assert_eq!(body_counter.load(Ordering::SeqCst), 0);
    assert_eq!(mapper_counter.load(Ordering::SeqCst), 0);

    let plan = compile_component(system(mapped)).unwrap();

    assert_eq!(body_counter.load(Ordering::SeqCst), 1);
    assert_eq!(mapper_counter.load(Ordering::SeqCst), 1);
    assert_eq!(render_system(plan.system_document()), "<counted_binding />");
    assert_eq!(plan.hooks().bindings()[0].binding(), "mapped-7");
}

#[test]
fn keyed_deferred_components_keep_nested_binding_identity() {
    let parent_counter = Arc::new(AtomicUsize::new(0));
    let child_counter = Arc::new(AtomicUsize::new(0));
    let tree: View<&'static str> = system(
        deferred_nested_parent(Arc::clone(&parent_counter), Arc::clone(&child_counter))
            .key("parent"),
    );

    let plan = compile_component(tree).unwrap();
    let binding = &plan.hooks().bindings()[0];

    assert_eq!(parent_counter.load(Ordering::SeqCst), 1);
    assert_eq!(child_counter.load(Ordering::SeqCst), 1);
    assert!(binding
        .id()
        .component()
        .as_str()
        .contains("component::deferred_nested_parent[parent]"));
    assert!(binding
        .id()
        .to_string()
        .ends_with("/component::deferred_nested_child[child]::runtime"));
}

#[test]
fn owned_string_and_arc_props_compile_and_move_into_deferred_body() {
    let plan = compile_component::<()>(system(owned_props_contract(
        "owned value".to_owned(),
        Arc::<str>::from("shared value"),
    )))
    .unwrap();

    assert_eq!(render_system(plan.system_document()), "<owned_props />");
}
