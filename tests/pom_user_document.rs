use agentview::pom::{
    ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, InlineChildren, InlineContent,
    MarkdownNode, ParagraphNode, ResolvedDocument, TextNode, XmlName, XmlNode,
};
use agentview::pom_cursor::UserDocumentCursor;
use agentview::pom_renderer::render_pom_document;
use agentview::pom_resolution::{
    resolve_system_document, resolve_user_document, PomResolutionError,
};
use agentview::AgentView;
use std::collections::BTreeMap;

fn text_xml(name: &str, text: &str) -> XmlNode {
    XmlNode::try_build(name, |children| {
        children.text(TextNode::new(text));
        Ok(())
    })
    .unwrap()
}

fn context(stable: &str, status: &str) -> XmlNode {
    XmlNode::try_build("agent_context", |children| {
        children.xml(text_xml("stable", stable));
        children.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            text_xml("status", status),
        ));
        Ok(())
    })
    .unwrap()
}

fn markdown_context(note: &str, status: &str) -> XmlNode {
    XmlNode::try_build("agent_context", |children| {
        let mut paragraph = InlineChildren::new();
        paragraph.push(InlineContent::try_text(note)?);
        children.markdown(MarkdownNode::Paragraph(ParagraphNode::new(paragraph)));
        children.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            text_xml("status", status),
        ));
        Ok(())
    })
    .unwrap()
}

fn user_document(task: &str, slot: Option<DiffSlot>) -> Document {
    Document::try_build(|blocks| {
        blocks.try_paragraph(|paragraph| paragraph.try_text(task))?;
        if let Some(slot) = slot {
            blocks.xml_slot(slot);
        }
        Ok(())
    })
    .unwrap()
}

fn resolved_ordinary(task: &str) -> ResolvedDocument {
    resolve_system_document(user_document(task, None))
}

fn resolved_full(task: &str, strategy: DiffStrategy, value: XmlNode) -> ResolvedDocument {
    resolve_system_document(user_document(
        task,
        Some(DiffSlot::present(strategy, value)),
    ))
}

fn resolved_with_xml(task: &str, xml: XmlNode) -> ResolvedDocument {
    resolve_system_document(
        Document::try_build(|blocks| {
            blocks.try_paragraph(|paragraph| paragraph.try_text(task))?;
            blocks.xml(xml);
            Ok(())
        })
        .unwrap(),
    )
}

fn delta_context(status: &str) -> XmlNode {
    let mut delta = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    delta
        .push_attribute(XmlName::try_from("rendering_mode").unwrap(), "delta")
        .unwrap();
    delta.push(agentview::pom::MixedContent::xml(text_xml(
        "status", status,
    )));
    delta
}

fn deletion(role: &str) -> XmlNode {
    let mut deletion = XmlNode::new(XmlName::try_from(role).unwrap());
    deletion
        .push_attribute(XmlName::try_from("rendering_mode").unwrap(), "delta")
        .unwrap();
    deletion.push(agentview::pom::MixedContent::xml(XmlNode::new(
        XmlName::try_from("none").unwrap(),
    )));
    deletion
}

fn collection(name: &str, items: &[&str]) -> XmlNode {
    XmlNode::try_build(name, |children| {
        for item in items {
            children.xml(text_xml("item", item));
        }
        Ok(())
    })
    .unwrap()
}

fn keyed_item(id: &str, status: &str) -> XmlNode {
    let mut item = XmlNode::new(XmlName::try_from("actor").unwrap());
    item.push_attribute(XmlName::try_from("id").unwrap(), id)
        .unwrap();
    item.push_attribute(XmlName::try_from("status").unwrap(), status)
        .unwrap();
    item
}

fn keyed_collection(name: &str, items: &[(&str, &str)]) -> XmlNode {
    XmlNode::try_build(name, |children| {
        for (id, status) in items {
            children.xml(keyed_item(id, status));
        }
        Ok(())
    })
    .unwrap()
}

fn operation(name: &str, value: XmlNode) -> XmlNode {
    XmlNode::try_build(name, |children| {
        children.xml(value);
        Ok(())
    })
    .unwrap()
}

