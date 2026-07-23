//! Rust value to Prompt Object Model conversion.
//!
//! [`AgentView`] builds a complete, typed POM root. The derive implementation
//! uses the hidden field adapter API in this module to preserve XML field
//! roles, flattened child edges, and diff metadata until resolution.

use std::{collections::BTreeMap, fmt::Display};

use crate::{
    pom::{
        ContentNode, DiffSlot, DiffStrategy, MarkdownNode, MixedChildren, MixedContent,
        ParagraphNode, PomError, TextNode, XmlAttribute, XmlName, XmlNode,
    },
    StorageString,
};

/// Builds a complete Prompt Object Model root from a Rust value.
pub trait AgentView {
    type Root;

    fn build_root(&self) -> Result<Self::Root, PomError>;
}

/// Field-shaped output used by generated `AgentView` implementations.
///
/// This is a derive support API rather than a POM syntax node. It records how a
/// value participates in its parent until the parent applies the field.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewField {
    Empty,
    Attribute(XmlAttribute),
    Content(MixedContent),
    Children(MixedChildren),
}

/// Nested-value adapter used by generated `AgentView` implementations.
#[doc(hidden)]
pub trait AgentViewValue: AgentView {
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError>;

    fn build_children(&self) -> Result<MixedChildren, PomError>;
}

/// Converts a nestable derived root into POM content.
///
/// `Document` intentionally has no implementation because it cannot be nested.
/// Optional roots preserve absence instead of inventing a synthetic node.
#[doc(hidden)]
pub trait IntoViewContent {
    fn into_view_content(self) -> Option<ContentNode>;
}

impl IntoViewContent for ContentNode {
    fn into_view_content(self) -> Option<ContentNode> {
        Some(self)
    }
}

impl IntoViewContent for TextNode {
    fn into_view_content(self) -> Option<ContentNode> {
        Some(self.into())
    }
}

impl IntoViewContent for XmlNode {
    fn into_view_content(self) -> Option<ContentNode> {
        Some(self.into())
    }
}

impl IntoViewContent for MarkdownNode {
    fn into_view_content(self) -> Option<ContentNode> {
        Some(self.into())
    }
}

impl IntoViewContent for ParagraphNode {
    fn into_view_content(self) -> Option<ContentNode> {
        Some(MarkdownNode::Paragraph(self).into())
    }
}

impl<T> IntoViewContent for Option<T>
where
    T: IntoViewContent,
{
    fn into_view_content(self) -> Option<ContentNode> {
        self.and_then(IntoViewContent::into_view_content)
    }
}

/// Applies a generated field to its containing XML node.
#[doc(hidden)]
pub fn push_view_field(node: &mut XmlNode, field: ViewField) -> Result<(), PomError> {
    match field {
        ViewField::Empty => Ok(()),
        ViewField::Attribute(attribute) => {
            let (name, value) = attribute.into_parts();
            node.push_attribute(name, value)
        }
        ViewField::Content(content) => {
            node.push(content);
            Ok(())
        }
        ViewField::Children(children) => {
            node.extend_children(children);
            Ok(())
        }
    }
}

/// Applies a generated field as an explicitly addressable XML diff edge.
#[doc(hidden)]
pub fn push_diff_view_field(
    node: &mut XmlNode,
    role: XmlName,
    strategy: DiffStrategy,
    field: ViewField,
) -> Result<(), PomError> {
    let slot = match field_into_xml_node(role.clone(), field) {
        Some(value) => DiffSlot::present(strategy, value),
        None => DiffSlot::absent(role, strategy),
    };
    node.push(MixedContent::xml_slot(slot));
    Ok(())
}

/// Converts a display value without performing XML or Markdown escaping.
#[doc(hidden)]
pub fn view_value(value: &(impl Display + ?Sized)) -> StorageString {
    value.to_string().into()
}

/// Preserves the child edges produced by a flattened generated field.
#[doc(hidden)]
pub fn render_children_field(
    value: &(impl AgentViewValue + ?Sized),
) -> Result<ViewField, PomError> {
    value.build_children().map(ViewField::Children)
}

impl AgentView for String {
    type Root = TextNode;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        Ok(TextNode::new(self.as_str()))
    }
}

impl AgentViewValue for String {
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
        Ok(ViewField::Attribute(XmlAttribute::new(role, self.as_str())))
    }

    fn build_children(&self) -> Result<MixedChildren, PomError> {
        Ok(text_children(self.as_str()))
    }
}

impl AgentView for str {
    type Root = TextNode;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        Ok(TextNode::new(self))
    }
}

impl AgentViewValue for str {
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
        Ok(ViewField::Attribute(XmlAttribute::new(role, self)))
    }

    fn build_children(&self) -> Result<MixedChildren, PomError> {
        Ok(text_children(self))
    }
}

impl<T> AgentView for &T
where
    T: AgentView + ?Sized,
{
    type Root = T::Root;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        (*self).build_root()
    }
}

impl<T> AgentViewValue for &T
where
    T: AgentViewValue + ?Sized,
{
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
        (*self).build_field(role)
    }

    fn build_children(&self) -> Result<MixedChildren, PomError> {
        (*self).build_children()
    }
}

