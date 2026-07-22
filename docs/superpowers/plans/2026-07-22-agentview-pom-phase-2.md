# AgentView POM Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the first usable, additive Prompt Object Model AST without switching the existing AgentView renderer, differ, derive macro, or session runtime.

**Architecture:** A new public `agentview::pom` facade exposes validated Markdown/XML/Text values, context-typed child wrappers, canonical child sequences, explicit XML-only `DiffSlot` edges, and closure/compositional builders. The legacy `semantic_view` path remains untouched; Phase 2 only establishes a complete, serializable authoring tree and compile-time API boundaries for later producer/diff/resolver migration.

**Tech Stack:** Rust 2021, `ecow::EcoString` through `StorageString`, `serde::Serialize`, `thiserror`, Cargo integration tests, and trybuild compile-fail tests.

## Global Constraints

- Work additively under `src/pom`; do not modify `semantic_view`, `semantic_diff`, derive, renderer, agent, templates, or session behavior.
- POM is an authoring-first normalized AST, not a Markdown/XML parser and not a source-round-trip model.
- V1 Markdown is exactly `Heading`, `Paragraph`, ordered/unordered `List`, `CodeBlock`, `ThematicBreak`, `Strong`, and `CodeSpan` plus shared `TextNode` and XML islands.
- XML names use ASCII `[A-Za-z_][A-Za-z0-9_.-]*`; namespace colon, comments, DOCTYPE, processing instructions, CDATA, raw Markdown, and raw XML have no construction path.
- `TextNode` is context-neutral. CR/LF is rejected only when text enters `InlineChildren`; XML mixed content and code bodies preserve it.
- Empty sequence text is omitted and adjacent text is concatenated exactly. Never trim, merge across XML/Markdown/`DiffSlot`, or normalize atomic code bodies away.
- XML attribute equality is name/value based and order-independent; iteration and diagnostic serialization retain author insertion order.
- `DiffSlot::present(strategy, value: XmlNode)` derives `role` from `value.name`; `DiffSlot::absent(role, strategy)` retains an explicit absent edge. Markdown/Text cannot be direct slot values.
- All POM value types implement diagnostic `Serialize` only. Do not implement `Deserialize`; the wire shape is not a compatibility or persistence contract.
- Public fields and raw vectors remain private. Expose typed mutation and read-only traversal only.
- Every positive behavior follows observable RED -> minimal GREEN -> regression GREEN -> refactor. Do not accept unresolved-import trybuild snapshots.
- No new dependency or claimed MSRV is required; use Rust 2021 capabilities already available in the workspace.

## File Map

| File | Responsibility |
|---|---|
| `src/pom/mod.rs` | Private submodule declarations and stable public facade exports. |
| `src/pom/error.rs` | `PomError`, `ContentContext`, and `ContentKind` diagnostics. |
| `src/pom/text.rs` | Context-neutral `TextNode`. |
| `src/pom/xml.rs` | `XmlName`, attributes, `XmlNode`, and crate-private typed XML metadata. |
| `src/pom/markdown.rs` | V1 Markdown payload types and semantic classification. |
| `src/pom/content.rs` | `ContentNode`, private `ContentEdge`, typed content wrappers, and read-only `ContentRef`. |
| `src/pom/children.rs` | Block/inline/mixed sequences, normalization, and closure builders. |
| `src/pom/diff_slot.rs` | `DiffStrategy` and XML-only `DiffSlot`. |
| `src/pom/document.rs` | Non-nestable `Document` root and document builder entry points. |
| `src/lib.rs` | Add public `pom` module and selected prelude re-exports. |
| `tests/pom_ast.rs` | Runtime AST, validation, normalization, builder, traversal, and serialization behavior. |
| `tests/pom_compile_fail.rs` | Independent POM trybuild runner. |
| `tests/ui/pom/*.rs` | Invalid API call sites. |
| `tests/ui/pom/*.stderr` | Reviewed compiler diagnostics for invalid API call sites. |

---

### Task 1: Bootstrap Validated Primitive Values

**Files:**
- Create: `src/pom/mod.rs`
- Create: `src/pom/error.rs`
- Create: `src/pom/text.rs`
- Create: `src/pom/xml.rs`
- Create: `src/pom/markdown.rs`
- Modify: `src/lib.rs`
- Create: `tests/pom_ast.rs`

**Interfaces:**
- Produces: `PomError`, `TextNode::{new,value,is_empty}`, `XmlName::{new,as_str}`, `TryFrom<&str> for XmlName`, `HeadingLevel::{H1..H6,number}`, and `TryFrom<u8> for HeadingLevel`.
- Consumes: `crate::StorageString` and `thiserror::Error` already in the workspace.

- [x] **Step 1: Write the bootstrap XML-name test**

```rust
use agentview::pom::XmlName;

#[test]
fn xml_name_accepts_prompt_safe_ascii_subset() {
    for raw in ["agent_context", "actor-1", "a.b", "_private", "A9"] {
        let name = XmlName::try_from(raw).unwrap();
        assert_eq!(name.as_str(), raw);
    }
}
```

- [x] **Step 2: Run the exact test and observe RED**

Run: `cargo test --test pom_ast xml_name_accepts_prompt_safe_ascii_subset -- --exact`

Expected: compile failure `E0432` because `agentview::pom` does not exist. This unresolved import is accepted only for the bootstrap test and must not become a trybuild snapshot.

- [x] **Step 3: Add the minimal facade, error, and `XmlName` implementation**

Add `pub mod pom;` to `src/lib.rs`. In `src/pom/mod.rs` declare private modules and re-export only types that exist in this task:

```rust
mod error;
mod markdown;
mod text;
mod xml;

pub use error::PomError;
pub use markdown::HeadingLevel;
pub use text::TextNode;
pub use xml::XmlName;
```

