use std::{collections::BTreeMap, fmt::Display};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticNode {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<SemanticChild>,
    intrinsic_diff_strategy: Option<IntrinsicDiffStrategy>,
    identity: Option<Box<SemanticFragment>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticFragment {
    Node(SemanticNode),
    Text(String),
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticField {
    Empty,
    Attr { name: String, value: String },
    Fragment(SemanticFragment),
    Fragments(Vec<SemanticFragment>),
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticDiffStrategy {
    Recursive,
    Replace,
    Append,
    Sequence,
    Set,
    Keyed(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IntrinsicDiffStrategy {
    Map,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticDiffSlot {
    pub(crate) field_name: &'static str,
    pub(crate) strategy: SemanticDiffStrategy,
    pub(crate) value: Option<SemanticFragment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SemanticChild {
    Fragment(SemanticFragment),
    DiffSlot(SemanticDiffSlot),
}

pub trait AgentView {
    fn render_root(&self) -> SemanticFragment;

    fn render_field(&self, field_name: &'static str) -> SemanticField;

    fn render_children(&self) -> Vec<SemanticFragment> {
        vec![self.render_root()]
    }
}

pub trait AgentViewRoot: AgentView {}

pub trait AgentViewCollect<Source: ?Sized>: Sized {
    fn collect(source: &Source) -> Self;
}

impl AgentView for String {
    fn render_root(&self) -> SemanticFragment {
        SemanticFragment::Text(self.clone())
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        SemanticField::Attr {
            name: field_name.to_owned(),
            value: self.clone(),
        }
    }
}

impl AgentView for str {
    fn render_root(&self) -> SemanticFragment {
        SemanticFragment::Text(self.to_owned())
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        SemanticField::Attr {
            name: field_name.to_owned(),
            value: self.to_owned(),
        }
    }
}

impl<T> AgentView for &T
where
    T: AgentView + ?Sized,
{
    fn render_root(&self) -> SemanticFragment {
        (*self).render_root()
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        (*self).render_field(field_name)
    }

    fn render_children(&self) -> Vec<SemanticFragment> {
        (*self).render_children()
    }
}

macro_rules! impl_scalar_agent_view {
    ($($ty:ty),* $(,)?) => {
        $(
            impl AgentView for $ty {
                fn render_root(&self) -> SemanticFragment {
                    SemanticFragment::Text(self.to_string())
                }

                fn render_field(&self, field_name: &'static str) -> SemanticField {
                    SemanticField::Attr {
                        name: field_name.to_owned(),
                        value: self.to_string(),
                    }
                }
            }
        )*
    };
}

impl_scalar_agent_view!(
    bool, char, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
);

impl<T> AgentView for Option<T>
where
    T: AgentView,
{
    fn render_root(&self) -> SemanticFragment {
        match self {
            Some(value) => value.render_root(),
            None => SemanticFragment::Node(SemanticNode::new("none")),
        }
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        match self {
            Some(value) => value.render_field(field_name),
            None => SemanticField::Empty,
        }
    }

    fn render_children(&self) -> Vec<SemanticFragment> {
        match self {
            Some(value) => value.render_children(),
            None => Vec::new(),
        }
    }
}

impl<T> AgentView for Vec<T>
where
    T: AgentView,
{
    fn render_root(&self) -> SemanticFragment {
        SemanticFragment::Node(render_list_node("list", self))
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        SemanticField::Fragment(SemanticFragment::Node(render_list_node(field_name, self)))
    }

    fn render_children(&self) -> Vec<SemanticFragment> {
        self.iter()
            .map(|item| SemanticFragment::Node(fragment_as_node(item.render_root())))
            .collect()
    }
}

impl<K, V> AgentView for BTreeMap<K, V>
where
    K: AgentView + Ord,
    V: AgentView,
{
    fn render_root(&self) -> SemanticFragment {
        SemanticFragment::Node(render_map_node("map", self))
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        SemanticField::Fragment(SemanticFragment::Node(render_map_node(field_name, self)))
    }
}

impl SemanticNode {
    pub fn new(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            attrs: Vec::new(),
            children: Vec::new(),
            intrinsic_diff_strategy: None,
            identity: None,
        }
    }

    pub fn element(tag: impl Into<String>, value: impl Into<String>) -> Self {
        let mut node = Self::new(tag);
        node.push_text(value);
        node
    }

    pub fn push_attr(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.attrs.push((name.into(), value.into()));
    }

    pub fn push_child(&mut self, child: SemanticNode) {
        self.push_fragment(SemanticFragment::Node(child));
    }

    pub fn push_fragment(&mut self, fragment: SemanticFragment) {
        self.children.push(SemanticChild::Fragment(fragment));
    }

    pub fn push_field(&mut self, field: SemanticField) {
        match field {
            SemanticField::Empty => {}
            SemanticField::Attr { name, value } => self.push_attr(name, value),
            SemanticField::Fragment(fragment) => self.push_fragment(fragment),
            SemanticField::Fragments(fragments) => {
                for fragment in fragments {
                    self.push_fragment(fragment);
                }
            }
        }
    }

    pub fn push_diff_field(
        &mut self,
        field_name: &'static str,
        strategy: SemanticDiffStrategy,
        field: SemanticField,
    ) {
        let value = field_as_optional_fragment(field_name, field);
        self.children
            .push(SemanticChild::DiffSlot(SemanticDiffSlot {
                field_name,
                strategy,
                value,
            }));
    }

    pub fn push_text(&mut self, text: impl Into<String>) {
        self.push_fragment(SemanticFragment::Text(text.into()));
    }

    pub fn push_comment(&mut self, comment: impl Into<String>) {
        self.push_fragment(SemanticFragment::Comment(comment.into()));
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find_map(|(attr_name, value)| (attr_name == name).then_some(value.as_str()))
    }

    pub(crate) fn tag(&self) -> &str {
        &self.tag
    }

    pub(crate) fn attrs(&self) -> &[(String, String)] {
        &self.attrs
    }

    pub(crate) fn children(&self) -> &[SemanticChild] {
        &self.children
    }

    pub(crate) fn diff_slots(&self) -> impl Iterator<Item = &SemanticDiffSlot> {
        self.children.iter().filter_map(|child| match child {
            SemanticChild::Fragment(_) => None,
            SemanticChild::DiffSlot(slot) => Some(slot),
        })
    }

    pub(crate) fn has_rendered_children(&self) -> bool {
        self.children.iter().any(|child| match child {
            SemanticChild::Fragment(_) => true,
            SemanticChild::DiffSlot(slot) => slot.value.is_some(),
        })
    }

    pub(crate) fn intrinsic_diff_strategy(&self) -> Option<&IntrinsicDiffStrategy> {
        self.intrinsic_diff_strategy.as_ref()
    }

    pub(crate) fn identity(&self) -> Option<&SemanticFragment> {
        self.identity.as_deref()
    }

    fn set_identity(&mut self, identity: Option<SemanticFragment>) {
        self.identity = identity.map(Box::new);
    }
}

pub fn render_agent_view_xml(view: &impl AgentView) -> String {
    render_semantic_fragment_xml(&view.render_root())
}

pub fn render_agent_view_diff_xml<T>(view: &T, previous: &T) -> Option<String>
where
    T: AgentView,
{
    crate::semantic_diff::diff_agent_views(view, previous)
        .map(|fragment| render_semantic_fragment_xml(&fragment))
}

pub fn render_semantic_fragment_xml(fragment: &SemanticFragment) -> String {
    render_fragment(fragment, 0)
}

pub fn render_semantic_node_xml(node: &SemanticNode) -> String {
    render_node(node, 0)
}

pub fn view_value(value: &impl Display) -> String {
    value.to_string()
}

pub fn render_children_field(value: &impl AgentView) -> SemanticField {
    SemanticField::Fragments(value.render_children())
}

fn field_as_node(field_name: &'static str, field: SemanticField) -> SemanticNode {
    match field {
        SemanticField::Empty => unreachable!("empty fields do not have a semantic fragment"),
        SemanticField::Attr { name, value } => SemanticNode::element(name, value),
        SemanticField::Fragment(SemanticFragment::Node(node)) => node,
        SemanticField::Fragment(SemanticFragment::Text(text)) => {
            SemanticNode::element(field_name, text)
        }
        SemanticField::Fragment(SemanticFragment::Comment(comment)) => {
            let mut node = SemanticNode::new(field_name);
            node.push_comment(comment);
            node
        }
        SemanticField::Fragments(fragments) => {
            let mut node = SemanticNode::new(field_name);
            for fragment in fragments {
                node.push_fragment(fragment);
            }
            node
        }
    }
}

fn field_as_optional_fragment(
    field_name: &'static str,
    field: SemanticField,
) -> Option<SemanticFragment> {
    match field {
        SemanticField::Empty => None,
        field => Some(SemanticFragment::Node(field_as_node(field_name, field))),
    }
}

fn render_list_node<T>(tag: impl Into<String>, items: &[T]) -> SemanticNode
where
    T: AgentView,
{
    let mut node = SemanticNode::new(tag);
    for item in items {
        push_list_item(&mut node, item.render_root());
    }
    node
}

fn push_list_item(parent: &mut SemanticNode, fragment: SemanticFragment) {
    match fragment {
        SemanticFragment::Node(node) => parent.push_child(node),
        SemanticFragment::Text(text) => parent.push_child(SemanticNode::element("item", text)),
        SemanticFragment::Comment(comment) => {
            let mut item = SemanticNode::new("item");
            item.push_comment(comment);
            parent.push_child(item);
        }
    }
}

fn render_map_node<K, V>(tag: impl Into<String>, entries: &BTreeMap<K, V>) -> SemanticNode
where
    K: AgentView + Ord,
    V: AgentView,
{
    let mut node = SemanticNode::new(tag);
    node.intrinsic_diff_strategy = Some(IntrinsicDiffStrategy::Map);
    for (key, value) in entries {
        node.push_child(render_map_entry(key, value));
    }
    node
}

fn render_map_entry<K, V>(key: &K, value: &V) -> SemanticNode
where
    K: AgentView,
    V: AgentView,
{
    let mut entry = SemanticNode::new("entry");
    let key_field = key.render_field("key");
    entry.set_identity(field_as_optional_fragment("key", key_field.clone()));
    entry.push_field(key_field);
    entry.push_field(value.render_field("value"));
    entry
}

fn fragment_as_node(fragment: SemanticFragment) -> SemanticNode {
    match fragment {
        SemanticFragment::Node(node) => node,
        SemanticFragment::Text(text) => SemanticNode::element("item", text),
        SemanticFragment::Comment(comment) => {
            let mut node = SemanticNode::new("item");
            node.push_comment(comment);
            node
        }
    }
}

fn render_node(node: &SemanticNode, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let attrs = render_attrs(&node.attrs);
    let visible_fragments = node
        .children
        .iter()
        .filter_map(|child| match child {
            SemanticChild::Fragment(fragment) => Some(fragment),
            SemanticChild::DiffSlot(slot) => slot.value.as_ref(),
        })
        .collect::<Vec<_>>();

    match visible_fragments.as_slice() {
        [] => format!("{indent}<{}{} />", node.tag, attrs),
        [SemanticFragment::Text(text)] => {
            format!(
                "{indent}<{}{}>{}</{}>",
                node.tag,
                attrs,
                escape_text(text),
                node.tag
            )
        }
        _ => {
            let rendered_children = node
                .children
                .iter()
                .filter_map(|child| render_child(child, depth + 1))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{indent}<{}{}>\n{}\n{indent}</{}>",
                node.tag, attrs, rendered_children, node.tag
            )
        }
    }
}

fn render_child(child: &SemanticChild, depth: usize) -> Option<String> {
    match child {
        SemanticChild::Fragment(fragment) => Some(render_fragment(fragment, depth)),
        SemanticChild::DiffSlot(slot) => slot
            .value
            .as_ref()
            .map(|fragment| render_fragment(fragment, depth)),
    }
}

fn render_fragment(fragment: &SemanticFragment, depth: usize) -> String {
    match fragment {
        SemanticFragment::Node(node) => render_node(node, depth),
        SemanticFragment::Text(text) => format!("{}{}", "  ".repeat(depth), escape_text(text)),
        SemanticFragment::Comment(comment) => {
            format!("{}<!-- {} -->", "  ".repeat(depth), escape_comment(comment))
        }
    }
}

fn render_attrs(attrs: &[(String, String)]) -> String {
    attrs
        .iter()
        .map(|(name, value)| format!(" {}=\"{}\"", name, escape_attr(value)))
        .collect::<String>()
}

fn escape_text(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(raw: &str) -> String {
    escape_text(raw)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn escape_comment(raw: &str) -> String {
    raw.replace("--", "- -")
}
