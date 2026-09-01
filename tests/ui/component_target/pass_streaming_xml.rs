use std::convert::Infallible;

use agentview::component::prelude::*;

#[component]
fn lifecycle_subscription() -> Component {
    view! {
        {
            StreamingXml::tag("speak")
                .on_open(|element| async move {
                    let _attributes = element.attributes;
                    Ok::<(), Infallible>(())
                })
                .on_stream(|element| async move {
                    let _cumulative_snapshot = element.content;
                    Ok::<(), Infallible>(())
                })
                .on_complete(|element| async move {
                    let _complete_content = element.content;
                    Ok::<(), Infallible>(())
                })
                .on_invalid(|_diagnostic| async move { Ok::<(), Infallible>(()) })
        }
    }
}

#[component]
fn open_only() -> Component {
    view! {
        {
            StreamingXml::tag("open_only")
                .on_open(|_element: XmlElement| async move { Ok::<(), Infallible>(()) })
        }
    }
}

fn main() {
    let _lifecycle = lifecycle_subscription();
    let _open = open_only();
    let _direct: Component = StreamingXml::tag("complete_only")
        .on_complete(|_element| async move { Ok::<(), Infallible>(()) })
        .into();
}
