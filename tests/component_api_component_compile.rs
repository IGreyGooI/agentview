use std::sync::Mutex;

static TRYBUILD: Mutex<()> = Mutex::new(());

#[test]
fn target_component_pom_subset_compiles_from_the_public_preludes() {
    let _guard = TRYBUILD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/component_target/pass_static_component.rs");
    tests.pass("tests/ui/component_target/pass_dynamic_component.rs");
    tests.pass("tests/ui/component_target/pass_dynamic_root.rs");
    tests.pass("tests/ui/component_target/pass_event_select.rs");
    tests.pass("tests/ui/component_target/pass_event_selector_names.rs");
    tests.pass("tests/ui/component_target/pass_formatted_text.rs");
    tests.pass("tests/ui/component_target/pass_diff.rs");
    tests.pass("tests/ui/component_target/pass_signal_authoring.rs");
    tests.pass("tests/ui/component_target/pass_xml_streaming_tool_call.rs");
    tests.pass("tests/ui/component_target/pass_non_clone_no_hook_props.rs");
    tests.pass("tests/ui/component_target/pass_provider_port_one_method.rs");
}

#[test]
fn unsupported_or_invalid_target_component_forms_fail_at_compile_time() {
    let _guard = TRYBUILD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/component_target/fail_*.rs");
}
