//! A persistent chess application operated by any CLI-capable model.
//!
//! Build: cargo build --no-default-features --example chess_cli
//! Observe: target/debug/examples/chess_cli start
//! Act: printf '%s\n' '{"action":"move","input":{"uci":"e2e4"}}' | target/debug/examples/chess_cli

mod app;
mod game;

use agentview::component::execution::StdinApplication;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    StdinApplication::run(app::chess_application).await
}
