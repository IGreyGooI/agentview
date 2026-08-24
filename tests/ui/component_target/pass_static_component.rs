use agentview::component::{prelude::*, ComponentHost};

#[derive(AgentView)]
#[agent_view(kind = "compile_note")]
struct CompileNote {
    #[view(text)]
    body: String,
}

#[derive(Clone, Copy)]
struct Props;

fn inline_note() -> Component {
    view! { note { "inline" } }
}

#[component]
fn turn(_props: Props, events: EventInput<ProviderEvent>) -> Component {
    let _cloned_route = events.clone();
    view! {
        #[developer]
        contract { version: "v1", "typed" }

        inline_note()
    }
}

#[component]
fn application_root(props: Props, events: EventInput<ProviderEvent>) -> Component {
    view! {
        #[system_once]
        protocol {
            nested { "stable" }
        }

        turn(props, events)
    }
}

fn main() {
    let _derived = CompileNote {
        body: String::from("derive helper and view! coexist"),
    };
    let _components = ComponentHost::new(application_root, Props);
}
