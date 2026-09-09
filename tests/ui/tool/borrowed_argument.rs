use agentview::component::prelude::*;

#[tool]
fn lookup(query: &str) -> Result<String, ToolError> {
    Ok(query.to_owned())
}

fn main() {}
