use agentview::component::prelude::Component;

#[agentview::view(component)]
fn invalid() -> Component {
    panic!("legacy attribute must not compile")
}

fn main() {}
