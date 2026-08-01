use agentview::component::advanced::experimental::{StreamingProvidedView, StreamingValueView};
use agentview::prelude::*;

struct RootChannels;

enum RootOutput {
    Value(u8),
}

enum RootDiagnostic {
    Local(String),
}

impl TurnChannels for RootChannels {
    type Output = RootOutput;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = RootDiagnostic;
}

#[agentview::view(component)]
fn local_stream() -> StreamingValueView<u8, String> {
    StreamingXml::new(XmlNode::new(XmlName::try_from("local_stream").unwrap()))
        .init_state(())
        .into_view()
}

#[agentview::view(component)]
fn invalid_channel() -> StreamingProvidedView<RootChannels> {
    local_stream().map_output(RootOutput::Value)
}

fn main() {
    let _ = invalid_channel();
}
