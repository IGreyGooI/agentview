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

fn model_input(label: &str) -> String {
    format!("## {label}\n\n<state>ready & waiting</state>")
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
    let system = String::from("# System\n\n<protocol>preserve this source</protocol>");
    let raw_history = String::from("- first\n- second");
    let borrowed = String::from("<borrowed>literal XML source</borrowed>");
    let borrowed_string = &borrowed;
    let borrowed_str: &str = "plain\nmultiline\nMarkdown";
    let raw_diff = String::from("<complete>compare as one raw block</complete>");

    let _component = view! {
        {component}
        {note}
        {optional_note}
        {history}

        #[system_once]
        {system}

        #[developer]
        {raw_history}

        #[developer]
        {borrowed_string}

        #[user]
        {borrowed_str}

        #[assistant]
        {model_input("input")}
    };

    let _diff_component = view! {
        #[diff(slot = "typed_note")]
        {diff_note}

        #[diff(slot = "raw_document")]
        {raw_diff}
    };
}
