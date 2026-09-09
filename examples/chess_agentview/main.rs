use std::process::ExitCode;

use agentview::component::execution::{Application, ApplicationFault, ReactionPort};
use anyhow::Context as _;
use tokio::sync::watch;

mod application_state;
mod chess_action;
mod chess_action_component;
mod chess_application;
mod chess_draw_state;
mod live_provider;
mod uci;

use chess_application::{
    chess_application, outcome_name, stop_stockfish, ChessConfig, ChessResult,
};
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

    let result = play_chess(config, provider).await?;

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

async fn play_chess(
    config: ChessConfig,
    provider: impl ReactionPort,
) -> anyhow::Result<ChessResult> {
    let (stop, stop_receiver) = watch::channel(false);
    let (result_sender, result_receiver) = watch::channel(None);
    let (cleanup_sender, mut cleanup_receiver) = watch::channel(None);
    let mut application = Application::mount(
        move || {
            chess_application(
                config.clone(),
                stop_receiver.clone(),
                result_sender.clone(),
                cleanup_sender.clone(),
            )
        },
        provider,
    )?;

    let result = application
        .run()
        .await
        .context("Chess model reaction failed")
        .and_then(|_| {
            result_receiver
                .borrow()
                .clone()
                .context("Chess application exited before producing a terminal result")
        });
    // Await UCI cleanup before shutdown aborts the component-owned coroutine.
    let cleanup = stop_stockfish(&stop, &mut cleanup_receiver).await;
    let shutdown = application.shutdown().await;
    finish_run(result, cleanup, shutdown)
}

fn finish_run(
    result: anyhow::Result<ChessResult>,
    actor_cleanup: anyhow::Result<()>,
    application_shutdown: Result<(), ApplicationFault>,
) -> anyhow::Result<ChessResult> {
    match result {
        Ok(result) => {
            actor_cleanup.context("Chess actor cleanup failed")?;
            application_shutdown.context("Application shutdown failed")?;
            Ok(result)
        }
        Err(operation) => {
            if let Err(cleanup) = actor_cleanup {
                anyhow::bail!(
                    "Chess operation failed ({operation:#}); Chess actor cleanup also failed ({cleanup:#})"
                );
            }
            if let Err(shutdown) = application_shutdown {
                anyhow::bail!(
                    "Chess operation failed ({operation:#}); Application shutdown also failed ({shutdown})"
                );
            }
            Err(operation)
        }
    }
}

fn load_dotenv() -> anyhow::Result<()> {
    match dotenvy::dotenv_override() {
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
