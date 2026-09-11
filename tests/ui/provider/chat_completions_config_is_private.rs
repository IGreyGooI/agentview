use agentview::provider::async_openai::AsyncOpenAiTransportConfig;

fn main() {
    let _ = AsyncOpenAiTransportConfig::with_chat_completions_serialized_request_body_limit;
    let _ = AsyncOpenAiTransportConfig::with_chat_completions_frame_constraints;
}
