include!("support.inc");

fn main() {
    XmlStreamingToolCall::new::<Channels>("test")
        .state_with(|_| Ok::<_, std::convert::Infallible>(()))
        .finish(|_, _| StreamingToolDecision::Accept(StreamingToolUpdate::none()));
}
