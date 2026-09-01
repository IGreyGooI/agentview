#![allow(deprecated, reason = "legacy ComponentEvents UI fixture")]

use agentview::component::prelude::*;

#[allow(non_camel_case_types)]
#[derive(ComponentEvents)]
enum Events {
    __(String),
}

fn main() {}
