use std::sync::Arc;

use agentview::component::{
    advanced::{
        lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
        provider::MountedProviderEpoch,
    },
    component, system_view, Never, TurnChannels,
};
use agentview::llm_call::TextTurnEvent;

struct Channels;

impl TurnChannels for Channels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

fn system(_: SystemMountContext<'_, ()>) -> SystemView<Channels> {
    system_view(component(()))
}

fn main() {
    let epoch = mount_system_epoch(&(), system).unwrap();
    let catalog = epoch.provider_tool_catalog();
    let _ = epoch.provider_epoch();
    let _ = MountedProviderEpoch::new(Arc::from("system"), catalog);
}
