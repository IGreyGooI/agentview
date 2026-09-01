use agentview::component::prelude::*;

#[derive(Clone)]
struct AsyncTaskProps {
    label: String,
    alternate: bool,
}

#[component]
fn async_task_authoring(props: AsyncTaskProps) -> Component {
    let direct_label = props.label.clone();
    use_future(move || async move {
        spawn(async move {
            let _ = direct_label;
        })
        .unwrap();
    });

    let qualified_label = props.label.clone();
    agentview::component::prelude::use_future(move || async move {
        agentview::component::prelude::spawn(async move {
            let _ = qualified_label;
        })
        .unwrap();
    });

    let direct: Coroutine<String> = use_coroutine(2, |mut inbox| async move {
        while let Some(message) = inbox.recv().await {
            spawn(async move {
                let _ = message;
            })
            .unwrap();
        }
    });
    let qualified: Coroutine<u64> = agentview::component::prelude::use_coroutine(
        1,
        |mut inbox: CoroutineInbox<u64>| async move {
            if let Some(message) = inbox.recv().await {
                agentview::component::prelude::spawn(async move {
                    let _ = message;
                })
                .unwrap();
            }
        },
    );

    // Conditional hook order is valid Rust authoring syntax. The runtime
    // rejects a changed committed topology rather than making it a macro error.
    if props.alternate {
        use_future(|| async {});
        let _ordered: Coroutine<()> = use_coroutine(1, |_inbox| async {});
    } else {
        let _ordered: Coroutine<()> = use_coroutine(1, |_inbox| async {});
        use_future(|| async {});
    }

    let _public_surface = (
        direct.clone(),
        direct.capacity(),
        direct.is_closed(),
        qualified,
    );
    let rendered_label = props.label.clone();
    view! { async_task_authoring { "{rendered_label}" } }
}

fn assert_send_sync<T: Send + Sync>() {}

fn check_send_error(error: CoroutineSendError<String>) -> String {
    let _ = (error.is_stale_mount(), error.is_closed());
    error.into_inner()
}

fn check_spawn_result(result: Result<(), SpawnError>) {
    let _ = result;
}

fn main() {
    assert_send_sync::<Coroutine<String>>();
    let _check_send_error: fn(CoroutineSendError<String>) -> String = check_send_error;
    check_spawn_result(Err(SpawnError::ContextUnavailable));
    let _component = async_task_authoring(AsyncTaskProps {
        label: String::from("tasks"),
        alternate: false,
    });
}