fn collection_delta(name: &str, operations: Vec<XmlNode>) -> XmlNode {
    let mut delta = XmlNode::new(XmlName::try_from(name).unwrap());
    delta
        .push_attribute(XmlName::try_from("rendering_mode").unwrap(), "delta")
        .unwrap();
    for operation in operations {
        delta.push(agentview::pom::MixedContent::xml(operation));
    }
    delta
}

fn resolve_collection_change(
    strategy: DiffStrategy,
    previous: XmlNode,
    current: XmlNode,
) -> (ResolvedDocument, UserDocumentCursor) {
    let (_, cursor) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(strategy.clone(), previous)),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();
    resolve_user_document(
        user_document("Continue.", Some(DiffSlot::present(strategy, current))),
        &cursor,
    )
    .unwrap()
}

fn only_xml_child(node: &XmlNode) -> XmlNode {
    let mut children = node.children().iter();
    let child = match children.next() {
        Some(ContentRef::Node(ContentNode::Xml(child))) => child.clone(),
        other => panic!("expected one XML child, got {other:?}"),
    };
    assert!(children.next().is_none());
    child
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, agentview::AgentView)]
#[agent_view(kind = "compound_key")]
struct CompoundMapKey {
    namespace: &'static str,
    id: u8,
}

fn map_node(entries: &[(CompoundMapKey, &'static str)]) -> XmlNode {
    entries
        .iter()
        .cloned()
        .collect::<BTreeMap<_, _>>()
        .build_root()
        .unwrap()
}

fn map_entry(key: CompoundMapKey, value: &'static str) -> XmlNode {
    only_xml_child(&map_node(&[(key, value)]))
}

#[test]
fn first_seen_slot_materializes_full_and_records_only_its_complete_baseline() {
    let current = context("same", "ready");
    let source = user_document(
        "Choose an action.",
        Some(DiffSlot::present(DiffStrategy::Recursive, current.clone())),
    );

    let (resolved, next) = resolve_user_document(source, &UserDocumentCursor::default()).unwrap();

    assert_eq!(
        resolved,
        resolved_full(
            "Choose an action.",
            DiffStrategy::Recursive,
            current.clone()
        )
    );
    assert_eq!(next.len(), 1);
    assert_eq!(
        next.strategy(&XmlName::try_from("agent_context").unwrap()),
        Some(&DiffStrategy::Recursive)
    );
    assert_eq!(
        next.value(&XmlName::try_from("agent_context").unwrap()),
        Some(&current)
    );
}

#[test]
fn unchanged_slot_is_omitted_while_ordinary_current_content_is_preserved() {
    let current = context("same", "ready");
    let (_, previous) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(DiffStrategy::Recursive, current.clone())),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();

    let (resolved, next) = resolve_user_document(
        user_document(
            "Second task.",
            Some(DiffSlot::present(DiffStrategy::Recursive, current)),
        ),
        &previous,
    )
    .unwrap();

    assert_eq!(resolved, resolved_ordinary("Second task."));
    assert_eq!(next, previous);
}

#[test]
fn unchanged_inline_slot_prunes_its_now_empty_markdown_containers() {
    fn inline_document(value: XmlNode, strong: bool) -> Document {
        Document::try_build(|blocks| {
            blocks.try_paragraph(|paragraph| {
                let slot = DiffSlot::present(DiffStrategy::Recursive, value);
                if strong {
                    paragraph.strong(|content| content.xml_slot(slot));
                } else {
                    paragraph.xml_slot(slot);
                }
                Ok(())
            })?;
            Ok(())
        })
        .unwrap()
    }

    for strong in [false, true] {
        let value = text_xml("agent_context", "ready");
        let (first, cursor) = resolve_user_document(
            inline_document(value.clone(), strong),
            &UserDocumentCursor::default(),
        )
        .unwrap();
        assert!(!render_pom_document(&first).unwrap().is_empty());

        let (unchanged, next) =
            resolve_user_document(inline_document(value, strong), &cursor).unwrap();

        assert!(unchanged.children().is_empty());
        assert_eq!(render_pom_document(&unchanged).unwrap(), "");
        assert_eq!(next, cursor);
    }
}

#[test]
fn xml_diff_value_can_retain_markdown_while_a_nested_slot_changes() {
    let previous_value = markdown_context("Stable Markdown.", "ready");
    let (_, cursor) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(DiffStrategy::Recursive, previous_value)),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();

    let (resolved, _) = resolve_user_document(
        user_document(
            "Continue.",
            Some(DiffSlot::present(
                DiffStrategy::Recursive,
                markdown_context("Stable Markdown.", "changed"),
            )),
        ),
        &cursor,
    )
    .unwrap();

    assert_eq!(
        resolved,
        resolved_with_xml("Continue.", delta_context("changed"))
    );
}

