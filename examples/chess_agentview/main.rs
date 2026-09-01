use std::process::ExitCode;

mod application_state;
mod chess_action;
mod chess_action_component;
mod chess_application;
mod chess_draw_state;
mod live_provider;
mod uci;

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the shared scripted provider exposes cases used by other example tests"
)]
#[path = "../support/scripted_provider.rs"]
mod scripted_provider;

use chess_application::{outcome_name, ChessApplication};
use live_provider::LiveConfig;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("chess_agentview failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    if let Some(argument) = arguments.next() {
        if (argument == "--help" || argument == "-h") && arguments.next().is_none() {
            print_help();
            return Ok(());
        }
        anyhow::bail!("usage: chess_agentview");
    }

    load_dotenv()?;
    let live = LiveConfig::from_environment(|name| std::env::var_os(name))?;
    let provider = live.build_provider()?;
    let config = live.application_config()?;

    let application = ChessApplication::mount(config, provider)?;
    let result = application.run().await?;

    println!("outcome={}", outcome_name(&result.outcome));
    println!("final_fen={}", result.final_board);
    println!(
        "moves={}",
        result
            .committed_moves
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(())
}

fn load_dotenv() -> anyhow::Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn print_help() {
    println!("Usage: chess_agentview");
    println!("Required: OPENAI_API_KEY");
    println!("Optional: AGENTVIEW_MODEL, OPENAI_BASE_URL, AGENTVIEW_STOCKFISH_BIN");
    println!("Optional: AGENTVIEW_CHESS_PLY_LIMIT, AGENTVIEW_ENGINE_NODES");
}
