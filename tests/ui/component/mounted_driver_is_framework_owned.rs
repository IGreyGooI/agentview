use agentview::component::advanced::mounted::MountedAgentFactory;

fn main() {
    let _ = std::any::type_name::<dyn MountedAgentFactory<(), (), ()>>();
}