#[test]
fn resolution_preserves_empty_xml_wrappers_and_authored_empty_markdown_errors() {
    fn wrapped_slot_document(value: XmlNode) -> Document {
        Document::try_build(|blocks| {
            blocks.xml(XmlNode::try_build("wrapper", |children| {
                children.xml_slot(DiffSlot::present(DiffStrategy::Recursive, value));
                Ok(())
            })?);
            Ok(())
        })
        .unwrap()
    }

    let value = text_xml("agent_context", "ready");
    let (_, cursor) = resolve_user_document(
        wrapped_slot_document(value.clone()),
        &UserDocumentCursor::default(),
    )
    .unwrap();
    let (unchanged, _) = resolve_user_document(wrapped_slot_document(value), &cursor).unwrap();
    assert_eq!(render_pom_document(&unchanged).unwrap(), "<wrapper />");

    let authored_empty = Document::build(|blocks| blocks.paragraph(|_| {}));
    let (authored_empty, _) =
        resolve_user_document(authored_empty, &UserDocumentCursor::default()).unwrap();
    assert_eq!(
        render_pom_document(&authored_empty),
        Err(agentview::pom_renderer::PomRenderError::InvalidEmptyParagraph)
    );
}

#[test]
fn recursive_change_lowers_to_a_slot_free_delta_and_advances_the_full_baseline() {
    let previous_value = context("same", "ready");
    let (_, previous) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(DiffStrategy::Recursive, previous_value)),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();
    let current = context("same", "changed");

    let (resolved, next) = resolve_user_document(
        user_document(
            "Continue.",
            Some(DiffSlot::present(DiffStrategy::Recursive, current.clone())),
        ),
        &previous,
    )
    .unwrap();

    assert_eq!(
        resolved,
        resolved_with_xml("Continue.", delta_context("changed"))
    );
    assert_eq!(
        next.value(&XmlName::try_from("agent_context").unwrap()),
        Some(&current)
    );
    assert_slot_free(&resolved);
}

#[test]
fn explicit_absent_emits_deletion_and_removes_the_baseline() {
    let (_, previous) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(
                DiffStrategy::Recursive,
                context("same", "ready"),
            )),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();

    let (resolved, next) = resolve_user_document(
        user_document(
            "Continue.",
            Some(DiffSlot::absent(
                XmlName::try_from("agent_context").unwrap(),
                DiffStrategy::Recursive,
            )),
        ),
        &previous,
    )
    .unwrap();

    assert_eq!(
        resolved,
        resolved_with_xml("Continue.", deletion("agent_context"))
    );
    assert!(!next.contains(&XmlName::try_from("agent_context").unwrap()));
    assert_slot_free(&resolved);
}

#[test]
fn strategy_change_sends_full_current_value_and_replaces_the_baseline_strategy() {
    let value = context("same", "ready");
    let (_, previous) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(DiffStrategy::Recursive, value.clone())),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();

    let (resolved, next) = resolve_user_document(
        user_document(
            "Continue.",
            Some(DiffSlot::present(DiffStrategy::Replace, value.clone())),
        ),
        &previous,
    )
    .unwrap();

    assert_eq!(
        resolved,
        resolved_full("Continue.", DiffStrategy::Replace, value.clone())
    );
    assert_eq!(
        next.strategy(&XmlName::try_from("agent_context").unwrap()),
        Some(&DiffStrategy::Replace)
    );
    assert_eq!(
        next.value(&XmlName::try_from("agent_context").unwrap()),
        Some(&value)
    );
}

