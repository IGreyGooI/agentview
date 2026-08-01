use agentview::{
    component::{advanced::experimental::FinishedStreamingAttempt, Never, TurnChannels},
    llm_call::TextTurnEvent,
};

struct Channels;

impl TurnChannels for Channels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

async fn reuse_finished(mut finished: FinishedStreamingAttempt<Channels>) {
    finished
        .on_event(TextTurnEvent::TextDelta(String::new()))
        .await;
}

fn main() {}
