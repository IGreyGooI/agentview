use agentview::component::prelude::*;

struct Invalid;

impl Invalid {
    #[component]
    fn render(self) -> Component {
        view! {}
    }
}

fn main() {}
