#![allow(unused_imports)]

use agentview::prelude::{view, PomView};

struct BorrowedProps<'a> {
    value: &'a str,
}

#[agentview::view(component)]
fn nested_borrowed_contract<'a>(props: BorrowedProps<'a>) -> PomView {
    let _ = props;
    view(())
}

fn main() {}
