use agentview::pom::{HeadingLevel, HeadingNode, InlineChildren};

fn main() {
    let _ = HeadingNode {
        level: HeadingLevel::H1,
        children: InlineChildren::new(),
    };
}
