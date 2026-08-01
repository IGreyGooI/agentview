//! Independent consumer review: the smallest mounted POM application shape.
//!
//! Run with:
//! `cargo run --example mounted_author_review`

use agentview::component::prelude::*;
use support::mount;

#[path = "support/mounted_prompt_trace.rs"]
mod support;

#[derive(Clone)]
struct GreetingTurn {
    recipient: String,
}

#[derive(Clone, AgentView)]
#[agent_view(document)]
struct GreeterSystem {
    #[view(paragraph)]
    policy: &'static str,
}

#[derive(Clone, AgentView)]
#[agent_view(document)]
struct GreetingUser {
    #[view(paragraph)]
    request: String,
}

#[view(component)]
fn greeter() -> PromptComponent<GreetingTurn> {
    prompt_component(
        GreeterSystem {
            policy: "Reply with a concise greeting.",
        },
        |turn: &GreetingTurn| GreetingUser {
            request: format!("Greet {}.", turn.recipient),
        },
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let definition = greeter().into_harness(EpochContractId::new("review/greeter/v1")?);
    let mut mounted = mount(definition).await?;

    mounted
        .run_turn(GreetingTurn {
            recipient: "Mira".to_owned(),
        })
        .await?;
    mounted
        .run_turn(GreetingTurn {
            recipient: "Noah".to_owned(),
        })
        .await?;

    let trace = mounted.trace()?;
    println!("SYSTEM (once)\n{}", trace.system());
    for user in trace.users() {
        println!("\nUSER\n{user}");
    }
    Ok(())
}
