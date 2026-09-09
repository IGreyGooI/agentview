use agentview::component::prelude::*;

#[tool]
fn lookup(#[serde(flatten)] query: String) -> Result<String, ToolError> {
    Ok(query)
}

fn main() {}
