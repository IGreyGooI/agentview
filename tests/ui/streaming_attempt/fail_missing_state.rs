include!("support.inc");

fn main() {
    XmlStreamingToolCall::new::<Channels>("test")
        .without_live()
        .without_publication()
        .build();
}
