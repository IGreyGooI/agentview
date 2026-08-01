use agentview::prelude::*;

struct RootChannels;

impl TurnChannels for RootChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[derive(Debug)]
struct Selection;

impl BindingInstance<RootChannels> for Selection {}

fn durable_leaf() -> DurableComponent<RootChannels> {
    durable_binding_factory(
        RuntimeContract::new("test.selection", "v1").unwrap(),
        Document::from_xml(XmlNode::new(XmlName::try_from("selection").unwrap())),
        RuntimeRoute::xml("selection").unwrap(),
        || Selection,
    )
}

fn invalid_component() -> Component<RootChannels> {
    component((durable_leaf(),))
}

fn main() {}
