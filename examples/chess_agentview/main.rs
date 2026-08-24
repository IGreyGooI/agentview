use std::process::ExitCode;

mod chess_actions;
mod chess_agent;
mod chess_feedback;
mod chess_game_state;
mod chess_player;
mod entry;
mod game;
mod live;
mod model;
mod observability;
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
    if std::env::args_os().nth(1).is_some() {
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

fn load_live_environment() -> Result<(), EntryFailure> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(EntryFailure::LiveEnvironment),
    }
}