Implement the first error variants in `error.rs`:

```rust
use crate::StorageString;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PomError {
    #[error("invalid XML name: {value}")]
    InvalidXmlName { value: StorageString },
    #[error("invalid Markdown heading level: {value}")]
    InvalidHeadingLevel { value: u8 },
}
```

Implement `XmlName` in `xml.rs` with `Debug`, `Clone`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, and `Hash`. For this first GREEN, `XmlName::new` stores `impl Into<StorageString>` and returns `Ok`; `TryFrom<&str>` delegates to `new`, and `as_str` returns `&str`. This is the minimum implementation proven by the valid-name test. Exact rejection is added only after the next RED. Diagnostic serialization is deliberately deferred to Task 7 so it gets an observable RED.

- [x] **Step 4: Verify XML-name GREEN**

Run: `cargo test --test pom_ast xml_name_accepts_prompt_safe_ascii_subset -- --exact`

Expected: PASS.

- [x] **Step 5: Run three independent primitive RED/GREEN cycles**

Append these tests separately, running the exact command after each addition before implementing it:

```rust
use agentview::pom::{HeadingLevel, PomError, TextNode};

#[test]
fn xml_name_rejects_invalid_and_namespace_names() {
    for raw in ["", "9actor", "actor context", "actor:context", "<actor>", "é"] {
        assert_eq!(
            XmlName::try_from(raw),
            Err(PomError::InvalidXmlName { value: raw.into() })
        );
    }
}

#[test]
fn heading_level_accepts_only_one_through_six() {
    for (raw, expected) in [
        (1, HeadingLevel::H1),
        (2, HeadingLevel::H2),
        (3, HeadingLevel::H3),
        (4, HeadingLevel::H4),
        (5, HeadingLevel::H5),
        (6, HeadingLevel::H6),
    ] {
        assert_eq!(HeadingLevel::try_from(raw).unwrap(), expected);
        assert_eq!(expected.number(), raw);
    }
    assert_eq!(
        HeadingLevel::try_from(0),
        Err(PomError::InvalidHeadingLevel { value: 0 })
    );
    assert_eq!(
        HeadingLevel::try_from(7),
        Err(PomError::InvalidHeadingLevel { value: 7 })
    );
}

#[test]
fn text_node_preserves_author_text() {
    let text = TextNode::new("  first\nsecond  ");
    assert_eq!(text.value(), "  first\nsecond  ");
    assert!(!text.is_empty());
    assert!(TextNode::new("").is_empty());
}
```

Add and run each test separately, then make only its minimum implementation before adding the next test:

- invalid-name RED: an invalid input incorrectly returns `Ok`; GREEN adds the exact ASCII grammar and preserves the original value in `InvalidXmlName`;
- heading RED: `HeadingLevel`/conversion is missing; GREEN adds `H1..H6`, `number`, and `TryFrom<u8>`;
- text RED: `TextNode`/accessors are missing; GREEN adds context-neutral storage without trimming or newline rejection.

- [x] **Step 6: Keep the final primitive signatures exact**

In `markdown.rs`, define `HeadingLevel::{H1..H6}`, derive `Debug, Clone, Copy, PartialEq, Eq`, implement `number`, and implement `TryFrom<u8>` returning `InvalidHeadingLevel` outside `1..=6`.

In `text.rs`, define:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextNode {
    value: StorageString,
}

impl TextNode {
    pub fn new(value: impl Into<StorageString>) -> Self;
    pub fn value(&self) -> &str;
    pub fn is_empty(&self) -> bool;
    pub(crate) fn append(&mut self, value: &str);
}
```

Do not trim or reject newlines here.

- [x] **Step 7: Run task tests and format check**

Run:

```bash
cargo test --test pom_ast
cargo fmt --all --check
```

Expected: all four POM tests PASS and formatting is clean.

- [x] **Step 8: Commit the primitive slice**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs
git commit -m "feat(pom): add validated primitive values"
```

---
### Task 2: Preserve XML Attribute Order Without Giving It Semantic Meaning

**Files:**
- Modify: `src/pom/error.rs`
- Modify: `src/pom/xml.rs`
- Modify: `src/pom/mod.rs`
- Modify: `src/lib.rs`
- Modify: `tests/pom_ast.rs`

**Interfaces:**
- Consumes: `XmlName` from Task 1.
- Produces: `XmlAttribute::{new,name,value}`, `XmlAttributes::{new,insert,try_insert,get,iter,len,is_empty}` and order-independent `PartialEq/Eq`.

- [x] **Step 1: Write and run the duplicate-attribute RED test**

```rust
use agentview::pom::{PomError, XmlAttributes, XmlName};

#[test]
fn xml_attributes_reject_duplicate_names() {
    let id = XmlName::try_from("id").unwrap();
    let mut attributes = XmlAttributes::new();
    attributes.insert(id.clone(), "actor.1").unwrap();
    assert_eq!(
        attributes.insert(id.clone(), "actor.2"),
        Err(PomError::DuplicateXmlAttribute { name: id })
    );
}
```

Run: `cargo test --test pom_ast xml_attributes_reject_duplicate_names -- --exact`

Expected: compile failure because `XmlAttributes` and `DuplicateXmlAttribute` do not exist.

- [x] **Step 2: Implement duplicate-safe insertion**

Add to `PomError`:

```rust
#[error("duplicate XML attribute: {name}")]
DuplicateXmlAttribute { name: XmlName },
```

Implement `Display for XmlName`, then add in `xml.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlAttribute {
    name: XmlName,
    value: StorageString,
}

#[derive(Debug, Clone, Default)]
pub struct XmlAttributes(Vec<XmlAttribute>);
```

