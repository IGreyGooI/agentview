use agentview::component::prelude::*;

#[tool(name = "lookup order")]
fn lookup(value: String) -> Result<String, ToolError> {
    Ok(value)
}

#[tool(name = "valid_alias")]
fn inspect(value: String) -> Result<String, ToolError> {
    Ok(value)
}

fn main() {}
