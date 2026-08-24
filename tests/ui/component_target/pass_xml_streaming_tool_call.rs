use agentview::component::prelude::XmlStreamingToolCall;

fn main() {
    let _contract = XmlStreamingToolCall::contract("compile.xml-tool-call", "v1")
        .empty_element("selection")
        .required_attribute::<u32>("value")
        .exactly_one();
}
