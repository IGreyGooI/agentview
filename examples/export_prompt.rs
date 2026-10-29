//! Export the current Component view for an external model without model history.
//!
//! Run `cargo run --no-default-features --example export_prompt`; no credentials are needed.

use std::io::{self, Write};

use agentview::component::{prelude::*, ComponentHost};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Route {
    Train,
    Ferry,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SelectRoute {
    route: Route,
}

#[component]
fn route_picker() -> Component {
    let selected = use_signal(String::new);
    let current = selected.with(Clone::clone).expect("mounted selection");

    view! {
        #[system_once]
        "Choose a route from the current options using select_route."

        route_request {
            destination { "Harbor office" }
            options { "train: 20 minutes; ferry: 35 minutes" }
            selected { "{current}" }
        }

        NativeToolCall {
            name: "select_route",
            description: "Record the selected route.",
            on_call: move |input: SelectRoute| selected.set(match input.route {
                Route::Train => "train".to_owned(),
                Route::Ferry => "ferry".to_owned(),
            }),
        }

        #[user(ephemeral, repeat)]
        "Prefer the quickest available route."
    }
}

fn main() -> anyhow::Result<()> {
    let mut host = ComponentHost::new_root(|_| route_picker(), ());
    let prompt: String = host.render()?.projection().render_prompt()?;

    // A caller can pass this String to its external decision API.
    io::stdout().lock().write_all(prompt.as_bytes())?;
    Ok(())
}