#[test]
fn a_slot_missing_from_the_current_document_keeps_its_existing_baseline() {
    let (_, previous) = resolve_user_document(
        user_document(
            "First task.",
            Some(DiffSlot::present(
                DiffStrategy::Recursive,
                context("same", "ready"),
            )),
        ),
        &UserDocumentCursor::default(),
    )
    .unwrap();

    let (resolved, next) =
        resolve_user_document(user_document("No context this turn.", None), &previous).unwrap();

    assert_eq!(resolved, resolved_ordinary("No context this turn."));
    assert_eq!(next, previous);
}

#[test]
fn append_diff_emits_only_insert_operations_for_the_new_tail() {
    let current = collection("items", &["a", "b", "c", "d"]);

    let (resolved, next) = resolve_collection_change(
        DiffStrategy::Append,
        collection("items", &["a", "b"]),
        current.clone(),
    );

    assert_eq!(
        resolved,
        resolved_with_xml(
            "Continue.",
            collection_delta(
                "items",
                vec![
                    operation("insert", text_xml("item", "c")),
                    operation("insert", text_xml("item", "d")),
                ],
            ),
        )
    );
    assert_eq!(
        next.value(&XmlName::try_from("items").unwrap()),
        Some(&current)
    );
}

#[test]
fn append_diff_falls_back_to_the_complete_current_collection_when_the_prefix_changes() {
    let current = collection("items", &["a", "changed"]);

    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Append,
        collection("items", &["a", "b"]),
        current.clone(),
    );

    assert_eq!(resolved, resolved_with_xml("Continue.", current));
}

#[test]
fn sequence_diff_emits_tail_insertions_and_removals() {
    let inserted = collection("items", &["a", "b", "c"]);
    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Sequence,
        collection("items", &["a"]),
        inserted,
    );
    assert_eq!(
        resolved,
        resolved_with_xml(
            "Continue.",
            collection_delta(
                "items",
                vec![
                    operation("insert", text_xml("item", "b")),
                    operation("insert", text_xml("item", "c")),
                ],
            ),
        )
    );

    let shortened = collection("items", &["a"]);
    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Sequence,
        collection("items", &["a", "b", "c"]),
        shortened,
    );
    assert_eq!(
        resolved,
        resolved_with_xml(
            "Continue.",
            collection_delta(
                "items",
                vec![
                    operation("remove", text_xml("item", "b")),
                    operation("remove", text_xml("item", "c")),
                ],
            ),
        )
    );
}

#[test]
fn sequence_diff_falls_back_to_the_complete_current_collection_for_a_middle_change() {
    let current = collection("items", &["a", "changed", "c"]);

    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Sequence,
        collection("items", &["a", "b", "c"]),
        current.clone(),
    );

    assert_eq!(resolved, resolved_with_xml("Continue.", current));
}

#[test]
fn set_diff_emits_structural_insertions_and_removals_but_ignores_order() {
    let current = collection("items", &["b", "c"]);
    let (resolved, _) =
        resolve_collection_change(DiffStrategy::Set, collection("items", &["a", "b"]), current);
    assert_eq!(
        resolved,
        resolved_with_xml(
            "Continue.",
            collection_delta(
                "items",
                vec![
                    operation("insert", text_xml("item", "c")),
                    operation("remove", text_xml("item", "a")),
                ],
            ),
        )
    );

    let reordered = collection("items", &["b", "a"]);
    let (resolved, next) = resolve_collection_change(
        DiffStrategy::Set,
        collection("items", &["a", "b"]),
        reordered.clone(),
    );
    assert_eq!(resolved, resolved_ordinary("Continue."));
    assert_eq!(
        next.value(&XmlName::try_from("items").unwrap()),
        Some(&reordered)
    );
}

#[test]
fn keyed_diff_uses_attributes_for_insert_remove_and_update_identity() {
    let current = keyed_collection("actors", &[("c", "new"), ("a", "changed")]);
    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Keyed(XmlName::try_from("id").unwrap()),
        keyed_collection("actors", &[("b", "gone"), ("a", "old")]),
        current,
    );

    assert_eq!(
        resolved,
        resolved_with_xml(
            "Continue.",
            collection_delta(
                "actors",
                vec![
                    operation("insert", keyed_item("c", "new")),
                    operation("remove", keyed_item("b", "gone")),
                    operation("update", keyed_item("a", "changed")),
                ],
            ),
        )
    );
}

