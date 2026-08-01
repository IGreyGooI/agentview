use agentview::{
    component::{
        advanced::lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
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

fn invalid_system(cx: SystemMountContext<'_, ()>) -> SystemView<RootChannels> {
    let _ = cx.task();
    system_view(component(()))
}

fn main() {
    let _ = mount_system_epoch(&(), invalid_system);
}
