use agentview::pom::ResolvedDocument;

fn main() {
    let _: ResolvedDocument = serde_json::from_str("{}").unwrap();
}
