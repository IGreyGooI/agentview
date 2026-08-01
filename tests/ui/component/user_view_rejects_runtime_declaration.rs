use agentview::{component::user_view, prelude::*};

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

fn main() {
    let _ = user_view(binding_factory::<RootChannels, (), _>(
        "runtime",
        Document::from_xml(XmlNode::new(XmlName::try_from("runtime").unwrap())),
        RuntimeRoute::xml("runtime").unwrap(),
        || Runtime,
    ));
}
