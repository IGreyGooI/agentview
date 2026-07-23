use agentview::templates::TurnArtifact;

fn main() {
    let _ = TurnArtifact {
        kind: "parser_error".into(),
        payload: Box::new(String::from("<parser_error>raw</parser_error>")),
    };
}
