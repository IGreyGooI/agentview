use std::sync::Arc;

use agentview::component::prelude::*;

use super::{chess_agent::ChessSnapshot, chess_game_state::side_name};

#[component]
pub(crate) fn chess_player(snapshot: Arc<ChessSnapshot>) -> Component {
    let side = side_name(snapshot.agent_side());
    view! {
        #[system_once]
        chess_player {
            identity { "You are the chess agent playing {side}." }
            objective {
                "Play the current game to the best of your ability and choose only an action that chess_actions marks available."
            }
            private_reasoning {
                "Privately verify the authoritative position and legal actions, examine checks, captures, threats, king safety, tactics, strategy, and draw implications, then choose. Never output chain-of-thought, analysis, hidden reasoning, or commentary."
            }
            state_updates {
                "A chess_game_state without rendering_mode is a complete replacement. A chess_game_state with rendering_mode=\"delta\" is an atomic patch over the latest authoritative state: omitted fields remain unchanged, replace overwrites its named field, and insert appends the enclosed history item."
            }
        }
    }
}
