//! Byte attribution for a complete exported prompt.
//!
//! Source metadata is collected from the typed projection during rendering.
//! Profiling never parses prompt text and never changes application delivery.

use std::{collections::HashMap, ops::Range};

/// One Component instance represented in a rendered prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptComponent {
    pub(crate) identity: String,
    pub(crate) parent: Option<String>,
}

impl PromptComponent {
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn parent(&self) -> Option<&str> {
        self.parent.as_deref()
    }
}

/// What a source contributed to the exported prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PromptSourceKind {
    Content,
    System,
    NativeTool,
    Decision,
    Formatting,
}

/// The owner of a byte range in an exported prompt.
///
/// Framework formatting and declarations without Component ownership remain
/// explicit rather than being charged to an arbitrary Component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PromptSource {
    Component {
        identity: String,
        kind: PromptSourceKind,
    },
    Framework {
        kind: PromptSourceKind,
    },
    Unattributed {
        kind: PromptSourceKind,
    },
}

impl PromptSource {
    pub fn component_identity(&self) -> Option<&str> {
        match self {
            Self::Component { identity, .. } => Some(identity),
            Self::Framework { .. } | Self::Unattributed { .. } => None,
        }
    }

    pub const fn kind(&self) -> PromptSourceKind {
        match self {
            Self::Component { kind, .. }
            | Self::Framework { kind }
            | Self::Unattributed { kind } => *kind,
        }
    }
}

/// One nonempty source range, measured in UTF-8 bytes of the complete prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSourceSpan {
    pub(crate) range: Range<usize>,
    pub(crate) source: PromptSource,
}

impl PromptSourceSpan {
    pub fn byte_range(&self) -> Range<usize> {
        self.range.clone()
    }

    pub fn source(&self) -> &PromptSource {
        &self.source
    }
}

/// An exported complete prompt together with its Component source map.
///
/// This profiles the current projection with empty external history. It does
/// not measure provider envelopes, retained conversation, cached tokens, or
/// provider-reported usage. Source ranges partition the entire exported text,
/// including formatting and action declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub(crate) text: String,
    pub(crate) spans: Vec<PromptSourceSpan>,
    pub(crate) components: Vec<PromptComponent>,
}

impl RenderedPrompt {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn source_spans(&self) -> &[PromptSourceSpan] {
        &self.spans
    }

    pub fn components(&self) -> &[PromptComponent] {
        &self.components
    }

    /// Measure UTF-8 bytes contributed by each Component.
    ///
    /// Every source byte belongs to exactly one Component, the framework, or
    /// an unattributed declaration. Component self counts plus framework and
    /// unattributed counts equal `total_bytes()`. Inclusive counts also include
    /// descendants and must not be summed together. No tokenizer is needed.
    /// This is a measurement of the exported text, not a token estimate.
    pub fn profile_bytes(&self) -> PromptByteProfile {
        let indexes = self
            .components
            .iter()
            .enumerate()
            .map(|(index, component)| (component.identity.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut components = self
            .components
            .iter()
            .cloned()
            .map(|component| ComponentByteUsage {
                component,
                self_bytes: 0,
                inclusive_bytes: 0,
            })
            .collect::<Vec<_>>();
        let mut framework_bytes = 0;
        let mut unattributed_bytes = 0;
        for span in &self.spans {
            let bytes = span.range.len();
            match &span.source {
                PromptSource::Component { identity, .. } => {
                    components[indexes[identity.as_str()]].self_bytes += bytes;
                }
                PromptSource::Framework { .. } => framework_bytes += bytes,
                PromptSource::Unattributed { .. } => unattributed_bytes += bytes,
            }
        }

        // Source maps are constructed by the renderer from the explicit
        // Component tree. They cannot be supplied or mutated by callers.
        let parents = self
            .components
            .iter()
            .map(|component| component.parent.as_deref().map(|parent| indexes[parent]))
            .collect::<Vec<_>>();
        for index in 0..components.len() {
            let bytes = components[index].self_bytes;
            let mut ancestor = Some(index);
            while let Some(ancestor_index) = ancestor {
                components[ancestor_index].inclusive_bytes += bytes;
                ancestor = parents[ancestor_index];
            }
        }

        PromptByteProfile {
            components,
            total_bytes: self.text.len(),
            framework_bytes,
            unattributed_bytes,
        }
    }
}

/// UTF-8 byte costs for one Component instance.
///
/// Self counts exclude descendants. Inclusive counts include descendants, so
/// summing inclusive rows would double-count nested Components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentByteUsage {
    component: PromptComponent,
    self_bytes: usize,
    inclusive_bytes: usize,
}

impl ComponentByteUsage {
    pub fn identity(&self) -> &str {
        self.component.identity()
    }

