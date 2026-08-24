use agentview::component::prelude::*;

fn main() {
    let value = "implemented";
    let _component = view! {
        "formatted {value}"
        md::paragraph {
            "inline "
            md::code_span { "{value}" }
        }
        item { "xml {value}" }
    };
}
