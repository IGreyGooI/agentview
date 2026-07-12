//! Minimal AgentViewApp hello world.
//!
//! Run with:
//! `cargo run --example hello_world`

use std::sync::{Arc, Mutex};

use agentview::agent::DefaultContextState;
use agentview::prelude::*;

#[derive(Debug)]
struct HelloState {
    greeting: String,
    name: Option<String>,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "hello")]
struct HelloView {
    greeting: String,

    #[view(diff)]
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct HelloViewBuilder;

#[async_trait::async_trait]
impl ContextViewBuilder for HelloViewBuilder {
    type Source = Arc<Mutex<HelloState>>;
    type View = HelloView;

    async fn capture(&self, source: &Self::Source) -> Self::View {
        let state = source.lock().unwrap();
        HelloView {
            greeting: state.greeting.clone(),
            name: state.name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct HelloLayout;

impl PromptLayout for HelloLayout {
    fn system_template(&self) -> &'static str {
        "{{ instructions }}"
    }

    fn user_template(&self) -> &'static str {
        "{{ task }}"
    }
}

#[derive(Default)]
struct NameSink {
    name: Option<String>,
}

#[async_trait::async_trait]
impl TurnSink<ControlReply> for NameSink {
    type Output = Option<String>;

    async fn on_event(&mut self, reply: ControlReply) {
        self.name = match reply {
            ControlReply::Text(text) => Some(text.trim().to_owned()),
            ControlReply::Structured(_) => None,
        };
    }

    async fn finish(self: Box<Self>) -> Self::Output {
        self.name.filter(|name| !name.is_empty())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = Arc::new(Mutex::new(HelloState {
        greeting: "Hello".to_owned(),
        name: None,
    }));

    let view_model = DefaultAgentViewModel::new(
        HelloLayout,
        HelloViewBuilder,
        PromptSystemVars {
            instructions: "Ask for a name, then say hello.".to_owned(),
            output_schema: None,
        },
        IdentityTransform,
    );

    let (mut app, _awake): (AgentViewApp<_, Turn, ()>, _) = AgentViewApp::new(
        view_model,
        Arc::clone(&source),
        PromptContext::<Turn, DefaultContextState>::without_system(),
    );

    let snapshot = app.observe("Ask the caller for their name.").await?;
    println!(
        "observe epoch={} turn={}",
        snapshot.view_epoch, snapshot.turn_id
    );
    println!(
        "view: {}",
        snapshot
            .view
            .render_full(&TemplateEngine::new())
            .await?
            .as_str()
    );
    println!("prompt: {}", snapshot.turn_prompt.task);

    let update = app
        .act_with_sink(
            &snapshot.turn_id,
            ControlReply::text("world"),
            NameSink::default(),
            |session, source, name| {
                if let Some(name) = name {
                    source.lock().unwrap().name = Some(name.clone());
                    session.push_history(Turn::user(format!("name = {name}")));
                }
                Ok(())
            },
            "Say hello to the named caller.",
        )
        .await?;

    let next = update
        .snapshot()
        .ok_or_else(|| anyhow::anyhow!("hello world example expected a full update"))?;
    println!("update epoch={} turn={}", next.view_epoch, next.turn_id);
    println!(
        "view: {}",
        next.view
            .render_full(&TemplateEngine::new())
            .await?
            .as_str()
    );
    println!("prompt: {}", next.turn_prompt.task);

    Ok(())
}
