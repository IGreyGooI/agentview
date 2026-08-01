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

fn provided_child() -> Component<RootChannels> {
    binding_factory(
        "runtime",
        Document::from_xml(XmlNode::new(XmlName::try_from("runtime").unwrap())),
        RuntimeRoute::xml("runtime").unwrap(),
        || Runtime,
    )
}

#[agentview::view(component)]
fn invalid_pom_parent() -> PomView {
    pom_view((provided_child(),))
}

fn main() {
    let _ = invalid_pom_parent();
}
