use std::convert::Infallible;

use agentview::component::prelude::XmlStreamingToolCall;

fn main() {
    let _attribute_contract = XmlStreamingToolCall::contract("compile.xml-tool-call", "v1")
        .empty_element("selection")
        .required_attribute::<u32>("value")
        .on_decoded(|_value| async { Ok::<(), Infallible>(()) })
        .on_invalid(|_diagnostic| async { Ok::<(), Infallible>(()) });

    let _empty_contract = XmlStreamingToolCall::contract("compile.xml-empty", "v1")
        .empty_element("resign")
        .on_decoded(|| async { Ok::<(), Infallible>(()) })
        .on_invalid(|_diagnostic| async { Ok::<(), Infallible>(()) });
}
