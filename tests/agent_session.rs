use agentview::prelude::*;

#[test]
fn new_agent_session_starts_without_a_view_cursor() {
    let mut context = PromptContext::<Turn, usize>::new("system");
    context.push_history(Turn::user("committed"));

    let session = AgentSession::<Turn, usize, String>::new(context);

    assert_eq!(session.context().system(), Some("system"));
    assert_eq!(session.context().history().len(), 1);
    assert!(session.view_cursor().is_none());
}

#[test]
fn context_state_can_be_updated_through_the_session() {
    let context = PromptContext::<Turn, usize>::new("system");
    let mut session = AgentSession::<Turn, usize, String>::new(context);

    *session.context_state_mut() = 7;

    assert_eq!(*session.context().context_state(), 7);
}

#[test]
fn session_exposes_safe_prompt_context_mutations() {
    let context = PromptContext::<Turn>::without_system();
    let mut session = AgentSession::<Turn, (), String>::new(context);

    session.set_system_once("system");
    session.push_history(Turn::user("first"));
    session.extend_history([Turn::assistant("second")]);
    session.push_working_set(Turn::user("working"));
    session.replace_working_set(vec![Turn::user("replacement working")]);

    assert_eq!(session.context().system(), Some("system"));
    assert_eq!(session.context().history().len(), 2);
    assert_eq!(session.context().working_set().len(), 1);
    assert_eq!(
        &*session.context().working_set()[0].text,
        "replacement working"
    );

    session.clear_working_set();
    assert!(session.context().working_set().is_empty());
}
