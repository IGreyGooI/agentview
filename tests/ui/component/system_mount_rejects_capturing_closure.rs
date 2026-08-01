use agentview::{
    component::{advanced::lifecycle::mount_system_epoch, system_view},
    prelude::*,
};

struct RootChannels;

impl TurnChannels for RootChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

fn main() {
    let turn_data = String::from("must not enter System");
    let _ = mount_system_epoch(&(), move |_| {
        let _ = &turn_data;
        system_view::<RootChannels, ()>(component(()))
    });
}
