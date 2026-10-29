use crate::transcript::CanonicalInputItem;

use super::{ProviderFault, RenderedProjection};

pub(crate) fn render_projection_prompt(
    projection: &RenderedProjection,
) -> Result<String, ProviderFault> {
    let capacity = projection
        .nodes()
        .iter()
        .map(|node| node.items().len())
        .sum();
    let mut sections = Vec::with_capacity(capacity);
    let mut ephemeral = Vec::new();
    for node in projection.nodes() {
        for (index, item) in node.items().iter().enumerate() {
            let section = render_prompt_item(item)?;
            if node.is_ephemeral_item(index) {
                ephemeral.push(section);
            } else {
                sections.push(section);
            }
        }
    }
    sections.extend(ephemeral);
    Ok(sections.join("\n\n"))
}

fn render_prompt_item(item: &CanonicalInputItem) -> Result<String, ProviderFault> {
    super::prompt_export::render_prompt_item(item).map_err(|_| match item {
        CanonicalInputItem::ProviderExtension(extension) => ProviderFault::model_rejected(format!(
            "provider-neutral prompt renderer cannot render provider extension {}:{} v{}",
            extension.provider(),
            extension.capability(),
            extension.schema_version()
        )),
        _ => ProviderFault::model_rejected(
            "provider-neutral prompt renderer could not render canonical POM",
        ),
    })
}
