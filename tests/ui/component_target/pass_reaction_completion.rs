use std::convert::Infallible;

use agentview::component::prelude::*;

#[component]
fn reaction_completions(label: String) -> Component {
    let direct_label = label.clone();
    use_reaction_completion(move || async move {
        let _ = direct_label;
        Ok::<(), Infallible>(())
    });

    agentview::component::prelude::use_reaction_completion(move || async move {
        let _ = label;
        Ok::<(), Infallible>(())
    });

    view! { reaction_completions {} }
}

fn main() {
    let _component = reaction_completions(String::from("capture"));
}
