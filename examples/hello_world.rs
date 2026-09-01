//! Minimal Component composition without provider I/O.

use agentview::{
    component::{execution::RenderedProjection, prelude::*, ComponentHost},
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};

#[derive(Clone)]
struct HelloProps {
    recipient: String,
}

#[component]
fn greeting_policy() -> Component {
    view! {
        #[system_once]
        greeting_policy { "Greet the named person in one short sentence." }
    }
}

#[component]
fn greeting_request(recipient: String) -> Component {
    view! {
        greeting_request {
            recipient { "{recipient}" }
        }
    }
}

#[component]
fn hello_application(props: HelloProps) -> Component {
    view! {
        greeting_policy()
        greeting_request(props.recipient)
    }
}

fn render_hello(recipient: &str) -> anyhow::Result<RenderedProjection> {
    let mut components = ComponentHost::new_root(
        hello_application,
        HelloProps {
            recipient: recipient.to_owned(),
        },
    );
    Ok(components.render()?.projection().clone())
}

fn projection_text(projection: &RenderedProjection) -> anyhow::Result<String> {
    Ok(projection
        .to_transcript()?
        .items()
        .iter()
        .filter_map(|item| match item {
            CanonicalInputItem::Instruction { pom, .. }
            | CanonicalInputItem::Message { pom, .. } => Some(render_pom_document(pom)),
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("\n"))
}

fn main() -> anyhow::Result<()> {
    let projection = render_hello("world")?;
    println!("{}", projection_text(&projection)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_policy_and_request_components() {
        let projection = render_hello("Ada").expect("hello projection renders");
        let identities = projection
            .nodes()
            .iter()
            .map(|node| node.identity())
            .collect::<Vec<_>>();

        assert!(identities
            .iter()
            .any(|name| name.contains("greeting_policy")));
        assert!(identities
            .iter()
            .any(|name| name.contains("greeting_request")));
    }
}
