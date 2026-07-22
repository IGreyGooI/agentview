use agentview::pom::Document;

fn main() {
    let _: Document = serde_json::from_str("{}").unwrap();
}
