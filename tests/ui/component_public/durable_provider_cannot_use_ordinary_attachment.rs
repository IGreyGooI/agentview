use agentview::component::advanced::provider::{
    DurableMountedProviderExecutor, MountedProviderEpoch,
};

fn attach_without_durable_identity<P>(provider: &P, epoch: MountedProviderEpoch)
where
    P: DurableMountedProviderExecutor<String>,
{
    let _ = provider.attach_epoch(epoch);
}

fn main() {}
