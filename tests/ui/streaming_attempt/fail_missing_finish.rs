include!("support.inc");

fn main() {
    XmlStreamingToolCall::new::<Channels>("test")
        .state_with(|_| Ok::<_, std::convert::Infallible>(()))
        .element(element(), |handlers| handlers.on_complete(|_, _| StreamingToolUpdate::none()))
        .without_live()
        .without_publication()
        .build();
}
