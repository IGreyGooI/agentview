use agentview::component::advanced::experimental::{
    view as raw_view, StreamingChannelsView, ViewExt,
};
use agentview::prelude::*;

struct LocalChannels;

impl TurnChannels for LocalChannels {
    type Output = u8;
    type Live = String;
    type Commit = Never;
    type Diagnostic = Never;
}

#[agentview::view(component)]
fn multi_lane_stream() -> StreamingChannelsView<LocalChannels> {
    StreamingXml::<TurnEmission<LocalChannels>, Never>::new(XmlNode::new(
        XmlName::try_from("multi_lane").unwrap(),
    ))
    .init_state(())
    .into_view()
}

fn invalid_binding_remap() {
    let raw_view = raw_view(multi_lane_stream());
    let _ = raw_view.map_binding(|binding| binding.map_effect(|_| 1_u8));
}

fn main() {}
