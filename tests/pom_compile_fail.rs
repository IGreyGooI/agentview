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
    t.compile_fail("tests/ui/pom/document_block_rejects_text_root.rs");
    t.compile_fail("tests/ui/pom/children_have_no_mutable_vector_access.rs");
    t.compile_fail("tests/ui/pom/paragraph_xml_rejects_non_xml_root.rs");
    t.compile_fail("tests/ui/pom/document_is_not_prompt_renderable.rs");
    t.compile_fail("tests/ui/pom/turn_artifact_rejects_raw_payload.rs");
    t.compile_fail("tests/ui/pom/document_diff_rejects_text_root.rs");
    t.compile_fail("tests/ui/pom/document_diff_rejects_markdown_root.rs");
    t.compile_fail("tests/ui/pom/document_diff_rejects_document_root.rs");
    t.compile_fail("tests/ui/pom/document_diff_rejects_block_mode.rs");
    t.compile_fail("tests/ui/pom/document_diff_rejects_xml_mode.rs");
    t.compile_fail("tests/ui/pom/document_name_requires_diff.rs");
    t.compile_fail("tests/ui/pom/document_vec_diff_without_mode.rs");
    t.compile_fail("tests/ui/pom/document_collection_mode_without_diff.rs");
    t.compile_fail("tests/ui/pom/document_collection_mode_on_non_vec.rs");
    t.compile_fail("tests/ui/pom/document_replace_with_collection_mode.rs");
}
