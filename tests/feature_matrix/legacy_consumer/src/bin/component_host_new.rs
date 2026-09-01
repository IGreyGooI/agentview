use agentview::component::{prelude::view, ComponentHost};

fn main() {
    let _host = ComponentHost::new(|_: (), _events| view! { legacy_host {} }, ());
}
