#![allow(deprecated, reason = "legacy ComponentEvents UI fixture")]

use agentview::component::prelude::*;

#[derive(ComponentEvents)]
enum Events {
    Finished,
}

fn main() {}
