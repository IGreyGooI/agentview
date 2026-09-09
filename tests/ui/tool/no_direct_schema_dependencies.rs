use agentview::component::prelude::*;

#[tool]
fn echo(value: String) -> Result<String, ToolError> {
    Ok(value)
}

fn main() {
    let _: Component = NativeToolCall::new(echo);
}
