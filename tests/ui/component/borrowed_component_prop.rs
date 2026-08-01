#![allow(unused_imports)]

use agentview::prelude::{PomView, view};

#[agentview::view(component)]
fn borrowed_contract(value: &str) -> PomView {
    assert_eq!(value, "not owned");
    view(())
}

fn main() {
}
