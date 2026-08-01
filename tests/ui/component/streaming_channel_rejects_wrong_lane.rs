use agentview::component::advanced::experimental::{StreamingProvidedView, StreamingValueView};
use agentview::prelude::*;

struct RootChannels;

enum RootOutput {
    Value(u8),
}

enum RootLive {
    Value(u8),
}

enum RootDiagnostic {
    Local(String),
}

impl TurnChannels for RootChannels {
    type Output = RootOutput;
    type Live = RootLive;
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
    local_stream()
        .map_live(RootOutput::Value)
        .map_diagnostic(RootDiagnostic::Local)
}

fn main() {
    let _ = invalid_channel();
}
