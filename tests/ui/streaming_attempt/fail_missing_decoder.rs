include!("support.inc");

fn main() {
    XmlStreamingToolCall::new::<Channels>("test")
        .state_with(|_| Ok::<_, std::convert::Infallible>(()))
        .element(XmlToolElement::text("say"), |handlers| {
            handlers.on_complete(|_, _| StreamingToolUpdate::none())
        });
}
