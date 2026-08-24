use agentview::component::prelude::Signal;

fn legacy_write_only_handle(signal: Signal<u64>) {
    let _ = signal.setter();
}

fn main() {}
