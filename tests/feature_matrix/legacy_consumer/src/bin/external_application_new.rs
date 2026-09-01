use agentview::component::{execution::ExternalApplication, prelude::view};

fn main() {
    let _application = ExternalApplication::new(|_events| view! { legacy_external {} });
}
