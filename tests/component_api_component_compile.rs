use std::{path::PathBuf, sync::Mutex};

static TRYBUILD: Mutex<()> = Mutex::new(());

const LEGACY_FAIL_FIXTURES: [&str; 9] = [
    "fail_component_events_generic.rs",
    "fail_component_events_invalid_selector_name.rs",
    "fail_component_events_multi_field_variant.rs",
    "fail_component_events_named_variant.rs",
    "fail_component_events_non_enum.rs",
    "fail_component_events_selector_collision.rs",
    "fail_component_events_unit_variant.rs",
    "fail_component_events_variant_name_collision.rs",
    "fail_event_selector_wrong_parent.rs",
];

fn fail_fixtures() -> Vec<PathBuf> {
    let mut fixtures = std::fs::read_dir("tests/ui/component_target")
        .expect("read Component UI fixture directory")
        .map(|entry| entry.expect("read Component UI fixture entry").path())
        .filter(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some("rs")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("fail_"))
        })
        .collect::<Vec<_>>();
    fixtures.sort();
    if !cfg!(feature = "legacy-provider-port") {
        fixtures.retain(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !LEGACY_FAIL_FIXTURES.contains(&name))
        });
    }
    fixtures
}

#[test]
fn target_component_pom_subset_compiles_from_the_public_preludes() {
    let _guard = TRYBUILD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/component_target/pass_static_component.rs");
    tests.pass("tests/ui/component_target/pass_dynamic_component.rs");
    tests.pass("tests/ui/component_target/pass_dynamic_root.rs");
    #[cfg(feature = "legacy-provider-port")]
    tests.pass("tests/ui/component_target/pass_event_select.rs");
    #[cfg(feature = "legacy-provider-port")]
    tests.pass("tests/ui/component_target/pass_event_selector_names.rs");
    tests.pass("tests/ui/component_target/pass_formatted_text.rs");
    tests.pass("tests/ui/component_target/pass_diff.rs");
    tests.pass("tests/ui/component_target/pass_signal_authoring.rs");
    tests.pass("tests/ui/component_target/pass_provider_event_handler.rs");
    tests.pass("tests/ui/component_target/pass_preparation_authoring.rs");
    tests.pass("tests/ui/component_target/pass_reaction_completion.rs");
    tests.pass("tests/ui/component_target/pass_reaction_request.rs");
    tests.pass("tests/ui/component_target/pass_async_task_authoring.rs");
    tests.pass("tests/ui/component_target/pass_xml_streaming_tool_call.rs");
    tests.pass("tests/ui/component_target/pass_streaming_xml.rs");
    tests.pass("tests/ui/component_target/pass_non_clone_no_hook_props.rs");
    #[cfg(feature = "legacy-provider-port")]
    tests.pass("tests/ui/component_target/pass_provider_port_one_method.rs");
    tests.pass("tests/ui/component_target/pass_reaction_port.rs");
    tests.pass("tests/ui/component_target/pass_application_lifecycle.rs");
}

#[test]
fn unsupported_or_invalid_target_component_forms_fail_at_compile_time() {
    let _guard = TRYBUILD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tests = trybuild::TestCases::new();
    for fixture in fail_fixtures() {
        tests.compile_fail(fixture);
    }
}
