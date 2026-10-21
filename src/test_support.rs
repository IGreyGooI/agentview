use crate::{
    component::execution::{Frame, RenderedProjection},
    pom::{ContentNode, ContentRef, Document, ResolvedDocument, TextNode, XmlNode},
    pom_resolution::resolve_system_document,
    transcript::CanonicalInputItem,
};

pub(crate) fn text_element(name: &str, value: &str) -> XmlNode {
    XmlNode::try_build(name, |children| {
        children.text(TextNode::new(value));
        Ok(())
    })
    .unwrap()
}

pub(crate) fn element(name: &str, children: impl IntoIterator<Item = XmlNode>) -> XmlNode {
    XmlNode::try_build(name, |builder| {
        for child in children {
            builder.xml(child);
        }
        Ok(())
    })
    .unwrap()
}

pub(crate) fn document(roots: impl IntoIterator<Item = XmlNode>) -> ResolvedDocument {
    resolve_system_document(Document::build(|builder| {
        for root in roots {
            builder.xml(root);
        }
    }))
}

pub(crate) fn item_pom(item: &CanonicalInputItem) -> Option<&ResolvedDocument> {
    match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            Some(pom)
        }
        _ => None,
    }
}

pub(crate) fn root_elements<'a>(
    items: impl IntoIterator<Item = &'a CanonicalInputItem>,
) -> Vec<&'a XmlNode> {
    items
        .into_iter()
        .filter_map(item_pom)
        .flat_map(|pom| pom.children().iter())
        .filter_map(|child| match child {
            ContentRef::Node(ContentNode::Xml(node)) => Some(node),
            _ => None,
        })
        .collect()
}

pub(crate) fn element_text<'a>(
    items: impl IntoIterator<Item = &'a CanonicalInputItem>,
    name: &str,
) -> Vec<&'a str> {
    root_elements(items)
        .into_iter()
        .filter(|node| node.name().as_str() == name)
        .flat_map(|node| node.children().iter())
        .map(|child| match child {
            ContentRef::Node(ContentNode::Text(text)) => text.value(),
            other => panic!("expected a text field, got {other:?}"),
        })
        .collect()
}

pub(crate) fn projection_items(
    projection: &RenderedProjection,
) -> impl Iterator<Item = &CanonicalInputItem> {
    projection.nodes().iter().flat_map(|node| node.items())
}

pub(crate) fn frame_items(frame: &Frame) -> impl Iterator<Item = &CanonicalInputItem> {
    frame
        .submission()
        .replay()
        .iter()
        .chain(frame.submission().staged_inputs())
        .chain(frame.submission().projection().items())
        .chain(frame.submission().ephemeral())
}