macro_rules! impl_scalar_agent_view {
    ($($ty:ty),* $(,)?) => {
        $(
            impl AgentView for $ty {
                type Root = TextNode;

                fn build_root(&self) -> Result<Self::Root, PomError> {
                    Ok(TextNode::new(view_value(self)))
                }
            }

            impl AgentViewValue for $ty {
                fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
                    Ok(ViewField::Attribute(XmlAttribute::new(role, view_value(self))))
                }

                fn build_children(&self) -> Result<MixedChildren, PomError> {
                    Ok(text_children(view_value(self)))
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
    type Root = Option<T::Root>;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        self.as_ref().map(AgentView::build_root).transpose()
    }
}

impl<T> AgentViewValue for Option<T>
where
    T: AgentViewValue,
{
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
        match self {
            Some(value) => value.build_field(role),
            None => Ok(ViewField::Empty),
        }
    }

    fn build_children(&self) -> Result<MixedChildren, PomError> {
        match self {
            Some(value) => value.build_children(),
            None => Ok(MixedChildren::new()),
        }
    }
}

impl<T> AgentView for Vec<T>
where
    T: AgentView,
    T::Root: IntoViewContent,
{
    type Root = XmlNode;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        build_list_node(XmlName::try_from("list")?, self)
    }
}

impl<T> AgentViewValue for Vec<T>
where
    T: AgentView,
    T::Root: IntoViewContent,
{
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
        Ok(ViewField::Content(MixedContent::xml(build_list_node(
            role, self,
        )?)))
    }

    fn build_children(&self) -> Result<MixedChildren, PomError> {
        build_collection_children(self)
    }
}

impl<K, V> AgentView for BTreeMap<K, V>
where
    K: AgentViewValue + Ord,
    V: AgentViewValue,
{
    type Root = XmlNode;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        build_map_node(XmlName::try_from("map")?, self)
    }
}

impl<K, V> AgentViewValue for BTreeMap<K, V>
where
    K: AgentViewValue + Ord,
    V: AgentViewValue,
{
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError> {
        Ok(ViewField::Content(MixedContent::xml(build_map_node(
            role, self,
        )?)))
    }

    fn build_children(&self) -> Result<MixedChildren, PomError> {
        let mut children = MixedChildren::new();
        children.push(MixedContent::xml(build_map_node(
            XmlName::try_from("map")?,
            self,
        )?));
        Ok(children)
    }
}

fn text_children(value: impl Into<StorageString>) -> MixedChildren {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new(value)));
    children
}

fn field_into_xml_node(role: XmlName, field: ViewField) -> Option<XmlNode> {
    match field {
        ViewField::Empty => None,
        ViewField::Attribute(attribute) => {
            let (_, value) = attribute.into_parts();
            let mut node = XmlNode::new(role);
            node.push(MixedContent::text(TextNode::new(value)));
            Some(node)
        }
        ViewField::Content(content) => match content.into_node_or_slot() {
            Ok(ContentNode::Xml(node)) if node.name() == &role => Some(node),
            Ok(content) => {
                let mut node = XmlNode::new(role);
                node.push(MixedContent::node(content));
                Some(node)
            }
            Err(slot) => {
                let mut node = XmlNode::new(role);
                node.push(MixedContent::xml_slot(slot));
                Some(node)
            }
        },
        ViewField::Children(children) => Some(XmlNode::new(role).with_children(children)),
    }
}

fn build_list_node<T>(name: XmlName, items: &[T]) -> Result<XmlNode, PomError>
where
    T: AgentView,
    T::Root: IntoViewContent,
{
    Ok(XmlNode::new(name).with_children(build_collection_children(items)?))
}

fn build_collection_children<T>(items: &[T]) -> Result<MixedChildren, PomError>
where
    T: AgentView,
    T::Root: IntoViewContent,
{
    let mut children = MixedChildren::new();
    for item in items {
        let Some(content) = item.build_root()?.into_view_content() else {
            continue;
        };
        match content {
            ContentNode::Xml(node) => children.push(MixedContent::xml(node)),
            ContentNode::Text(text) => {
                let mut item = XmlNode::new(XmlName::try_from("item")?);
                item.push(MixedContent::text(text));
                children.push(MixedContent::xml(item));
            }
            ContentNode::Markdown(markdown) => {
                let mut item = XmlNode::new(XmlName::try_from("item")?);
                item.push(MixedContent::markdown(markdown));
                children.push(MixedContent::xml(item));
            }
        }
    }
    Ok(children)
}

fn build_map_node<K, V>(name: XmlName, entries: &BTreeMap<K, V>) -> Result<XmlNode, PomError>
where
    K: AgentViewValue + Ord,
    V: AgentViewValue,
{
    let mut node = XmlNode::new(name);
    node.mark_as_map();
    node.extend_children(build_map_entries(entries)?);
    Ok(node)
}

fn build_map_entries<K, V>(entries: &BTreeMap<K, V>) -> Result<MixedChildren, PomError>
where
    K: AgentViewValue + Ord,
    V: AgentViewValue,
{
    let mut children = MixedChildren::new();
    for (key, value) in entries {
        let key_role = XmlName::try_from("key")?;
        let key_field = key.build_field(key_role.clone())?;
        let identity = field_into_xml_node(key_role, key_field.clone()).map(ContentNode::from);

        let mut entry = XmlNode::new(XmlName::try_from("entry")?);
        entry.set_identity(identity);
        push_view_field(&mut entry, key_field)?;
        push_view_field(&mut entry, value.build_field(XmlName::try_from("value")?)?)?;
        children.push(MixedContent::xml(entry));
    }
    Ok(children)
}
