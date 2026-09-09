#[path = "../../../examples/chess_agentview/chess_action.rs"]
#[allow(dead_code)] // The legacy action-only parser does not construct thought diagnostics.
mod chess_action;
mod chess_actions;
mod chess_agent;
#[path = "../../../examples/chess_agentview/chess_draw_state.rs"]
mod chess_draw_state;
mod chess_feedback;
mod chess_game_state;
mod chess_player;
mod game;
mod live;
mod model;
mod observability;
#[path = "../../../examples/chess_agentview/uci.rs"]
mod uci;

#[tokio::test]
#[ignore = "requires OPENAI_API_KEY and runs a live Responses API Chess game"]
async fn live_acceptance() {
    load_live_environment().expect("load live test environment");
    let config = live::LiveConfig::from_environment(|name| std::env::var_os(name))
        .expect("configure live Responses API Chess game");
    let evidence = live::run_live_game(config)
        .await
        .expect("complete live Responses API Chess game");
    live::validate_live_evidence(&evidence).expect("validate live Chess evidence");
}

fn load_live_environment() -> anyhow::Result<()> {
    match dotenvy::dotenv_override() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
