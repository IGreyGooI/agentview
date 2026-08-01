#[test]
fn invalid_component_functions_fail_to_compile() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/component/*.rs");
}
