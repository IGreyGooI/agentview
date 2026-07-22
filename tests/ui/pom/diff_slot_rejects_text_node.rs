use agentview::pom::{DiffSlot, DiffStrategy, TextNode};

fn main() {
    DiffSlot::present(DiffStrategy::Recursive, TextNode::new("not XML"));
}