`insert(XmlName, impl Into<StorageString>)` rejects an existing name before pushing. `try_insert(&str, value)` validates the raw name through `XmlName::try_from`. All getters expose borrowed data; no mutable vector accessor is added.

- [x] **Step 3: Verify duplicate GREEN**

Run: `cargo test --test pom_ast xml_attributes_reject_duplicate_names -- --exact`

Expected: PASS.

- [x] **Step 4: Write insertion-order and semantic-equality RED tests**

```rust
#[test]
fn xml_attribute_reordering_is_semantically_equal() {
    let mut left = XmlAttributes::new();
    left.try_insert("id", "actor.1").unwrap();
    left.try_insert("name", "Rachel").unwrap();

    let mut right = XmlAttributes::new();
    right.try_insert("name", "Rachel").unwrap();
    right.try_insert("id", "actor.1").unwrap();

    assert_eq!(left, right);
}

#[test]
fn attribute_iteration_preserves_insertion_order() {
    let mut attributes = XmlAttributes::new();
    attributes.try_insert("id", "actor.1").unwrap();
    attributes.try_insert("name", "Rachel").unwrap();

    let observed = attributes
        .iter()
        .map(|attribute| (attribute.name().as_str(), attribute.value()))
        .collect::<Vec<_>>();
    assert_eq!(observed, vec![("id", "actor.1"), ("name", "Rachel")]);
}
```

Run each exact test before implementation. Expected RED: derived/vector equality treats reorder as unequal, then missing or incorrect iteration order.

- [x] **Step 5: Implement custom equality and read-only traversal**

Implement `PartialEq` for `XmlAttributes` by checking equal length and matching each left attribute by name/value through `get`; implement `Eq`. Keep the backing `Vec` and its insertion order unchanged. Re-export `XmlAttribute` and `XmlAttributes` from `pom` and the crate prelude.

- [x] **Step 6: Run focused and broader GREEN checks**

```bash
cargo test --test pom_ast
cargo test --workspace --tests
cargo fmt --all --check
```

Expected: PASS.

- [x] **Step 7: Commit XML attributes**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs
git commit -m "feat(pom): add semantic XML attributes"
```

---

### Task 3: Establish the Recursive POM Type Graph and Markdown Payloads

**Files:**
- Create: `src/pom/content.rs`
- Create: `src/pom/children.rs`
- Create: `src/pom/document.rs`
- Modify: `src/pom/markdown.rs`
- Modify: `src/pom/xml.rs`
- Modify: `src/pom/mod.rs`
- Modify: `src/lib.rs`
- Modify: `tests/pom_ast.rs`

**Interfaces:**
- Consumes: validated values and attributes from Tasks 1-2.
- Produces: the complete non-diff recursive type graph: `Document`, `ContentNode`, `ContentRef`, block/inline/mixed wrappers and child sequences, all V1 Markdown payload types, and `XmlNode`.
- Leaves for later tasks: dynamic context errors, canonical sequence normalization, diff edges, and convenience closure builders.

- [x] **Step 1: Write the Markdown-payload RED test**

```rust
use agentview::pom::{
    BlockChildren, CodeBlockNode, CodeSpanNode, ListItem, ListKind, ListNode,
    TextNode,
};

#[test]
fn ordered_list_and_code_nodes_preserve_semantic_payload() {
    let item = ListItem::new(BlockChildren::new());
    let list = ListNode::new(ListKind::Ordered { start: 3 }, vec![item]);
    assert_eq!(list.kind(), &ListKind::Ordered { start: 3 });
    assert_eq!(list.items().len(), 1);

    let block = CodeBlockNode::new(Some("rust".into()), TextNode::new("<tag>\n"));
    assert_eq!(block.language(), Some("rust"));
    assert_eq!(block.body().value(), "<tag>\n");

    let span = CodeSpanNode::new(TextNode::new("<tag>"));
    assert_eq!(span.body().value(), "<tag>");
}
```

Run: `cargo test --test pom_ast ordered_list_and_code_nodes_preserve_semantic_payload -- --exact`

Expected: compile failure because the Markdown payload types and child containers do not exist.

- [x] **Step 2: Add the V1 Markdown payload structs**

Define and derive `Debug, Clone, PartialEq, Eq` for:

```rust
pub enum MarkdownNode {
    Heading(HeadingNode),
    Paragraph(ParagraphNode),
    List(ListNode),
    CodeBlock(CodeBlockNode),
    ThematicBreak,
    Strong(StrongNode),
    CodeSpan(CodeSpanNode),
}

pub struct HeadingNode { level: HeadingLevel, children: InlineChildren }
pub struct ParagraphNode { children: InlineChildren }
pub struct StrongNode { children: InlineChildren }
pub struct ListNode { kind: ListKind, items: Vec<ListItem> }
pub enum ListKind { Unordered, Ordered { start: u64 } }
pub struct ListItem { children: BlockChildren }
pub struct CodeBlockNode { language: Option<StorageString>, body: TextNode }
pub struct CodeSpanNode { body: TextNode }
```

Every struct gets an infallible constructor over already typed values and borrowed getters. `MarkdownNode` gets crate-visible exhaustive `is_block`/`is_inline` classification with no wildcard match.

- [x] **Step 3: Add the minimal recursive containers required to compile and verify GREEN**

Use this ownership graph across `content.rs`, `children.rs`, `xml.rs`, and `document.rs`:

```rust
pub enum ContentNode {
    Markdown(MarkdownNode),
    Xml(XmlNode),
    Text(TextNode),
}

enum ContentEdge {
    Node(ContentNode),
}

pub struct BlockContent(ContentEdge);
pub struct InlineContent(ContentEdge);
pub struct MixedContent(ContentEdge);

pub struct BlockChildren(Vec<BlockContent>);
pub struct InlineChildren(Vec<InlineContent>);
pub struct MixedChildren(Vec<MixedContent>);

