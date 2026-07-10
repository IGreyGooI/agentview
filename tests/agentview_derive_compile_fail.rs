#[test]
fn vec_diff_requires_an_explicit_mode() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/vec_diff_without_mode.rs");
    t.compile_fail("tests/ui/collection_mode_without_diff.rs");
    t.compile_fail("tests/ui/collection_mode_on_non_vec.rs");
    t.compile_fail("tests/ui/replace_without_diff.rs");
    t.compile_fail("tests/ui/replace_with_collection_mode.rs");
    t.compile_fail("tests/ui/skip_with_diff.rs");
    t.compile_fail("tests/ui/attr_with_diff.rs");
    t.compile_fail("tests/ui/text_with_diff.rs");
    t.compile_fail("tests/ui/comment_with_diff.rs");
    t.compile_fail("tests/ui/flatten_with_diff.rs");
    t.compile_fail("tests/ui/conflicting_field_modes.rs");
}
