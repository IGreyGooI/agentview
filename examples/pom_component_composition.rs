//! Ordered canonical input contributed by several business Components.

use agentview::{
    component::{execution::RenderedProjection, prelude::*, ComponentHost},
    pom_renderer::render_pom_document,
    transcript::CanonicalInputItem,
};

#[derive(Clone)]
struct SupportCase {
    account_id: String,
    plan: String,
    request: String,
}

#[component]
fn support_policy() -> Component {
    view! {
        #[system_once]
        support_policy { "Resolve the request using only the supplied account context." }
    }
}

#[component]
fn account_context(account_id: String, plan: String) -> Component {
    view! {
        #[developer]
        account_context {
            account_id { "{account_id}" }
            plan { "{plan}" }
        }
    }
}

#[component]
fn customer_request(request: String) -> Component {
    view! {
        customer_request { "{request}" }
    }
}

#[component]
fn response_requirements() -> Component {
    view! {
        response_requirements { "Return a concise answer and the next action." }
    }
}

#[component]
fn support_application(props: SupportCase) -> Component {
    view! {
        support_policy()
        account_context(props.account_id, props.plan)
        customer_request(props.request)
        response_requirements()
    }
}

fn render_support_case() -> anyhow::Result<RenderedProjection> {
    let props = SupportCase {
        account_id: "acct-1042".to_owned(),
        plan: "team".to_owned(),
        request: "Explain why yesterday's export is unavailable.".to_owned(),
    };
    let mut components = ComponentHost::new_root(support_application, props);
    Ok(components.render()?.projection().clone())
}

fn rendered_item(item: &CanonicalInputItem) -> anyhow::Result<Option<String>> {
    match item {
        CanonicalInputItem::Instruction { pom, .. } | CanonicalInputItem::Message { pom, .. } => {
            Ok(Some(render_pom_document(pom)?))
        }
        _ => Ok(None),
    }
}

fn main() -> anyhow::Result<()> {
    let projection = render_support_case()?;
    for node in projection.nodes() {
        println!("COMPONENT {}", node.identity());
        for item in node.items() {
            if let Some(rendered) = rendered_item(item)? {
                println!("{rendered}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use agentview::{pom_renderer::render_pom_document, transcript::CanonicalInputItem};

    use super::*;

    #[test]
    fn preserves_component_and_canonical_input_order() {
        let projection = render_support_case().expect("support projection renders");
        let rendered = projection
            .nodes()
            .iter()
            .flat_map(|node| node.items())
            .filter_map(|item| match item {
                CanonicalInputItem::Instruction { pom, .. }
                | CanonicalInputItem::Message { pom, .. } => {
                    Some(render_pom_document(pom).expect("POM renders"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let account = rendered.find("<account_context>").expect("account context");
        let request = rendered
            .find("<customer_request>")
            .expect("customer request");
        let format = rendered
            .find("<response_requirements>")
            .expect("response format");
        assert!(account < request && request < format);
    }
}