pub struct XmlNode {
    name: XmlName,
    attributes: XmlAttributes,
    children: MixedChildren,
    metadata: XmlMetadata,
}

pub struct Document {
    children: BlockChildren,
}
```

Recursion is through the child `Vec`; do not add per-child `Box`. Add `new`, `push(typed_content)`, `len`, `is_empty`, and read-only `iter` methods to child sequences. `ContentRef<'a>` initially exposes `Node(&'a ContentNode)`; Task 5 adds the diff variant. Add `From<MarkdownNode/XmlNode/TextNode> for ContentNode`, but do not add unconditional `From<ContentNode>` for block/inline wrappers.

`XmlMetadata` is crate-private typed/default state:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct XmlMetadata {
    collection_kind: Option<IntrinsicCollectionKind>,
    identity: Option<Box<ContentNode>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntrinsicCollectionKind { Map }
```

Do not expose a generic metadata map or public setters in Phase 2.

Run: `cargo test --test pom_ast ordered_list_and_code_nodes_preserve_semantic_payload -- --exact`

Expected: PASS.

- [x] **Step 4: Write the mixed-order RED test**

```rust
use agentview::pom::{
    ContentNode, ContentRef, HeadingLevel, HeadingNode, InlineChildren,
    MarkdownNode, MixedChildren, MixedContent, TextNode, XmlName, XmlNode,
};

#[test]
fn mixed_children_preserve_text_markdown_xml_text_order() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("before")));
    children.push(MixedContent::markdown(MarkdownNode::Heading(
        HeadingNode::new(HeadingLevel::H2, InlineChildren::new()),
    )));
    children.push(MixedContent::xml(XmlNode::new(
        XmlName::try_from("actor").unwrap(),
    )));
    children.push(MixedContent::text(TextNode::new("after")));

    let kinds = children
        .iter()
        .map(|edge| match edge {
            ContentRef::Node(ContentNode::Text(_)) => "text",
            ContentRef::Node(ContentNode::Markdown(_)) => "markdown",
            ContentRef::Node(ContentNode::Xml(_)) => "xml",
        })
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["text", "markdown", "xml", "text"]);
}
```

Run the exact test. Expected RED: missing typed constructors/read-only iteration or wrong order.

- [x] **Step 5: Complete typed constructors and getters**

Add:

- `BlockContent::{heading,paragraph,list,code_block,thematic_break,xml}`;
- `InlineContent::{try_text,strong,code_span,xml}`;
- `MixedContent::{node,text,markdown,xml}`;
- `XmlNode::{new,name,attributes,children,push_attribute,push}`;
- `Document::{new,children}`.

At this point `InlineContent::try_text` only establishes the fallible signature; Task 4 adds newline validation. Re-export every public value type from `pom/mod.rs`. Add an explicit prelude re-export list for `BlockBuilder`, `BlockChildren`, `BlockContent`, `CodeBlockNode`, `CodeSpanNode`, `ContentContext`, `ContentKind`, `ContentNode`, `ContentRef`, `DiffSlot`, `DiffStrategy`, `Document`, `HeadingLevel`, `HeadingNode`, `InlineBuilder`, `InlineChildren`, `InlineContent`, `ListBuilder`, `ListItem`, `ListKind`, `ListNode`, `MarkdownKind`, `MarkdownNode`, `MixedBuilder`, `MixedChildren`, `MixedContent`, `ParagraphNode`, `PomError`, `StrongNode`, `TextNode`, `XmlAttribute`, `XmlAttributes`, `XmlName`, and `XmlNode`; names introduced by later tasks are added when those tasks land.

- [x] **Step 6: Run focused and workspace tests**

```bash
cargo test --test pom_ast
cargo test --workspace --tests
cargo fmt --all --check
```

Expected: PASS and legacy semantic tests unchanged.

- [x] **Step 7: Commit the recursive AST slice**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs
git commit -m "feat(pom): add mixed document AST"
```

---

### Task 4: Enforce Child Contexts and Canonical Text Sequences

**Files:**
- Modify: `src/pom/error.rs`
- Modify: `src/pom/content.rs`
- Modify: `src/pom/children.rs`
- Modify: `src/pom/mod.rs`
- Modify: `src/lib.rs`
- Modify: `tests/pom_ast.rs`

**Interfaces:**
- Consumes: complete non-diff type graph from Task 3.
- Produces: `ContentContext`, `ContentKind`, `MarkdownKind`, fallible dynamic conversion into block/inline wrappers, inline newline validation, and mandatory sequence normalization.

- [x] **Step 1: Write the direct-child validation RED test**

```rust
use agentview::pom::{
    BlockContent, ContentContext, ContentKind, ContentNode, HeadingLevel,
    HeadingNode, InlineChildren, InlineContent, MarkdownKind, MarkdownNode,
    PomError, TextNode,
};

