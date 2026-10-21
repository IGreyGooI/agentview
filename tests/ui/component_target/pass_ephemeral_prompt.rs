use agentview::component::{prelude::*, ComponentHost};

#[component]
fn nested_context() -> Component {
    view! {
        #[developer(repeat)]
        "Policy."

        #[assistant]
        "Authored example."
    }
}

#[component]
fn ephemeral_context(_props: ()) -> Component {
    view! {
        #[user(ephemeral, repeat)]
        "Current state."

        #[developer(repeat, ephemeral)]
        { String::from("Current workspace context.") }

        #[user(repeat, ephemeral)]
        #[diff(slot = "workspace")]
        workspace { "Complete state." }

        #[developer(ephemeral, repeat)]
        nested_context()
    }
}

fn main() {
    let mut host = ComponentHost::new_root(ephemeral_context, ());
    host.render().unwrap();
}
