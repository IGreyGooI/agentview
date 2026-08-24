use agentview::component::prelude::*;

fn main() {
    let priority = 3_u8;
    let instruction = String::from("Choose e2e4");
    let turn_id = "turn-7";
    let seconds = 4.25_f32;

    let _component = view! {
        "Turn {turn_id}: {instruction}"

        task {
            priority: priority,
            label: "clock {seconds:.1}s",
            instruction { "{instruction}" }
            clock { "{seconds:.1}s" }
        }
    };
}
