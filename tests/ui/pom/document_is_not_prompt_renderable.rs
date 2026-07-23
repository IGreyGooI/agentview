use agentview::pom::Document;
use agentview::templates::PromptRenderable;

fn require_prompt_renderable<T: PromptRenderable>() {}

fn main() {
    require_prompt_renderable::<Document>();
}
