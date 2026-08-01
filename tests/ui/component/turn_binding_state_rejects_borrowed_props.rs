use std::convert::Infallible;

use agentview::{
    component::{
        advanced::lifecycle::{mount_system_epoch, SystemMountContext, SystemView},
        system_view, TurnBindingCx,
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

struct TurnProps {
    value: String,
}

struct BorrowedState<'a> {
    value: &'a str,
}

fn system(_: SystemMountContext<'_, ()>) -> SystemView<RootChannels, TurnProps> {
    let contract = XmlNode::new(XmlName::try_from("borrowed").unwrap());
    system_view(
        StreamingXml::<TurnEmission<RootChannels>, Never>::new(contract)
            .try_state_with(
                |cx: &TurnBindingCx<'_, TurnProps, RootChannels>| -> Result<_, Infallible> {
                    Ok(BorrowedState {
                        value: &cx.props().value,
                    })
                },
            )
            .into_component(),
    )
}

fn main() {
    let _ = mount_system_epoch(&(), system);
}
