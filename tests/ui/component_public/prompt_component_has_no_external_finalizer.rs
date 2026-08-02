use agentview::component::prelude::*;

fn main() {
    let component: PromptComponent<()> = MountedFeature::system_only(durable_system(()));
    let _ = component.into_external_harness(
        (),
        EpochContractId::new("test/no-external-component-finalizer/v1").unwrap(),
    );
}
