use std::convert::Infallible;

use agentview::component::prelude::*;

struct OwnedInput(String);

#[component]
fn preparation_authoring() -> Component {
    use_preparation(|| async { Ok::<(), Infallible>(()) });

    agentview::component::prelude::use_preparation(|| async { Ok::<(), Infallible>(()) });

    let input = OwnedInput(String::from("owned"));
    use_preparation(move || {
        let consumed = input;
        async move {
            drop(consumed.0);
            Ok::<(), Infallible>(())
        }
    });

    view! { preparation_authoring {} }
}

fn main() {
    let _component = preparation_authoring();
}
