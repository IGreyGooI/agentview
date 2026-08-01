use agentview::component::advanced::experimental::{binding, view as raw_view, ProvidedView};
use agentview::component::{pom_view, PomView};

fn provided_child() -> ProvidedView<u8> {
    raw_view(binding("runtime", 7))
}

fn invalid_pom_root() -> PomView {
    pom_view((provided_child(),))
}

fn main() {
    let _ = invalid_pom_root();
}
