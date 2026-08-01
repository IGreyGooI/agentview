use agentview::component::{advanced::experimental::PublishedTurnReceipt, ProviderAttemptIdentity};

fn identity() -> ProviderAttemptIdentity {
    panic!("compile-fail fixture")
}

fn main() {
    let _receipt = PublishedTurnReceipt {
        identity: identity(),
    };
}
