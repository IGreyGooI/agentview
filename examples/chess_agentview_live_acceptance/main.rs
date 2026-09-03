use std::{io::Write, process::ExitCode};

#[path = "../chess_agentview/chess_action.rs"]
mod chess_action;
mod chess_actions;
mod chess_agent;
#[path = "../chess_agentview/chess_draw_state.rs"]
mod chess_draw_state;
mod chess_feedback;
mod chess_game_state;
mod chess_player;
mod entry;
mod game;
mod live;
mod model;
mod observability;
#[path = "../chess_agentview/uci.rs"]
mod uci;

use entry::EntryFailure;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let stderr = std::io::stderr();
            ExitCode::from(entry::write_entry_failure(&mut stderr.lock(), failure))
        }
    }
}

async fn run() -> Result<(), EntryFailure> {
    let mut arguments = std::env::args_os().skip(1);
    if let Some(argument) = arguments.next() {
        if (argument == "--help" || argument == "-h") && arguments.next().is_none() {
            let stdout = std::io::stdout();
            return write_help(&mut stdout.lock());
        }
        return Err(EntryFailure::InvalidArguments);
    }
    load_live_environment()?;
    let config = live::LiveConfig::from_environment(|name| std::env::var_os(name))
        .map_err(|reason| EntryFailure::LiveConfiguration { reason })?;
    let evidence = live::run_live_game(config)
        .await
        .map_err(|reason| EntryFailure::LiveRun { reason })?;
    live::validate_live_evidence(&evidence)
        .map_err(|reason| EntryFailure::LivePostTerminalValidation { reason })?;

    let stdout = std::io::stdout();
    entry::write_live_game_summary(&mut stdout.lock(), &evidence)
}

fn write_help(output: &mut impl Write) -> Result<(), EntryFailure> {
    output
        .write_all(b"Usage: chess_agentview_live_acceptance\n")
        .and_then(|()| output.flush())
        .map_err(|_| EntryFailure::StdoutWrite)
}

fn load_live_environment() -> Result<(), EntryFailure> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(EntryFailure::LiveEnvironment),
    }
}
