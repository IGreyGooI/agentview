use agentview::component::prelude::*;

struct Channels;
impl StreamingToolChannels for Channels {
    type Output = NoStreamingValue;
    type Live = NoStreamingValue;
    type Commit = NoStreamingValue;
    type Diagnostic = ();
}

fn element() -> XmlElementContract<(), String> {
    XmlToolElement::text("say").decode(|_| Ok(()), |_, text| Ok(text.to_owned()))
}
