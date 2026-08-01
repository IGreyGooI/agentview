//! P3 mount-plan inspection example. This compiles and validates heterogeneous
//! declarations without installing them into AgentLoop. Provider dispatch is
//! executable: each declared tool group creates a fresh per-attempt dispatcher
//! with typed results and updates. This example intentionally stops before that
//! attempt runtime; see `pom_provider_tools` for dispatch, Live, publication,
//! replay, collision, and abort behavior.

use std::convert::Infallible;

use agentview::{
    component::advanced::{
        experimental::*,
        provider::{
            provider_tool, ProviderDispatchContext, ProviderDispatchUpdate, ProviderDispatcher,
            ProviderToolCall, ProviderToolResponse,
        },
    },
    prelude::*,
};
use serde_json::json;

struct ToolChannels;

impl TurnChannels for ToolChannels {
    type Output = u32;
    type Live = String;
    type Commit = Never;
    type Diagnostic = String;
}

struct HarnessChannels;

#[derive(Debug)]
struct HarnessOutput {
    _selection: u32,
}

#[derive(Debug)]
struct HarnessLive {
    _append: String,
}

#[derive(Debug)]
struct HarnessDiagnostic {
    _message: String,
}

impl TurnChannels for HarnessChannels {
    type Output = HarnessOutput;
    type Live = HarnessLive;
    type Commit = Never;
    type Diagnostic = HarnessDiagnostic;
}

#[derive(Debug)]
struct XmlSelectionRuntime {
    _buffer: String,
}

impl BindingInstance<ToolChannels> for XmlSelectionRuntime {}

#[derive(Debug)]
struct MoveToolDispatcher {
    _pending_calls: Vec<String>,
}

#[async_trait::async_trait]
impl ProviderDispatcher<ToolChannels> for MoveToolDispatcher {
    type Error = Infallible;

    async fn dispatch(
        &mut self,
        _context: &ProviderDispatchContext,
        call: ProviderToolCall,
    ) -> Result<ProviderDispatchUpdate<ToolChannels>, Self::Error> {
        self._pending_calls.push(
            call.invocation_id()
                .expect("runtime validates invocation identity before dispatch")
                .to_owned(),
        );
        Ok(ProviderDispatchUpdate::new(
            ProviderToolResponse::success("moved"),
            StreamUpdate::new(),
        ))
    }
}

fn xml_document(name: &str) -> Document {
    Document::from_xml(XmlNode::new(
        XmlName::try_from(name).expect("static example tag is valid"),
    ))
}

#[agentview::view(component)]
fn interaction_tools() -> Component<ToolChannels> {
    let move_tool = ProviderToolSpec::new(
        "move_piece",
        "Move one piece to a legal square",
        json!({
            "type": "object",
            "properties": { "square": { "type": "string" } },
            "required": ["square"]
        }),
    )
    .expect("static provider tool contract is valid");

    component((
        binding_factory(
            "selection_stream",
            xml_document("select_intent"),
            RuntimeRoute::xml("select_intent").expect("static runtime route is valid"),
            || XmlSelectionRuntime {
                _buffer: String::new(),
            },
        ),
        provider_tool("move_piece", move_tool, || MoveToolDispatcher {
            _pending_calls: Vec::new(),
        }),
    ))
}

#[agentview::view(component)]
fn harness_root() -> Component<HarnessChannels> {
    component((
        system(interaction_tools().map_channels(TurnChannelMap::new(
            |selection| HarnessOutput {
                _selection: selection,
            },
            |append| HarnessLive { _append: append },
            Never::absurd,
            |message| HarnessDiagnostic { _message: message },
        ))),
        user(xml_document("task")),
    ))
}

fn main() -> anyhow::Result<()> {
    let plan = compile_mount_provided(harness_root())?;
    let system = resolve_system_document(plan.system_document().clone());
    let (user, _) =
        resolve_user_document(plan.user_document().clone(), &UserDocumentCursor::default())?;

    println!("SYSTEM\n{}", render_pom_document(&system)?);
    println!("\nUSER\n{}", render_pom_document(&user)?);

    println!("\nBINDING FACTORIES");
    for factory in plan.binding_factories().factories() {
        println!("{} route={}", factory.id(), factory.route());
    }

    println!("\nPROVIDER CAPABILITIES");
    for capability in plan.provider_capabilities().capabilities() {
        for spec in capability.specs() {
            println!(
                "{} tool={} schema={}",
                capability.id(),
                spec.name(),
                spec.input_schema()
            );
        }
    }

    Ok(())
}
