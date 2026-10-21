#![allow(dead_code)]

use agentview::{
    component::execution::RenderedProjection,
    pom::{ContentNode, ContentRef, Document, MarkdownNode, ResolvedDocument, TextNode, XmlNode},
    pom_resolution::resolve_artifact_document,
    transcript::CanonicalInputItem,
};

pub fn item_document(item: &CanonicalInputItem) -> ResolvedDocument {
    match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            pom.clone()
        }
        other => panic!("expected a typed POM item, got {other:?}"),
    }
}

pub fn item_documents(items: &[CanonicalInputItem]) -> Vec<ResolvedDocument> {
    items.iter().map(item_document).collect()
}

pub fn frame_projection_items(payload: &[u8]) -> Vec<CanonicalInputItem> {
    let payload: serde_json::Value = serde_json::from_slice(payload).unwrap();
    serde_json::from_value(payload["component"]["projection"]["items"].clone()).unwrap()
}

pub fn frame_document(payload: &[u8]) -> ResolvedDocument {
    combine(
        frame_projection_items(payload)
            .iter()
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
                _ => None,
            }),
    )
}

pub fn projection_document(projection: &RenderedProjection) -> ResolvedDocument {
    combine(
        projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => Some(pom.clone()),
                _ => None,
            }),
    )
}

pub fn combine(documents: impl IntoIterator<Item = ResolvedDocument>) -> ResolvedDocument {
    resolve_artifact_document(Document::build(|blocks| {
        for document in documents {
            for child in document.children().iter() {
                let ContentRef::Node(child) = child else {
                    panic!("a resolved document contains no diff slots");
                };
                blocks.push(agentview::pom::BlockContent::try_from_node(child.clone()).unwrap());
            }
        }
    }))
    .unwrap()
}

pub fn text_element(name: &str, value: &str) -> ResolvedDocument {
    let node = XmlNode::try_build(name, |children| {
        children.text(TextNode::new(value.to_owned()));
        Ok(())
    })
    .unwrap();
    resolve_artifact_document(Document::from_xml(node)).unwrap()
}

pub fn nested_element(name: &str, children: &[ResolvedDocument]) -> ResolvedDocument {
    let node = XmlNode::try_build(name, |builder| {
        for child in children
            .iter()
            .flat_map(|document| document.children().iter())
        {
            let ContentRef::Node(child) = child else {
                panic!("expected a resolved child");
            };
            builder.push(agentview::pom::MixedContent::node(child.clone()));
        }
        Ok(())
    })
    .unwrap();
    resolve_artifact_document(Document::from_xml(node)).unwrap()
}

pub fn paragraph(value: &str) -> ResolvedDocument {
    resolve_artifact_document(Document::build(|blocks| {
        blocks.paragraph(|inline| {
            inline.try_text(value.to_owned()).unwrap();
        });
    }))
    .unwrap()
}

pub fn raw(value: &str) -> ResolvedDocument {
    resolve_artifact_document(Document::from_raw_text(value.to_owned())).unwrap()
}

pub fn attribute<'a>(node: &'a XmlNode, name: &str) -> Option<&'a str> {
    node.attributes()
        .iter()
        .find(|attribute| attribute.name().as_str() == name)
        .map(|attribute| attribute.value())
}

pub fn direct_text(node: &XmlNode) -> String {
    node.children()
        .iter()
        .map(|child| match child {
            ContentRef::Node(ContentNode::Text(text)) => text.value(),
            ContentRef::Node(ContentNode::RawText(text)) => text.value(),
            other => panic!("expected a data field, got {other:?}"),
        })
        .collect()
}

fn descendants<'a>(
    children: impl Iterator<Item = ContentRef<'a>>,
    nodes: &mut Vec<&'a ContentNode>,
) {
    for child in children {
        let ContentRef::Node(child) = child else {
            panic!("expected a resolved node")
        };
        nodes.push(child);
        match child {
            ContentNode::Xml(child) => descendants(child.children().iter(), nodes),
            ContentNode::Markdown(MarkdownNode::Paragraph(child)) => {
                descendants(child.children().iter(), nodes)
            }
            ContentNode::Markdown(MarkdownNode::Heading(child)) => {
                descendants(child.children().iter(), nodes)
            }
            ContentNode::Markdown(MarkdownNode::Strong(child)) => {
                descendants(child.children().iter(), nodes)
            }
            ContentNode::Markdown(MarkdownNode::List(child)) => {
                for item in child.items() {
                    descendants(item.children().iter(), nodes)
                }
            }
            _ => {}
        }
    }
}

pub trait InspectPom {
    fn nodes(&self) -> Vec<&ContentNode>;

    fn elements(&self, name: &str) -> Vec<&XmlNode> {
        self.nodes()
            .into_iter()
            .filter_map(|node| match node {
                ContentNode::Xml(node) if node.name().as_str() == name => Some(node),
                _ => None,
            })
            .collect()
    }

    fn has_element(&self, name: &str) -> bool {
        !self.elements(name).is_empty()
    }

    fn has_field(&self, name: &str, value: &str) -> bool {
        self.elements(name).into_iter().any(|node| {
            let children: Vec<_> = node.children().iter().collect();
            matches!(children.as_slice(), [ContentRef::Node(ContentNode::Text(text))] if text.value() == value)
                || matches!(children.as_slice(), [ContentRef::Node(ContentNode::RawText(text))] if text.value() == value)
                || (children.is_empty() && value.is_empty())
        })
    }

    fn has_text(&self, value: &str) -> bool {
        self.nodes().into_iter().any(|node| match node {
            ContentNode::Text(text) => text.value() == value,
            ContentNode::RawText(text) => text.value() == value,
            _ => false,
        })
    }

    fn has_attribute(&self, name: &str, value: &str) -> bool {
        self.nodes().into_iter().any(|node| match node {
            ContentNode::Xml(node) => attribute(node, name) == Some(value),
            _ => false,
        })
    }
}

impl InspectPom for ResolvedDocument {
    fn nodes(&self) -> Vec<&ContentNode> {
        let mut nodes = Vec::new();
        descendants(self.children().iter(), &mut nodes);
        nodes
    }
}

impl InspectPom for XmlNode {
    fn nodes(&self) -> Vec<&ContentNode> {
        let mut nodes = Vec::new();
        descendants(self.children().iter(), &mut nodes);
        nodes
    }
}