#[test]
fn block_and_inline_contexts_reject_invalid_direct_children() {
    let text = ContentNode::Text(TextNode::new("orphan"));
    assert_eq!(
        BlockContent::try_from_node(text),
        Err(PomError::WrongContentContext {
            expected: ContentContext::Block,
            actual: ContentKind::Text,
        })
    );

    let heading = ContentNode::Markdown(MarkdownNode::Heading(HeadingNode::new(
        HeadingLevel::H2,
        InlineChildren::new(),
    )));
    assert_eq!(
        InlineContent::try_from_node(heading),
        Err(PomError::WrongContentContext {
            expected: ContentContext::Inline,
            actual: ContentKind::Markdown(MarkdownKind::Heading),
        })
    );
}
```

Run: `cargo test --test pom_ast block_and_inline_contexts_reject_invalid_direct_children -- --exact`

Expected: compile failure because context/kind diagnostics and dynamic conversion do not exist.

- [x] **Step 2: Implement exhaustive context classification and conversion**

Add copyable public enums covering every current syntax variant:

```rust
pub enum ContentContext { Block, Inline, Mixed }
pub enum ContentKind { Markdown(MarkdownKind), Xml, Text }
pub enum MarkdownKind {
    Heading, Paragraph, List, CodeBlock, ThematicBreak, Strong, CodeSpan,
}
```

Extend `PomError`:

```rust
InvalidInlineNewline { value: StorageString },
WrongContentContext { expected: ContentContext, actual: ContentKind },
```

Implement `ContentNode::kind` and exhaustive `MarkdownNode::kind`. `BlockContent::try_from_node` accepts only block Markdown and XML. For this GREEN, `InlineContent::try_from_node` accepts Text, inline Markdown, and XML without inspecting Text contents; the following RED adds CR/LF rejection through one shared private helper. `MixedContent::node` accepts any `ContentNode`.

- [x] **Step 3: Verify direct-child GREEN**

Run: `cargo test --test pom_ast block_and_inline_contexts_reject_invalid_direct_children -- --exact`

Expected: PASS.

- [x] **Step 4: Write and run inline-newline RED**

```rust
#[test]
fn markdown_inline_children_reject_newlines() {
    assert_eq!(
        InlineContent::try_text("first\nsecond"),
        Err(PomError::InvalidInlineNewline {
            value: "first\nsecond".into(),
        })
    );
    assert_eq!(
        InlineContent::try_text("first\rsecond"),
        Err(PomError::InvalidInlineNewline {
            value: "first\rsecond".into(),
        })
    );
}
```

Run the exact test. Expected RED: newline text incorrectly returns `Ok`.

Implement the check in the single private conversion used by both `try_text` and `try_from_node`, then re-run the exact test and the whole `pom_ast` suite.

- [x] **Step 5: Write canonical-normalization RED tests**

```rust
#[test]
fn text_sequences_drop_empty_and_merge_adjacent_text() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("")));
    children.push(MixedContent::text(TextNode::new("  first")));
    children.push(MixedContent::text(TextNode::new(" second  ")));

    assert_eq!(children.len(), 1);
    match children.iter().next().unwrap() {
        ContentRef::Node(ContentNode::Text(text)) => {
            assert_eq!(text.value(), "  first second  ");
        }
        other => panic!("expected normalized text, got {other:?}"),
    }
}

