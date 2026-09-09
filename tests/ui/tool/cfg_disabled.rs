#[agentview::tool]
#[cfg(any())]
fn disabled(value: String) -> Result<String, agentview::component::authoring::ToolError> {
    Ok(value)
}

fn main() {}
