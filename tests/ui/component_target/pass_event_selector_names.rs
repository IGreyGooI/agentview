use agentview::component::prelude::*;

#[derive(ComponentEvents)]
enum Events {
    HttpRequest(String),
    HTTPResponse(u16),
    ClockExpired(u64),
}

fn main() {
    let _ = Events::HTTP_REQUEST;
    let _ = Events::HTTP_RESPONSE;
    let _ = Events::CLOCK_EXPIRED;
}
