use agentview::component::advanced::provider::MountedProviderRequest;

fn main() {
    let _ = MountedProviderRequest::<String>::new(
        "call-1",
        Vec::new(),
        "user prompt".to_owned(),
        "test-model",
        128,
    );
    let _ = MountedProviderRequest {
        call_label: "call-1".into(),
        history: Vec::<String>::new(),
        user: "user prompt".to_owned(),
        model: "test-model".into(),
        max_tokens: 128,
    };
}