#[test]
fn text_normalization_does_not_trim_or_cross_xml() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new(" left ")));
    children.push(MixedContent::xml(XmlNode::new(
        XmlName::try_from("break").unwrap(),
    )));
    children.push(MixedContent::text(TextNode::new(" right ")));

    assert_eq!(children.len(), 3);
    let texts = children
        .iter()
        .filter_map(|edge| match edge {
            ContentRef::Node(ContentNode::Text(text)) => Some(text.value()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec![" left ", " right "]);
}
```

Run each exact test before changing production code. Expected RED: empty/adjacent text remains as multiple edges.

- [x] **Step 6: Centralize sequence normalization**

In `children.rs`, add one private helper over `ContentEdge` that:

1. drops an incoming `Node(ContentNode::Text(text))` when `text.is_empty()`;
2. appends its exact value to the immediately preceding text node;
3. pushes every other edge unchanged.

Use it from `InlineChildren::push` and `MixedChildren::push`. `BlockChildren` cannot contain direct text and uses normal push. Do not normalize inside `CodeBlockNode` or `CodeSpanNode`.

- [x] **Step 7: Run regression GREEN**

```bash
cargo test --test pom_ast
cargo test --workspace --tests
cargo fmt --all --check
```

Expected: PASS.

- [x] **Step 8: Commit validation and normalization**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs
git commit -m "feat(pom): enforce child contexts"
```

---

### Task 5: Add Explicit XML-Only Diff Edges

**Files:**
- Create: `src/pom/diff_slot.rs`
- Modify: `src/pom/content.rs`
- Modify: `src/pom/children.rs`
- Modify: `src/pom/mod.rs`
- Modify: `src/lib.rs`
- Modify: `tests/pom_ast.rs`

**Interfaces:**
- Consumes: `XmlName`, `XmlNode`, and typed child wrappers.
- Produces: `DiffStrategy::{Recursive,Replace,Append,Sequence,Set,Keyed}`, `DiffSlot::{present,absent,role,strategy,value,is_present}`, `ContentRef::DiffSlot`, and `xml_slot` construction in every context where XML is legal.

- [x] **Step 1: Write and run present-slot RED tests**

```rust
use agentview::pom::{DiffSlot, DiffStrategy, XmlName, XmlNode};

#[test]
fn present_diff_slot_derives_role_from_value_name() {
    let value = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    let slot = DiffSlot::present(DiffStrategy::Recursive, value.clone());

    assert_eq!(slot.role().as_str(), "agent_context");
    assert_eq!(slot.strategy(), &DiffStrategy::Recursive);
    assert_eq!(slot.value(), Some(&value));
    assert!(slot.is_present());
}

#[test]
fn xml_wrapped_markdown_is_a_valid_slot_value() {
    let mut inline = InlineChildren::new();
    inline.push(InlineContent::try_text("explanation").unwrap());

    let mut xml = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    xml.push(MixedContent::markdown(MarkdownNode::Paragraph(
        ParagraphNode::new(inline),
    )));

    let slot = DiffSlot::present(DiffStrategy::Recursive, xml);
    assert!(matches!(
        slot.value().unwrap().children().iter().next(),
        Some(ContentRef::Node(ContentNode::Markdown(
            MarkdownNode::Paragraph(_)
        )))
    ));
}

#[test]
fn all_diff_strategies_are_structurally_observable() {
    let key = XmlName::try_from("id").unwrap();
    let strategies = vec![
        DiffStrategy::Recursive,
        DiffStrategy::Replace,
        DiffStrategy::Append,
        DiffStrategy::Sequence,
        DiffStrategy::Set,
        DiffStrategy::Keyed(key),
    ];
    for strategy in strategies {
        let slot = DiffSlot::present(
            strategy.clone(),
            XmlNode::new(XmlName::try_from("items").unwrap()),
        );
        assert_eq!(slot.strategy(), &strategy);
    }
}
```

Run both exact tests before implementation:

```bash
cargo test --test pom_ast present_diff_slot_derives_role_from_value_name -- --exact
cargo test --test pom_ast xml_wrapped_markdown_is_a_valid_slot_value -- --exact
cargo test --test pom_ast all_diff_strategies_are_structurally_observable -- --exact
```

Expected: compile failure because `DiffSlot` and `DiffStrategy` do not exist.

- [x] **Step 2: Implement the XML-only slot type**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffStrategy {
    Recursive,
    Replace,
    Append,
    Sequence,
    Set,
    Keyed(XmlName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSlot {
    role: XmlName,
    strategy: DiffStrategy,
    value: Option<XmlNode>,
}
```

`present` takes exactly `(DiffStrategy, XmlNode)` and clones the role from `value.name()`. `absent` takes `(XmlName, DiffStrategy)`. There is no generic `ContentNode` overload and no role argument on `present`.

- [x] **Step 3: Verify present-slot GREEN**

Run all three exact commands from Step 1. Expected: PASS.

- [x] **Step 4: Write absent-slot, strategy, and ordered-edge RED tests**

```rust
#[test]
fn absent_diff_slot_retains_role_strategy_and_position() {
    let role = XmlName::try_from("agent_context").unwrap();
    let slot = DiffSlot::absent(role.clone(), DiffStrategy::Replace);
    assert_eq!(slot.role(), &role);
    assert_eq!(slot.strategy(), &DiffStrategy::Replace);
    assert_eq!(slot.value(), None);

    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("before")));
    children.push(MixedContent::xml_slot(slot));
    children.push(MixedContent::text(TextNode::new("after")));
    assert!(matches!(
        children.iter().nth(1),
        Some(ContentRef::DiffSlot(slot)) if slot.role() == &role
    ));
}

#[test]
fn text_normalization_does_not_cross_diff_slot() {
    let mut children = MixedChildren::new();
    children.push(MixedContent::text(TextNode::new("left")));
    children.push(MixedContent::xml_slot(DiffSlot::absent(
        XmlName::try_from("agent_context").unwrap(),
        DiffStrategy::Recursive,
    )));
    children.push(MixedContent::text(TextNode::new("right")));
    assert_eq!(children.len(), 3);
}
```

Run each exact test before implementation. Expected RED: diff edge constructors/variant are absent or normalization incorrectly crosses the edge.

- [x] **Step 5: Integrate diff edges without making them syntax nodes**

Extend private `ContentEdge` with `Diff(DiffSlot)` and public `ContentRef<'a>` with `DiffSlot(&'a DiffSlot)`. Add:

- `BlockContent::xml_slot(DiffSlot)`;
- `InlineContent::xml_slot(DiffSlot)`;
- `MixedContent::xml_slot(DiffSlot)`.

Keep `DiffSlot` out of `ContentNode`. Update read-only iteration and normalization exhaustively; a diff edge always blocks text merging, including an absent slot.

- [x] **Step 6: Keep earlier exhaustive traversal tests compiling**

When adding `ContentRef::DiffSlot`, update the Task 3 mixed-order test's exhaustive match with `ContentRef::DiffSlot(_) => "diff"`. Re-run all three Step 1 exact tests to prove the complete nested XML value and strategy set remain intact after edge integration.

- [x] **Step 7: Run regression GREEN**

```bash
cargo test --test pom_ast
cargo test --workspace --tests
cargo fmt --all --check
```

Expected: PASS.

- [x] **Step 8: Commit explicit diff edges**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs
git commit -m "feat(pom): add XML diff slots"
```

---

### Task 6: Add Compositional and Closure Authoring APIs

**Files:**
- Modify: `src/pom/document.rs`
- Modify: `src/pom/children.rs`
- Modify: `src/pom/markdown.rs`
- Modify: `src/pom/xml.rs`
- Modify: `src/pom/mod.rs`
- Modify: `src/lib.rs`
- Modify: `tests/pom_ast.rs`

**Interfaces:**
- Consumes: typed content wrappers and validation from Tasks 3-5.
- Produces: `BlockBuilder`, `InlineBuilder`, `MixedBuilder`, `ListBuilder`, `Document::{build,try_build}`, and `XmlNode::{build,try_build}`. Closure builders reuse the same typed constructors and `push` normalization as compositional construction.

- [x] **Step 1: Write closure/compositional parity RED**

```rust
use agentview::pom::{
    BlockChildren, BlockContent, DiffSlot, DiffStrategy, Document, HeadingLevel,
    HeadingNode, InlineChildren, InlineContent, XmlName, XmlNode,
};

#[test]
fn closure_and_compositional_builders_are_equivalent() {
    let context = XmlNode::new(XmlName::try_from("agent_context").unwrap());
    let closure = Document::try_build(|blocks| {
        blocks.try_heading(2, |inline| {
            inline.try_text("Known relationships")?;
            Ok(())
        })?;
        blocks.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            context.clone(),
        ));
        Ok(())
    })
    .unwrap();

    let mut heading_children = InlineChildren::new();
    heading_children.push(InlineContent::try_text("Known relationships").unwrap());
    let mut blocks = BlockChildren::new();
    blocks.push(BlockContent::heading(HeadingNode::new(
        HeadingLevel::H2,
        heading_children,
    )));
    blocks.push(BlockContent::xml_slot(DiffSlot::present(
        DiffStrategy::Recursive,
        context,
    )));
    let compositional = Document::new(blocks);

    assert_eq!(closure, compositional);
}
```

