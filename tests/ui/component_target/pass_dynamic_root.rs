use agentview::component::prelude::*;

#[derive(AgentView)]
#[agent_view(kind = "compile_note")]
struct CompileNote {
    #[view(text)]
    body: String,
}

fn inline_note() -> Component {
    view! { inline { "component" } }
}

fn main() {
    let note = CompileNote {
        body: String::from("typed"),
    };
    let optional_note = Some(CompileNote {
        body: String::from("optional"),
    });
    let history = vec![CompileNote {
        body: String::from("history"),
    }];
    let component = inline_note();
    let diff_note = CompileNote {
        body: String::from("diffable"),
    };

    let _component = view! {
        {component}
        {note}
        {optional_note}
        {history}
    };

    let _diff_component = view! {
        #[diff(slot = "typed_note")]
        {diff_note}
    };
}
