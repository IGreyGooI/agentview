use crate::component::{execution::command::CommandFeedback, prelude::*};

#[derive(AgentView)]
#[agent_view(kind = "command_feedback")]
pub(crate) struct CommandFeedbackView {
    #[view(element)]
    pub(crate) sequence: u64,
    pub(crate) input_feedback: Option<InputFeedbackView>,
    pub(crate) records: Vec<CommandResultView>,
}

#[derive(AgentView)]
#[agent_view(kind = "input_feedback")]
pub(crate) struct InputFeedbackView {
    pub(crate) code: String,
    #[view(text)]
    pub(crate) text: String,
}

#[derive(AgentView)]
#[agent_view(kind = "command_result")]
pub(crate) struct CommandResultView {
    pub(crate) sequence: u64,
    pub(crate) command: String,
    pub(crate) status: &'static str,
    pub(crate) code: Option<String>,
    #[view(text)]
    pub(crate) text: String,
}

#[component]
pub(crate) fn command_feedback(feedback: Signal<CommandFeedback>) -> Component {
    let snapshot = feedback
        .with(CommandFeedback::snapshot)
        .expect("mounted root command feedback");
    view! {
        #[diff(slot = "command_feedback")]
        { snapshot }
    }
}
