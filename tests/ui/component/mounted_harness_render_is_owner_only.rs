use agentview::component::{MountedHarnessDefinition, TurnChannels, UserTurnContext};

fn bypass_capture<C: TurnChannels>(definition: &MountedHarnessDefinition<C, ()>) {
    let _ = definition.render_user(UserTurnContext::new(&()));
}

fn main() {}
