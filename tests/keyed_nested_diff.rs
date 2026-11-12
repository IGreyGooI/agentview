use agentview::{
    pom::{
        ContentNode, ContentRef, DiffSlot, DiffStrategy, Document, ResolvedDocument, XmlName,
        XmlNode,
    },
    pom_cursor::UserDocumentCursor,
    pom_resolution::{resolve_system_document, resolve_user_document},
    AgentView,
};

#[derive(AgentView)]
#[agent_view(kind = "session")]
struct Session {
    id: String,
    #[view(diff)]
    ongoing: bool,
    #[view(diff(seq))]
    messages: Vec<String>,
}

#[derive(AgentView)]
#[agent_view(kind = "sessions")]
struct Sessions {
    #[view(diff(key = "id"))]
    sessions: Vec<Session>,
    #[view(element)]
    scope: &'static str,
}

fn session(id: &str, ongoing: bool, messages: &[&str]) -> Session {
    Session {
        id: id.into(),
        ongoing,
        messages: messages.iter().map(|text| (*text).to_owned()).collect(),
    }
}

fn snapshot(sessions: Vec<Session>) -> Sessions {
    Sessions {
        sessions,
        scope: "reference",
    }
}

fn resolve(
    sessions: &Sessions,
    cursor: &UserDocumentCursor,
) -> (ResolvedDocument, UserDocumentCursor) {
    resolve_user_document(
        Document::build(|blocks| {
            blocks.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                sessions.build_root().unwrap(),
            ));
        }),
        cursor,
    )
    .unwrap()
}

fn children(node: &XmlNode) -> Vec<&XmlNode> {
    node.children()
        .iter()
        .map(|edge| match edge {
            ContentRef::Node(ContentNode::Xml(node)) => node,
            _ => panic!("expected resolved XML children"),
        })
        .collect()
}

fn attr<'a>(node: &'a XmlNode, name: &str) -> Option<&'a str> {
    node.attributes()
        .iter()
        .find(|attribute| attribute.name().as_str() == name)
        .map(|attribute| attribute.value())
}

fn text(node: &XmlNode) -> String {
    node.children()
        .iter()
        .map(|edge| match edge {
            ContentRef::Node(ContentNode::Text(text)) => text.value(),
            _ => panic!("expected text data"),
        })
        .collect()
}

#[test]
fn keyed_update_recurses_nested_sequence_slots_and_preserves_identity_and_baseline() {
    let previous = snapshot(vec![
        session("active", true, &["original"]),
        session("other", true, &["unchanged"]),
    ]);
    let current = snapshot(vec![
        session("other", true, &["unchanged"]),
        session("active", true, &["original", "appended"]),
    ]);
    let (_, cursor) = resolve(&previous, &UserDocumentCursor::default());
    let (patch, cursor) = resolve(&current, &cursor);
    let ContentRef::Node(ContentNode::Xml(root)) = patch.children().iter().next().unwrap() else {
        panic!("root");
    };
    let collection = children(root)[0];
    let update = children(collection)[0];
    assert_eq!(update.name().as_str(), "update");
    assert_eq!(children(update).len(), 1);
    let active = children(update)[0];
    assert_eq!(attr(active, "id"), Some("active"));
    assert_eq!(attr(active, "rendering_mode"), Some("delta"));
    let messages = children(active)[0];
    assert_eq!(messages.name().as_str(), "messages");
    assert_eq!(attr(messages, "rendering_mode"), Some("delta"));
    let insert = children(messages)[0];
    assert_eq!(insert.name().as_str(), "insert");
    assert_eq!(children(insert).len(), 1);
    assert_eq!(text(children(insert)[0]), "appended");
    assert!(resolve(&current, &cursor).0.children().is_empty());

    let completed = snapshot(vec![
        session("active", false, &["original", "appended"]),
        session("other", true, &["unchanged"]),
    ]);
    let (patch, _) = resolve(&completed, &cursor);
    let ContentRef::Node(ContentNode::Xml(root)) = patch.children().iter().next().unwrap() else {
        panic!("root");
    };
    let active = children(children(children(root)[0])[0])[0];
    assert_eq!(attr(active, "id"), Some("active"));
    assert_eq!(attr(active, "rendering_mode"), Some("delta"));
    assert_eq!(children(active).len(), 1);
    assert_eq!(children(active)[0].name().as_str(), "ongoing");
    assert_eq!(text(children(active)[0]), "false");
}