Run: `cargo test --test pom_ast closure_and_compositional_builders_are_equivalent -- --exact`

Expected: compile failure because document/child closure builders do not exist.

- [x] **Step 2: Implement builders as thin typed facades**

Use these entry signatures:

```rust
impl Document {
    pub fn build(build: impl FnOnce(&mut BlockBuilder<'_>)) -> Self;
    pub fn try_build(
        build: impl FnOnce(&mut BlockBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<Self, PomError>;
}

impl XmlNode {
    pub fn build(name: XmlName, build: impl FnOnce(&mut MixedBuilder<'_>)) -> Self;
    pub fn try_build(
        raw_name: &str,
        build: impl FnOnce(&mut MixedBuilder<'_>) -> Result<(), PomError>,
    ) -> Result<Self, PomError>;
}
```

Builder methods must delegate to typed content constructors and sequence `push`:

```rust
pub struct BlockBuilder<'a> { children: &'a mut BlockChildren }
pub struct InlineBuilder<'a> { children: &'a mut InlineChildren }
pub struct MixedBuilder<'a> { children: &'a mut MixedChildren }
pub struct ListBuilder { items: Vec<ListItem> }
```

Final required surface:

- `BlockBuilder`: `push`, `heading`, `try_heading`, `paragraph`, `try_paragraph`, `list`, `try_list`, `code_block`, `thematic_break`, `xml`, `xml_slot`;
- `InlineBuilder`: `push`, `try_text`, `strong`, `try_strong`, `code_span`, `xml`, `xml_slot`;
- `MixedBuilder`: `push`, `text`, `markdown`, `xml`, `xml_slot`;
- `ListBuilder`: `item`, `try_item`, and private `finish`.

For the parity GREEN in this step, implement only `Document::try_build`, `BlockBuilder::{try_heading,xml_slot}`, and `InlineBuilder::try_text`. The builder-coverage RED below drives the remaining methods. The typed methods take validated values and infallible closures. The `try_*` methods take raw/fallible inputs and closures returning `Result<(), PomError>`. Do not store errors inside builders, panic on invalid input, infer a paragraph from raw block text, or duplicate validation logic.

- [x] **Step 3: Verify parity GREEN**

Run the exact test. Expected: PASS.

- [x] **Step 4: Write a builder-coverage RED test**

```rust
#[test]
fn closure_builders_cover_the_v1_authoring_surface() {
    let document = Document::try_build(|blocks| {
        blocks.try_heading(2, |inline| {
            inline.try_text("Title")?;
            Ok(())
        })?;
        blocks.try_paragraph(|inline| {
            inline.try_text("plain ")?;
            inline.try_strong(|strong| {
                strong.try_text("strong")?;
                Ok(())
            })?;
            inline.code_span(TextNode::new("code"));
            inline.xml(XmlNode::new(XmlName::try_from("ref")?));
            Ok(())
        })?;
        blocks.try_list(ListKind::Ordered { start: 3 }, |list| {
            list.try_item(|item| {
                item.try_paragraph(|inline| {
                    inline.try_text("first item")?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;
        blocks.code_block(Some("rust".into()), TextNode::new("<tag>\n"));
        blocks.thematic_break();
        blocks.xml(XmlNode::build(
            XmlName::try_from("mixed").unwrap(),
            |mixed| {
                mixed.text(TextNode::new("before"));
                mixed.markdown(MarkdownNode::Paragraph(ParagraphNode::new(
                    InlineChildren::new(),
                )));
                mixed.xml(XmlNode::new(XmlName::try_from("inner").unwrap()));
            },
        ));
        blocks.xml_slot(DiffSlot::present(
            DiffStrategy::Recursive,
            XmlNode::new(XmlName::try_from("agent_context")?),
        ));
        Ok(())
    })
    .unwrap();

    assert_eq!(document.children().len(), 7);
    assert!(matches!(
        document.children().iter().next(),
        Some(ContentRef::Node(ContentNode::Markdown(
            MarkdownNode::Heading(_)
        )))
    ));
    assert!(matches!(
        document.children().iter().nth(6),
        Some(ContentRef::DiffSlot(slot)) if slot.role().as_str() == "agent_context"
    ));
}
```

Run: `cargo test --test pom_ast closure_builders_cover_the_v1_authoring_surface -- --exact`

Expected RED: the first missing convenience method or wrong structural variant.

- [x] **Step 5: Complete only the missing builder methods and re-run GREEN**

Implement the minimum methods required by the coverage test. Reuse `BlockChildren::push`, `InlineChildren::push`, and `MixedChildren::push` so closure and compositional paths cannot diverge on normalization.

- [x] **Step 6: Run regression GREEN**

```bash
cargo test --test pom_ast
cargo test --workspace --tests
cargo fmt --all --check
```

Expected: PASS.

