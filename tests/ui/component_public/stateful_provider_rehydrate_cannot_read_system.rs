use agentview::component::advanced::provider::ProviderSessionRehydrateRequest;

fn cannot_resend_system(request: ProviderSessionRehydrateRequest<'_>) {
    let _ = request.system();
}

fn main() {}