#[test]
fn keyed_diff_falls_back_for_missing_or_duplicate_identity_attributes() {
    let missing = XmlNode::try_build("actors", |children| {
        children.xml(XmlNode::new(XmlName::try_from("actor")?));
        Ok(())
    })
    .unwrap();
    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Keyed(XmlName::try_from("id").unwrap()),
        keyed_collection("actors", &[("a", "old")]),
        missing.clone(),
    );
    assert_eq!(resolved, resolved_with_xml("Continue.", missing));

    let duplicate = keyed_collection("actors", &[("a", "first"), ("a", "second")]);
    let (resolved, _) = resolve_collection_change(
        DiffStrategy::Keyed(XmlName::try_from("id").unwrap()),
        keyed_collection("actors", &[("a", "old")]),
        duplicate.clone(),
    );
    assert_eq!(resolved, resolved_with_xml("Continue.", duplicate));
}

#[test]
fn recursive_diff_uses_intrinsic_map_identity_for_insert_remove_and_update() {
    let namespace_a = CompoundMapKey {
        namespace: "scene",
        id: 1,
    };
    let namespace_b = CompoundMapKey {
        namespace: "scene",
        id: 2,
    };
    let namespace_c = CompoundMapKey {
        namespace: "scene",
        id: 3,
    };
    let previous = map_node(&[(namespace_a.clone(), "old"), (namespace_b.clone(), "gone")]);
    let current = map_node(&[
        (namespace_a.clone(), "changed"),
        (namespace_c.clone(), "new"),
    ]);

    let (resolved, _) =
        resolve_collection_change(DiffStrategy::Recursive, previous, current.clone());

    assert_eq!(
        resolved,
        resolved_with_xml(
            "Continue.",
            collection_delta(
                "map",
                vec![
                    operation("insert", map_entry(namespace_c, "new")),
                    operation("remove", map_entry(namespace_b, "gone")),
                    operation("update", map_entry(namespace_a, "changed")),
                ],
            ),
        )
    );
}

#[test]
fn duplicate_outermost_roles_fail_before_returning_output_or_a_next_cursor() {
    let role = XmlName::try_from("agent_context").unwrap();
    let first = context("first", "ready");
    let second = context("second", "ready");
    let panel = XmlNode::try_build("panel", |children| {
        children.xml_slot(DiffSlot::present(DiffStrategy::Recursive, second));
        Ok(())
    })
    .unwrap();
    let source = Document::try_build(|blocks| {
        blocks.xml_slot(DiffSlot::present(DiffStrategy::Recursive, first));
        blocks.xml(panel);
        Ok(())
    })
    .unwrap();
    let previous = UserDocumentCursor::default();

    let error = resolve_user_document(source, &previous).unwrap_err();

    assert_eq!(
        error,
        PomResolutionError::DuplicateUserDiffSlotRole { role }
    );
    assert!(previous.is_empty());
}

fn assert_slot_free(document: &ResolvedDocument) {
    fn visit<'a>(children: impl Iterator<Item = ContentRef<'a>>) {
        for child in children {
            match child {
                ContentRef::DiffSlot(slot) => {
                    panic!("resolved document retained slot `{}`", slot.role())
                }
                ContentRef::Node(ContentNode::Xml(xml)) => visit(xml.children().iter()),
                ContentRef::Node(ContentNode::Markdown(markdown)) => match markdown {
                    agentview::pom::MarkdownNode::Heading(node) => visit(node.children().iter()),
                    agentview::pom::MarkdownNode::Paragraph(node) => visit(node.children().iter()),
                    agentview::pom::MarkdownNode::List(node) => {
                        for item in node.items() {
                            visit(item.children().iter());
                        }
                    }
                    agentview::pom::MarkdownNode::Strong(node) => visit(node.children().iter()),
                    agentview::pom::MarkdownNode::CodeBlock(_)
                    | agentview::pom::MarkdownNode::CodeSpan(_)
                    | agentview::pom::MarkdownNode::ThematicBreak => {}
                },
                ContentRef::Node(ContentNode::Text(_)) => {}
            }
        }
    }

    visit(document.children().iter());
}
