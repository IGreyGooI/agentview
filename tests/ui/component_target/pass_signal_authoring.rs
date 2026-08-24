use agentview::component::prelude::*;

#[component]
fn retained_counter() -> Component {
    let count = use_signal(|| 0_u64);
    let rendered = count.with(|count| *count).unwrap();

    let task_signal = count.clone();
    let _task = tokio::spawn(async move {
        task_signal.update(|count| *count += 1).unwrap();
        task_signal.set(10).unwrap();
    });

    view! { retained_count { "{rendered}" } }
}

fn assert_send_sync<T: Send + Sync>() {}

fn main() {
    assert_send_sync::<Signal<u64>>();
    let _component = retained_counter();
}
