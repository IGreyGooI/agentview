use agentview::component::advanced::experimental::{binding, view as raw_view, ProvidedView};
use agentview::component::{view, PomView};

fn provided_child() -> ProvidedView<u8> {
    raw_view(binding("runtime", 7))
}

#[agentview::view(component)]
fn invalid_pom_component() -> PomView {
    view((provided_child(),))
}

fn main() {
    let _ = invalid_pom_component();
}
