#[test]
fn chat_completions_is_not_part_of_the_public_provider_api() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/provider/chat_completions_is_private.rs");
    tests.compile_fail("tests/ui/provider/chat_completions_config_is_private.rs");
    tests.compile_fail("tests/ui/provider/chat_completions_errors_are_private.rs");
}
