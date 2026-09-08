use agentview::component::prelude::*;

#[component]
fn application_exit_component(label: String) -> Component {
    let direct = use_application_exit();
    let qualified = agentview::component::prelude::use_application_exit();
    let _exits = (direct, qualified);

    view! { application_exit_component { "{label}" } }
}

fn assert_send_sync<T: Send + Sync>() {}

fn main() {
    assert_send_sync::<ApplicationExitHandle>();
    let _component = application_exit_component(String::from("exit"));
}