    pub fn parent(&self) -> Option<&str> {
        self.component.parent()
    }

    pub const fn self_bytes(&self) -> usize {
        self.self_bytes
    }

    pub const fn inclusive_bytes(&self) -> usize {
        self.inclusive_bytes
    }
}

/// Exact UTF-8 byte sizes of one complete exported prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptByteProfile {
    components: Vec<ComponentByteUsage>,
    total_bytes: usize,
    framework_bytes: usize,
    unattributed_bytes: usize,
}

impl PromptByteProfile {
    pub fn components(&self) -> &[ComponentByteUsage] {
        &self.components
    }

    /// Component self bytes plus framework and unattributed bytes.
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub const fn framework_bytes(&self) -> usize {
        self.framework_bytes
    }

    pub const fn unattributed_bytes(&self) -> usize {
        self.unattributed_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(identity: &str, parent: Option<&str>) -> PromptComponent {
        PromptComponent {
            identity: identity.to_owned(),
            parent: parent.map(str::to_owned),
        }
    }

    fn owned(range: Range<usize>, identity: &str, kind: PromptSourceKind) -> PromptSourceSpan {
        PromptSourceSpan {
            range,
            source: PromptSource::Component {
                identity: identity.to_owned(),
                kind,
            },
        }
    }

    #[test]
    fn byte_costs_are_exclusive_and_roll_up_through_explicit_parents() {
        let prompt = RenderedPrompt {
            text: "abcdefghij".to_owned(),
            spans: vec![
                owned(0..2, "parent", PromptSourceKind::System),
                owned(2..5, "child/with/slashes", PromptSourceKind::Content),
                PromptSourceSpan {
                    range: 5..7,
                    source: PromptSource::Framework {
                        kind: PromptSourceKind::Formatting,
                    },
                },
                owned(7..8, "parent", PromptSourceKind::NativeTool),
                PromptSourceSpan {
                    range: 8..10,
                    source: PromptSource::Unattributed {
                        kind: PromptSourceKind::Decision,
                    },
                },
            ],
            // Parent links work independently of declaration order and names.
            components: vec![
                component("child/with/slashes", Some("parent")),
                component("parent", None),
                component("empty", Some("child/with/slashes")),
            ],
        };
        let profile = prompt.profile_bytes();
        let [child, parent, empty] = profile.components() else {
            panic!("every Component has a usage row");
        };
        assert_eq!(child.self_bytes(), 3);
        assert_eq!(child.inclusive_bytes(), 3);
        assert_eq!(child.parent(), Some("parent"));
        assert_eq!(parent.self_bytes(), 3);
        assert_eq!(parent.inclusive_bytes(), 6);
        assert_eq!(empty.self_bytes(), 0);
        assert_eq!(empty.inclusive_bytes(), 0);
        assert_eq!(profile.framework_bytes(), 2);
        assert_eq!(profile.unattributed_bytes(), 2);
        assert_eq!(profile.total_bytes(), 10);
        assert_eq!(
            profile
                .components()
                .iter()
                .map(ComponentByteUsage::self_bytes)
                .sum::<usize>()
                + profile.framework_bytes()
                + profile.unattributed_bytes(),
            profile.total_bytes(),
        );
    }

    #[test]
    fn multibyte_content_is_measured_in_utf8_bytes() {
        let prompt = RenderedPrompt {
            text: "你🙂".to_owned(),
            spans: vec![owned(0..7, "unicode", PromptSourceKind::Content)],
            components: vec![component("unicode", None)],
        };
        let profile = prompt.profile_bytes();
        assert_eq!(profile.total_bytes(), 7);
        assert_eq!(profile.components()[0].self_bytes(), 7);
        assert_eq!(profile.components()[0].inclusive_bytes(), 7);
        assert_eq!(profile.framework_bytes(), 0);
        assert_eq!(profile.unattributed_bytes(), 0);
    }

    #[test]
    fn empty_prompts_keep_zero_cost_components() {
        let prompt = RenderedPrompt {
            text: String::new(),
            spans: Vec::new(),
            components: vec![component("empty", None)],
        };
        let profile = prompt.profile_bytes();
        assert_eq!(profile.total_bytes(), 0);
        assert_eq!(profile.framework_bytes(), 0);
        assert_eq!(profile.unattributed_bytes(), 0);
        assert_eq!(profile.components()[0].identity(), "empty");
        assert_eq!(profile.components()[0].inclusive_bytes(), 0);
    }
}
