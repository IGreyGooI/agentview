use agentview::{
    component::{
        advanced::{experimental::binding, lifecycle::SystemView},
        system_view,
    },
    prelude::*,
};

struct RootChannels;

impl TurnChannels for RootChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[derive(Debug)]
struct LegacyRuntime;

fn invalid_system_view() -> SystemView<RootChannels> {
    system_view(binding("legacy-runtime", LegacyRuntime))
}

fn main() {
    let _ = invalid_system_view;
}
