use agentview::view;

#[view(widget)]
fn invalid_component() -> agentview::component::advanced::experimental::View<()> {
    Ok(agentview::component::advanced::experimental::ComponentNode::empty())
}

fn main() {}
