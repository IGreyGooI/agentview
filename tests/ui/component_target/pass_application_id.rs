use agentview::component::{prelude::*, ApplicationId};

#[component]
fn identified_component() -> Component {
    let local: ApplicationId = use_application_id();
    let qualified: ApplicationId = agentview::component::prelude::use_application_id();
    assert_eq!(local, qualified);
    view! { identified_component {} }
}

fn main() {
    let _component = identified_component();
}
