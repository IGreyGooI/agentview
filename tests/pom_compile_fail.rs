#[test]
fn invalid_pom_apis_fail_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/pom/diff_slot_rejects_text_node.rs");
    t.compile_fail("tests/ui/pom/diff_slot_rejects_derived_document.rs");
    t.compile_fail("tests/ui/pom/diff_slot_rejects_derived_markdown.rs");
    t.compile_fail("tests/ui/pom/diff_slot_present_rejects_explicit_role.rs");
    t.compile_fail("tests/ui/pom/markdown_payload_fields_are_private.rs");
    t.compile_fail("tests/ui/pom/inline_children_reject_heading.rs");
    t.compile_fail("tests/ui/pom/document_rejects_text.rs");
    t.compile_fail("tests/ui/pom/children_have_no_mutable_vector_access.rs");
    t.compile_fail("tests/ui/pom/pom_types_do_not_deserialize.rs");
    t.compile_fail("tests/ui/pom/document_is_not_prompt_renderable.rs");
    t.compile_fail("tests/ui/pom/resolved_document_does_not_deserialize.rs");
    t.compile_fail("tests/ui/pom/turn_artifact_rejects_raw_payload.rs");
}
