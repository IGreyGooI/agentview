//! First mounted POM component.
//!
//! The POM types through `hello_agent` are all of the component author's code.
//! The example-only host invoked by `main` opens one durable epoch and submits
//! two turn snapshots so the lifecycle is visible when the example runs.
//!
//! Run with:
//! `cargo run --example hello_world`

use agentview::component::prelude::*;
use support::run_turns;

#[path = "support/mounted_prompt_trace.rs"]
mod support;

#[derive(Clone)]
struct GreetingTurn {
    recipient: String,
}

#[derive(AgentView)]
#[agent_view(document)]
struct HelloSystemDocument {
    #[view(paragraph)]
    instruction: &'static str,
}

#[derive(AgentView)]
#[agent_view(document)]
struct HelloUserDocument {
    #[view(paragraph)]
    task: String,
}

#[view(component)]
fn hello_agent() -> PromptComponent<GreetingTurn> {
    prompt_component(
        // The host attaches this typed System POM once per durable epoch.
        HelloSystemDocument {
            instruction: "Greet the named person in one short sentence.",
        },
        // This pure closure builds a fresh typed User POM for every turn.
        |turn: &GreetingTurn| HelloUserDocument {
            task: format!("Say hello to {}.", turn.recipient),
        },
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // This is a stable deployment identity for the complete durable
    // System/runtime contract. It must not be generated per turn.
    let epoch_contract = EpochContractId::new("example/hello-world/v1")?;
    let harness = hello_agent().into_harness(epoch_contract);

    // Example-only host plumbing: application hosts own capture, provider I/O,
    // and call lifecycle. Component authors stop at `hello_agent` above.
    let trace = run_turns(
        harness,
        vec![
            GreetingTurn {
                recipient: "world".to_owned(),
            },
            GreetingTurn {
                recipient: "AgentView".to_owned(),
            },
        ],
    )
    .await?;

    println!("SYSTEM (attached once)\n{}", trace.system());
    for (index, user) in trace.users().iter().enumerate() {
        println!("\nUSER {}\n{}", index + 1, user);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attaches_one_system_and_renders_fresh_user_prompts() -> anyhow::Result<()> {
        let trace = run_turns(
            hello_agent().into_harness(EpochContractId::new("example/hello-world/test-v1")?),
            vec![
                GreetingTurn {
                    recipient: "Ada".to_owned(),
                },
                GreetingTurn {
                    recipient: "Grace".to_owned(),
                },
            ],
        )
        .await?;

        assert_eq!(
            trace.system(),
            "Greet the named person in one short sentence."
        );
        assert_eq!(trace.users(), ["Say hello to Ada.", "Say hello to Grace."]);
        Ok(())
    }
}
