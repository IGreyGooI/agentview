use agentview::provider::async_openai::AsyncOpenAiConfigError;

fn main() {
    let _ = AsyncOpenAiConfigError::InvalidChatCompletionsSerializedRequestBodyLimit;
    let _ = AsyncOpenAiConfigError::InvalidChatCompletionsFrameProfile;
    let _ = AsyncOpenAiConfigError::ChatCompletionsTargetIdentityExhausted;
}
