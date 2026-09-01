use agentview::component::execution::ComponentReactionRuntime;

fn main() {
    let _ = std::mem::size_of::<ComponentReactionRuntime<(), (), ()>>();
}
