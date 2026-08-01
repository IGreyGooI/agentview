//! External-consumer proof that ordinary component authors cannot import the
//! legacy raw component IR without opting into its compatibility feature.

#[test]
fn durable_provider_cannot_use_ordinary_attachment() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail(
        "tests/ui/component_public/durable_provider_cannot_use_ordinary_attachment.rs",
    );
}

#[test]
fn legacy_turn_bridge_is_not_part_of_component_prelude() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/component_public/legacy_turn_bridge_is_not_part_of_prelude.rs");
}

#[test]
fn stateful_provider_rehydrate_request_cannot_read_system() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail(
        "tests/ui/component_public/stateful_provider_rehydrate_cannot_read_system.rs",
    );
}

#[test]
fn host_integration_is_not_part_of_component_author_prelude() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail(
        "tests/ui/component_public/host_integration_is_not_in_component_author_prelude.rs",
    );
}

#[test]
fn external_control_is_not_part_of_component_author_prelude() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail(
        "tests/ui/component_public/external_control_is_not_in_component_author_prelude.rs",
    );
}

#[test]
fn provider_dispatcher_binding_is_not_part_of_component_author_prelude() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail(
        "tests/ui/component_public/provider_dispatcher_binding_is_not_in_component_author_prelude.rs",
    );
}

#[test]
fn mounted_host_is_not_part_of_the_legacy_root_prelude() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/component_public/root_prelude_does_not_export_mounted_host.rs");
}

#[test]
fn provider_dispatcher_binding_is_not_part_of_the_legacy_root_prelude() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail(
        "tests/ui/component_public/root_prelude_does_not_export_provider_dispatcher.rs",
    );
}

#[cfg(not(feature = "raw-component-ir"))]
#[test]
fn raw_component_ir_is_not_part_of_default_authoring() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/component_public/raw_component_ir_is_feature_gated.rs");
}
