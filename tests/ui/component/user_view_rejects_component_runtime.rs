use agentview::prelude::*;

struct RootChannels;

impl TurnChannels for RootChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[derive(Debug)]
struct Runtime;

impl BindingInstance<RootChannels> for Runtime {}

fn invalid_user_view() -> UserView {
    user_view(component(binding_factory(
        "selection",
        Document::from_xml(XmlNode::new(XmlName::try_from("selection").unwrap())),
        RuntimeRoute::xml("selection").unwrap(),
        || Runtime,
    )))
}

fn main() {}