#[derive(AgentView)]
#[agent_view(kind = "record")]
struct Record {
    id: String,
    #[view(element)]
    value: String,
}

#[derive(AgentView)]
#[agent_view(kind = "records")]
struct Records {
    #[view(diff(key = "id"))]
    entries: Vec<Record>,
    #[view(element)]
    scope: &'static str,
}

#[test]
fn ordinary_keyed_updates_keep_complete_record_replacement() {
    let make = |value: &str| Records {
        entries: vec![Record {
            id: "record".into(),
            value: value.into(),
        }],
        scope: "reference",
    };
    let previous = make("before");
    let current = make("after");
    let document = |value: &Records| {
        Document::build(|blocks| {
            blocks.xml_slot(DiffSlot::present(
                DiffStrategy::Recursive,
                value.build_root().unwrap(),
            ));
        })
    };
    let (_, cursor) =
        resolve_user_document(document(&previous), &UserDocumentCursor::default()).unwrap();
    let (patch, _) = resolve_user_document(document(&current), &cursor).unwrap();
    let ContentRef::Node(ContentNode::Xml(root)) = patch.children().iter().next().unwrap() else {
        panic!("root");
    };
    let updated = children(children(children(root)[0])[0])[0];
    assert_eq!(attr(updated, "id"), Some("record"));
    assert_eq!(attr(updated, "rendering_mode"), None);
    assert_eq!(text(children(updated)[0]), "after");
}

#[test]
fn rendering_mode_key_collision_falls_back_to_full_record_and_keeps_baseline() {
    let record = |ongoing, messages: &[&str]| {
        let mut record = session("active", ongoing, messages).build_root().unwrap();
        record
            .push_attribute(XmlName::try_from("rendering_mode").unwrap(), "business-key")
            .unwrap();
        record
    };
    let document = |ongoing, messages: &[&str]| {
        Document::build(|blocks| {
            let collection = XmlNode::try_build("sessions", |children| {
                children.xml(record(ongoing, messages));
                Ok(())
            })
            .unwrap();
            blocks.xml_slot(DiffSlot::present(
                DiffStrategy::Keyed(XmlName::try_from("rendering_mode").unwrap()),
                collection,
            ));
        })
    };
    let (_, cursor) = resolve_user_document(
        document(true, &["original"]),
        &UserDocumentCursor::default(),
    )
    .unwrap();
    let (patch, cursor) =
        resolve_user_document(document(false, &["original", "new"]), &cursor).unwrap();
    let ContentRef::Node(ContentNode::Xml(collection)) = patch.children().iter().next().unwrap()
    else {
        panic!("collection root");
    };
    assert_eq!(attr(collection, "rendering_mode"), Some("delta"));
    let update = children(collection)[0];
    assert_eq!(update.name().as_str(), "update");
    assert_eq!(children(update).len(), 1);
    let updated = children(update)[0];
    assert_eq!(attr(updated, "rendering_mode"), Some("business-key"));
    let expected = resolve_system_document(Document::from_xml(record(false, &["original", "new"])));
    let ContentRef::Node(ContentNode::Xml(expected)) = expected.children().iter().next().unwrap()
    else {
        panic!("complete expected record");
    };
    assert_eq!(updated, expected);
    let (unchanged, _) =
        resolve_user_document(document(false, &["original", "new"]), &cursor).unwrap();
    assert!(unchanged.children().is_empty());
}
