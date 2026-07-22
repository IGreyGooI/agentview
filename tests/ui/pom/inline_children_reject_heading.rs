use agentview::pom::{BlockContent, HeadingLevel, HeadingNode, InlineChildren};

fn main() {
    let mut children = InlineChildren::new();
    children.push(BlockContent::heading(HeadingNode::new(
        HeadingLevel::H1,
        InlineChildren::new(),
    )));
}
