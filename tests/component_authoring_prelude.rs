//! External-consumer proof for the narrow component-authoring import surface.

use agentview::component::{
    advanced::external::{
        ExternalActionRoute, ExternalReply, ExternalReplyContract, ExternalReplyContractId,
        MountedExternalHarnessDefinition,
    },
    prelude::*,
};
use agentview::control::ControlReply;

#[derive(Clone)]
struct GreetingProps {
    recipient: String,
}

#[derive(Clone, AgentView)]
#[agent_view(document)]
struct GreetingSystem {
    #[view(paragraph)]
    policy: &'static str,
}

#[derive(Clone, AgentView)]
#[agent_view(document)]
struct GreetingUser {
    #[view(paragraph)]
    request: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GreetingAction;

#[derive(Clone)]
struct GreetingReply {
    route: ExternalActionRoute,
    contract_id: ExternalReplyContractId,
}

impl GreetingReply {
    fn new() -> Self {
        Self {
            route: ExternalActionRoute::new("example.greeting.reply").unwrap(),
            contract_id: ExternalReplyContractId::new("example.greeting.reply/v1").unwrap(),
        }
    }
}

impl ExternalReplyContract for GreetingReply {
    type Action = GreetingAction;
    type Diagnostic = String;
    type System = GreetingReplyGrammar;

    fn system(&self) -> Self::System {
        GreetingReplyGrammar {
            syntax: "Reply with one greeting.",
        }
    }

    fn route(&self) -> &ExternalActionRoute {
        &self.route
    }

    fn contract_id(&self) -> &ExternalReplyContractId {
        &self.contract_id
    }

    fn decode(&self, _reply: &ControlReply) -> Result<Self::Action, Self::Diagnostic> {
        Ok(GreetingAction)
    }
}

#[derive(Clone, AgentView)]
#[agent_view(document)]
struct GreetingReplyGrammar {
    #[view(paragraph)]
    syntax: &'static str,
}

#[view(component)]
fn greeting_tool_contract() -> DurableComponent<NoTurnChannels, GreetingProps> {
    let contract = ProviderCapabilityContract::new(
        "example.greeting.lookup",
        "v1",
        [ProviderToolSpec::new(
            "lookup_recipient",
            "Read immutable recipient details",
            serde_json::json!({ "type": "object", "properties": {} }),
        )?],
    )?;
    durable_provider_contract(contract, ())
}

#[view(component)]
fn greeting() -> MountedFeature<NoTurnChannels, GreetingProps> {
    MountedFeature::try_new(
        durable_system((
            GreetingSystem {
                policy: "Reply with one short greeting.",
            }
            .build_root()?,
            greeting_tool_contract(),
        )),
        |turn| {
            GreetingUser {
                request: format!("Greet {}.", turn.props().recipient),
            }
            .build_root()
        },
    )
}

#[view(component)]
fn simple_greeting() -> PromptComponent<GreetingProps> {
    prompt_component(
        GreetingSystem {
            policy: "Reply with one short greeting.",
        },
        |props: &GreetingProps| GreetingUser {
            request: format!("Greet {}.", props.recipient),
        },
    )
}

fn assert_prompt_only<C>()
where
    C: TurnChannels<Output = Never, Live = Never, Commit = Never, Diagnostic = Never>,
{
}

#[test]
fn narrow_prelude_builds_a_prompt_only_mounted_component() {
    assert_prompt_only::<NoTurnChannels>();
    let definition = greeting()
        .into_harness(EpochContractId::new("test/component-authoring-prelude/v1").unwrap());
    let _ = definition;

    let definition = simple_greeting()
        .into_harness(EpochContractId::new("test/simple-prompt-component/v1").unwrap());
    let _ = definition;

    let external = MountedExternalHarnessDefinition::new(
        simple_greeting(),
        ExternalReply::new(GreetingReply::new()),
        EpochContractId::new("test/external-reply-authoring/v1").unwrap(),
    );
    assert_eq!(
        external.reply_contract_id().as_str(),
        "example.greeting.reply/v1"
    );
}
