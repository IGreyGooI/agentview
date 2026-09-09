use agentview::component::prelude::*;

struct Lookup;

impl Lookup {
    #[tool]
    fn lookup(&self, value: String) -> Result<String, ToolError> {
        Ok(value)
    }
}

fn main() {}