- [x] **Step 7: Commit authoring builders**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs
git commit -m "feat(pom): add typed document builders"
```

---

### Task 7: Add Diagnostic Serialization and Compile-Time API Guards

**Files:**
- Modify: all POM value-type files under `src/pom/`
- Modify: `tests/pom_ast.rs`
- Create: `tests/pom_compile_fail.rs`
- Create: `tests/ui/pom/diff_slot_rejects_text_node.rs`
- Create: `tests/ui/pom/diff_slot_present_rejects_explicit_role.rs`
- Create: `tests/ui/pom/markdown_payload_fields_are_private.rs`
- Create: `tests/ui/pom/inline_children_reject_heading.rs`
- Create: `tests/ui/pom/document_rejects_text.rs`
- Create: `tests/ui/pom/children_have_no_mutable_vector_access.rs`
- Create: `tests/ui/pom/pom_types_do_not_deserialize.rs`
- Create: matching `tests/ui/pom/*.stderr`
- Modify: `docs/superpowers/specs/2026-07-22-agentview-prompt-object-model-design.md`

**Interfaces:**
- Consumes: the complete Phase 2 AST.
- Produces: diagnostic-only `Serialize` coverage and compiler-checked negative API boundaries. It does not add `Deserialize`, renderer, parser, differ, resolver, cursor, or runtime integration.

- [x] **Step 1: Write diagnostic-serialization RED**

```rust
#[test]
fn diagnostic_serialization_preserves_order_and_diff_metadata() {
    let document = Document::try_build(|blocks| {
        blocks.try_paragraph(|inline| {
            inline.try_text("before")?;
            inline.xml_slot(DiffSlot::present(
                DiffStrategy::Keyed(XmlName::try_from("id")?),
                XmlNode::new(XmlName::try_from("agent_context")?),
            ));
            inline.try_text("after")?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();

    let json = serde_json::to_string(&document).unwrap();
    let before = json.find("before").unwrap();
    let role = json.find("agent_context").unwrap();
    let after = json.find("after").unwrap();
    assert!(before < role && role < after);
    assert!(json.contains("Keyed"));
    assert!(json.contains("id"));
}
```

Run: `cargo test --test pom_ast diagnostic_serialization_preserves_order_and_diff_metadata -- --exact`

Expected: compile failure `E0277` because `Document` does not yet implement `Serialize`.

- [x] **Step 2: Add `Serialize` transitively to the AST**

Derive `serde::Serialize` on every public POM value and the private types reachable from them: document, syntax nodes, payload structs/enums, wrappers, child sequences, `ContentEdge`, XML attributes/metadata, `DiffSlot`, strategies, validated names/levels, and context/kind diagnostics. Do not derive or manually implement `Deserialize` anywhere under `src/pom`.

Keep serialization structural and ordered. Do not add custom version tags or promise a stable wire representation.

- [x] **Step 3: Verify serialization GREEN**

Run:

```bash
cargo test --test pom_ast diagnostic_serialization_preserves_order_and_diff_metadata -- --exact
cargo test --test pom_ast
```

Expected: PASS.

- [x] **Step 4: Add the independent trybuild runner and invalid call sites**

Runner:

```rust
#[test]
fn invalid_pom_apis_fail_to_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/pom/diff_slot_rejects_text_node.rs");
    t.compile_fail("tests/ui/pom/diff_slot_present_rejects_explicit_role.rs");
    t.compile_fail("tests/ui/pom/markdown_payload_fields_are_private.rs");
    t.compile_fail("tests/ui/pom/inline_children_reject_heading.rs");
    t.compile_fail("tests/ui/pom/document_rejects_text.rs");
    t.compile_fail("tests/ui/pom/children_have_no_mutable_vector_access.rs");
    t.compile_fail("tests/ui/pom/pom_types_do_not_deserialize.rs");
}
```

Each fixture is minimal:

```rust
// diff_slot_rejects_text_node.rs: expected E0308
DiffSlot::present(DiffStrategy::Recursive, TextNode::new("not XML"));

// diff_slot_present_rejects_explicit_role.rs: expected E0061
DiffSlot::present(role, DiffStrategy::Recursive, xml);

// markdown_payload_fields_are_private.rs: expected E0451
let _ = HeadingNode { level: HeadingLevel::H1, children: InlineChildren::new() };

// inline_children_reject_heading.rs: expected E0308
children.push(BlockContent::heading(HeadingNode::new(
    HeadingLevel::H1,
    InlineChildren::new(),
)));

// document_rejects_text.rs: expected E0308
let _ = Document::new(TextNode::new("orphan"));

// children_have_no_mutable_vector_access.rs: expected E0599
Document::new(BlockChildren::new()).children_mut().clear();

// pom_types_do_not_deserialize.rs: expected E0277
let _: Document = serde_json::from_str("{}").unwrap();
```

Use normal `fn main()` fixtures and import only the required names.

- [x] **Step 5: Run trybuild, inspect RED, then accept exact diagnostics**

First run without `.stderr` files:

```bash
cargo test --test pom_compile_fail
```

Expected: FAIL with seven `wip/*.stderr` files. Inspect every file and confirm the error code/reason listed above; reject any unresolved import or unrelated type inference error.

Then accept reviewed snapshots:

```bash
TRYBUILD=overwrite cargo test --test pom_compile_fail
cargo test --test pom_compile_fail
```

Expected: PASS. These negative guards require no production broadening/narrowing when the already-tested positive API is correctly typed.

- [x] **Step 6: Update implementation status without rewriting the design**

In the design spec, change only the status line and add a short implementation-status note stating that Phase 2 AST exists additively while legacy runtime still uses `SemanticNode`. Do not claim producer, diff, resolver, renderer, or session migration is implemented.

- [x] **Step 7: Run full verification**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
git diff --check
```

Expected: all commands exit 0 with no warnings or failures.

- [x] **Step 8: Commit Phase 2 contracts**

```bash
git add src/lib.rs src/pom tests/pom_ast.rs tests/pom_compile_fail.rs tests/ui/pom \
  docs/superpowers/specs/2026-07-22-agentview-prompt-object-model-design.md \
  docs/superpowers/plans/2026-07-22-agentview-pom-phase-2.md \
  docs/semantic-agent-view.md
git commit -m "feat(pom): complete additive POM AST"
```

Do not stage the pre-existing `.gitignore` or `README.md` modifications.
