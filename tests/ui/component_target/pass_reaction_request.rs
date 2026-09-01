use agentview::component::prelude::*;

#[component]
fn reaction_requests(label: String) -> Component {
    let direct = use_reaction_request();
    let qualified = agentview::component::prelude::use_reaction_request();
    let _requests = (direct, qualified);

    view! { reaction_requests { "{label}" } }
}

fn assert_send_sync<T: Send + Sync>() {}

fn main() {
    assert_send_sync::<ReactionRequest>();
    let _component = reaction_requests(String::from("request"));
}
