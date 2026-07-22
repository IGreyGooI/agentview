use agentview::pom::{DiffSlot, DiffStrategy, XmlName, XmlNode};

fn main() {
    let role = XmlName::try_from("agent_context").unwrap();
    let xml = XmlNode::new(role.clone());
    DiffSlot::present(role, DiffStrategy::Recursive, xml);
}
