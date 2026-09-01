#![allow(deprecated, reason = "legacy ComponentEvents UI fixture")]

use agentview::component::prelude::*;

#[derive(ComponentEvents)]
enum Events {
    HttpRequest(String),
    HTTPRequest(String),
}

fn main() {}
