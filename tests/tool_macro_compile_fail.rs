#[test]
fn invalid_tool_signatures_fail_to_compile() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/tool/borrowed_argument.rs");
    tests.compile_fail("tests/ui/tool/missing_result.rs");
    tests.compile_fail("tests/ui/tool/generic.rs");
    tests.compile_fail("tests/ui/tool/receiver.rs");
    tests.compile_fail("tests/ui/tool/flatten.rs");
    tests.compile_fail("tests/ui/tool/name_override.rs");
    tests.pass("tests/ui/tool/cfg_disabled.rs");
    tests.pass("tests/ui/tool/no_direct_schema_dependencies.rs");
}
