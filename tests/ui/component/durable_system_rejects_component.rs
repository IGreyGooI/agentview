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

fn ordinary_runtime_component() -> Component<RootChannels> {
    binding_factory(
        "selection",
        Document::from_xml(XmlNode::new(XmlName::try_from("selection").unwrap())),
        RuntimeRoute::xml("selection").unwrap(),
        || Selection,
    )
}

fn invalid_durable_system() -> DurableSystem<RootChannels> {
    durable_system((ordinary_runtime_component(),))
}

fn main() {}
