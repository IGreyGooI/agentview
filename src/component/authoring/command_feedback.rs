use crate::component::{execution::command::CommandFeedback, prelude::*, EventContext};

#[derive(AgentView)]
#[agent_view(kind = "origin")]
pub(crate) struct CommandOriginView {
    application_id: String,
    reaction_id: Option<String>,
    event_id: String,
    caused_by: Option<String>,
}

impl From<&EventContext> for CommandOriginView {
    fn from(context: &EventContext) -> Self {
        Self {
            application_id: context.application_id().to_string(),
            reaction_id: context.reaction_id().map(|id| id.to_string()),
            event_id: context.event_id().to_string(),
            caused_by: context.caused_by().map(|id| id.to_string()),
        }
    }
}

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
    pub(crate) origin: CommandOriginView,
    pub(crate) code: String,
    #[view(text)]
    pub(crate) text: String,
}

#[derive(AgentView)]
#[agent_view(kind = "command_result")]
pub(crate) struct CommandResultView {
    pub(crate) origin: CommandOriginView,
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
