use agentview::component::prelude::*;

#[tool]
fn echo<T>(value: T) -> Result<T, ToolError> {
    Ok(value)
}

fn main() {}
