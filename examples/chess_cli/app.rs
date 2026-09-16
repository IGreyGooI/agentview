use agentview::component::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;

use super::game::GameState;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MoveInput {
    /// One canonical lowercase UCI move from the current legal_moves, such as e2e4.
    uci: String,
}

#[component]
pub(super) fn chess_application() -> Component {
    use_wait_for_command();
    let game = use_signal(GameState::default);
    let played_game = game.clone();
    let undone_game = game.clone();
    let new_game = game.clone();
    let content = game
        .with(GameState::projection)
        .expect("mounted chess state");
    view! {
        { content }

        Action {
            name: "move",
            description: "Play a legal move for the current side. Choose uci from legal_moves.",
            enabled: game.with(GameState::can_move).expect("mounted chess state"),
            on_call: move |input: MoveInput| {
                played_game.update(|game| game.play(&input.uci))
            },
        }
        Action {
            name: "undo",
            description: "Undo the most recent move.",
            enabled: game.with(GameState::can_undo).expect("mounted chess state"),
            on_call: move || undone_game.update(GameState::undo),
        }
        Action {
            name: "new",
            description: "Start a new game and discard the current move history.",
            on_call: move || new_game.update(GameState::new_game),
        }
    }
}
