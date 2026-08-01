use agentview::component::advanced::experimental::{view as raw_view, StreamingChannelsView};
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

fn invalid_single_lane_remap() {
    let raw_view = raw_view(multi_lane_stream());
    let _ = raw_view.map_output::<LocalChannels>(|_| 1);
}

fn main() {}
