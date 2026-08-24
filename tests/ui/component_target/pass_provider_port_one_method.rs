use agentview::component::execution::{
    ApplicationHost, ProviderEvent, ProviderEventStream, ProviderFault, ProviderPort,
    RenderedProjection,
};
use agentview::component::prelude::*;
use agentview::component::ComponentHost;
use async_trait::async_trait;

struct MinimalProvider;

#[async_trait]
impl ProviderPort for MinimalProvider {
    async fn execute<'a>(
        &'a mut self,
        _projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[component]
fn application_root(_props: (), _events: EventInput<ProviderEvent>) -> Component {
    view! { compile_gate { "current boundary" } }
}

#[tokio::main]
async fn main() {
    let mut components = ComponentHost::new(application_root, ());
    let mut host = ApplicationHost::new(MinimalProvider);
    let _reaction = host.dispatch_llm_reaction(&mut components).await;
}
