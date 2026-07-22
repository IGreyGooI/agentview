use agentview::pom::{BlockChildren, Document};

fn main() {
    Document::new(BlockChildren::new()).children_mut().clear();
}
