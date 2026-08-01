use agentview::component::advanced::provider::ProviderEpochRehydrateRequest;

fn rejects_system(request: ProviderEpochRehydrateRequest<'_>) {
    let _ = request.system();
}

fn main() {}
