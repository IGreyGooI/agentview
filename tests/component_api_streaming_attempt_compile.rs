#[test]
fn streaming_attempt_requires_complete_typed_declarations() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/streaming_attempt/fail_*.rs");
}
