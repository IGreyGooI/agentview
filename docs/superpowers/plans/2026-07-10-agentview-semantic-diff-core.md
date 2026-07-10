# AgentView Semantic Diff Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every `AgentView` root implicitly diffable while moving field diff behavior out of Rust value implementations and into generic semantic-tree comparison.

**Architecture:** `AgentView` renders only complete `SemanticFragment` and `SemanticField` values. Derived nodes preserve marked fields as hidden semantic diff slots; a new `semantic_diff` module compares two complete trees, treats the root as an implicit recursive slot, and emits replacement, removal, or delta patches. XML rendering expands present slots normally and ignores slot metadata.

**Tech Stack:** Rust 2021, proc-macro2/quote/syn derive macro, trybuild compile tests, Cargo integration tests.

## Global Constraints

- `#[view(diff)]` implies one addressable node.
- `#[view(attr, diff)]`, `text`, `comment`, `flatten`, and `skip` combinations are compile errors.
- `#[view(element, diff)]` remains an accepted compatibility spelling.
- `#[agent_view(diff)]` remains unsupported because every root is already an implicit diff slot.
- `AgentView` contains complete-render operations only; do not introduce a user-facing diffable marker trait.
- `SemanticField::Empty` means full-render omission and never silently represents delta deletion.
- Existing scalar, structured, option, vector, map, ContextView, and Chess XML outputs remain unchanged.

---

### Task 1: Enforce The Diff Field Syntax

**Files:**
- Modify: `agentview-derive/src/lib.rs`
- Modify: `tests/agentview_derive_compile_fail.rs`
- Create: `tests/ui/attr_with_diff.rs`
- Create: `tests/ui/text_with_diff.rs`
- Create: `tests/ui/comment_with_diff.rs`
- Create: `tests/ui/flatten_with_diff.rs`
- Create: `tests/ui/conflicting_field_modes.rs`
- Create: corresponding `tests/ui/*.stderr` snapshots
- Modify: `tests/agentview_diff.rs`

**Interfaces:**
- Consumes: existing `FieldMode`, `FieldOptions`, and `field_options` parsing.
- Produces: one rendering mode per field and a validated invariant that every diff field is either default/node rendering or the compatible `element` spelling.

- [ ] **Step 1: Add compile-fail fixtures for incompatible diff modes**

Use this shape for each incompatible mode, substituting `attr`, `text`,
`comment`, and `flatten`:

```rust
use agentview::prelude::AgentView;

#[derive(AgentView)]
struct InvalidView {
    #[view(attr, diff)]
    value: String,
}

fn main() {}
```

Add all fixtures to the existing trybuild test:

```rust
t.compile_fail("tests/ui/attr_with_diff.rs");
t.compile_fail("tests/ui/text_with_diff.rs");
t.compile_fail("tests/ui/comment_with_diff.rs");
t.compile_fail("tests/ui/flatten_with_diff.rs");
t.compile_fail("tests/ui/conflicting_field_modes.rs");
```

The conflicting fixture uses:

```rust
#[derive(AgentView)]
struct InvalidView {
    #[view(attr, element)]
    value: String,
}
```

- [ ] **Step 2: Run the compile tests and verify the new cases fail by compiling successfully**

Run:

```bash
cargo test --test agentview_derive_compile_fail
```

Expected: FAIL because the new invalid structs currently compile.

- [ ] **Step 3: Make rendering-mode parsing reject a second mode**

Add a mode setter that preserves the span of the second attribute:

```rust
fn set_field_mode(
    mode: &mut Option<FieldMode>,
    next: FieldMode,
    meta: &syn::meta::ParseNestedMeta<'_>,
) -> syn::Result<()> {
    if mode.is_some() {
        return Err(meta.error("a field can have only one rendering mode"));
    }
    *mode = Some(next);
    Ok(())
}
```

Initialize parsing with `let mut mode = None`, call this helper for `attr`,
`element`, `text`, `comment`, and `flatten`, then store
`mode.unwrap_or(FieldMode::Default)` in `FieldOptions`.
Derive `Copy` as well as `Clone` for `FieldMode` so validation does not consume
the parsed mode.

After parsing each non-skipped field, enforce:

```rust
if options.diff
    && matches!(
        options.mode,
        FieldMode::Attr | FieldMode::Text | FieldMode::Comment | FieldMode::Flatten
    )
{
    return Err(syn::Error::new_spanned(
        &field_ident,
        "`diff` implies node rendering and cannot be combined with this field mode",
    ));
}
```

Keep the existing `skip` validation, which already rejects `skip + diff`.
Collapse the `key`, `key_attr`, and `keyed` nested diff aliases into one parser
branch with `||` so strict Clippy does not report identical branches.

- [ ] **Step 4: Lock the compatible `element + diff` spelling**

Add a second scalar field to a small test view and assert both spellings render
as child nodes:

```rust
#[derive(AgentView)]
#[agent_view(kind = "diff_spellings")]
struct DiffSpellingsView {
    #[view(diff)]
    implicit: String,

    #[view(element, diff)]
    explicit: String,
}

#[test]
fn diff_and_element_diff_both_render_addressable_nodes() {
    let view = DiffSpellingsView {
        implicit: "one".to_owned(),
        explicit: "two".to_owned(),
    };

    assert_eq!(
        agentview::prelude::render_agent_view_xml(&view),
        "<diff_spellings>\n  <implicit>one</implicit>\n  <explicit>two</explicit>\n</diff_spellings>"
    );
}
```

- [ ] **Step 5: Generate and inspect trybuild snapshots**

Run:

```bash
TRYBUILD=overwrite cargo test --test agentview_derive_compile_fail
```

Expected: PASS. Each new `.stderr` must contain either
`` `diff` implies node rendering `` or
`a field can have only one rendering mode` at the annotated field.

- [ ] **Step 6: Run derive and diff tests**

Run:

```bash
cargo test --test agentview_derive --test agentview_diff --test agentview_derive_compile_fail
```

Expected: PASS.

- [ ] **Step 7: Commit the syntax contract**

```bash
git add agentview-derive/src/lib.rs tests/agentview_derive_compile_fail.rs tests/agentview_diff.rs tests/ui
git commit -m "feat(agentview): enforce node-shaped diff fields"
```

---

### Task 2: Preserve Diff Slots In Complete Semantic Trees

**Files:**
- Modify: `src/semantic_view.rs`
- Modify: `tests/semantic_view.rs`

**Interfaces:**
- Consumes: `SemanticField`, `SemanticFragment`, and `SemanticNode::push_field`.
- Produces: `SemanticDiffStrategy`, `SemanticNode::push_diff_field`, hidden `SemanticChild::DiffSlot` values, and unchanged full XML rendering.

- [ ] **Step 1: Add failing full-render tests for present and absent slots**

Add tests that use the semantic API directly:

```rust
use agentview::semantic_view::SemanticDiffStrategy;

#[test]
fn diff_slots_render_present_values_as_normal_nodes() {
    let mut node = SemanticNode::new("scene");
    node.push_diff_field(
        "summary",
        SemanticDiffStrategy::Recursive,
        SemanticField::Attr {
            name: "summary".to_owned(),
            value: "A new lead".to_owned(),
        },
    );

    assert_eq!(
        render_semantic_node_xml(&node),
        "<scene>\n  <summary>A new lead</summary>\n</scene>"
    );
}

#[test]
fn empty_diff_slots_are_omitted_from_full_xml() {
    let mut node = SemanticNode::new("scene");
    node.push_diff_field(
        "summary",
        SemanticDiffStrategy::Recursive,
        SemanticField::Empty,
    );

    assert_eq!(render_semantic_node_xml(&node), "<scene />");
}
```

- [ ] **Step 2: Run the semantic tests and verify the missing API failure**

Run:

```bash
cargo test --test semantic_view
```

Expected: FAIL because `SemanticDiffStrategy` and `push_diff_field` do not
exist.

- [ ] **Step 3: Add semantic child and strategy types**

Define the generated-code-facing strategy and crate-internal child forms:

```rust
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
```

Change `SemanticNode.children` to `Vec<SemanticChild>` and add private metadata:

```rust
intrinsic_diff_strategy: Option<IntrinsicDiffStrategy>,
identity: Option<SemanticFragment>,
```

Initialize both to `None` in `SemanticNode::new`.

- [ ] **Step 4: Add `push_diff_field` and read-only crate accessors**

Use one normalization path for every complete field shape:

```rust
pub fn push_diff_field(
    &mut self,
    field_name: &'static str,
    strategy: SemanticDiffStrategy,
    field: SemanticField,
) {
    let value = match field {
        SemanticField::Empty => None,
        field => Some(SemanticFragment::Node(field_as_node(field_name, field))),
    };
    self.children
        .push(SemanticChild::DiffSlot(SemanticDiffSlot {
            field_name,
            strategy,
            value,
        }));
}
```

Make `push_child`, `push_fragment`, `push_text`, `push_comment`, and
`push_field` wrap ordinary values in `SemanticChild::Fragment`. Add
`pub(crate)` accessors for `tag`, `attrs`, `children`, intrinsic strategy, and
identity so the sibling diff module never mutates private vectors directly.
The accessors include `ordinary_fragments()`, `diff_slots()`,
`has_rendered_children()`, `intrinsic_diff_strategy()`, and `identity()`.

- [ ] **Step 5: Make XML rendering expand slots without rendering metadata**

Render children through one helper:

```rust
fn render_child(child: &SemanticChild, depth: usize) -> Option<String> {
    match child {
        SemanticChild::Fragment(fragment) => Some(render_fragment(fragment, depth)),
        SemanticChild::DiffSlot(slot) => slot
            .value
            .as_ref()
            .map(|fragment| render_fragment(fragment, depth)),
    }
}
```

Filter absent slots before joining child output. Treat a node whose slots are
all absent as self-closing. Existing ordinary text-only nodes must retain the
single-line `<tag>text</tag>` format.

Update every legacy direct child inspection in this file during the same step.
In particular, `delta_or_none` and the temporary map diff implementation must
use `node.has_rendered_children()` instead of reading `children.is_empty()`, and
the text-only XML case must match the filtered visible fragments rather than
the raw `SemanticChild` slice.

- [ ] **Step 6: Run semantic and full derive tests**

Run:

```bash
cargo test --test semantic_view --test agentview_derive
```

Expected: PASS with no XML changes.

- [ ] **Step 7: Commit semantic slot storage**

```bash
git add src/semantic_view.rs tests/semantic_view.rs
git commit -m "feat(agentview): preserve diff slots in semantic trees"
```

---

### Task 3: Replace Type-Specific Diffing With Generic Tree Comparison

**Files:**
- Create: `src/semantic_diff.rs`
- Modify: `src/semantic_view.rs`
- Modify: `src/lib.rs`
- Modify: `agentview-derive/src/lib.rs`
- Modify: `tests/agentview_diff.rs`
- Modify: `tests/semantic_view.rs`

**Interfaces:**
- Consumes: complete roots, `SemanticChild::DiffSlot`, `SemanticDiffStrategy`, and map-node intrinsic metadata.
- Produces: generic `render_agent_view_diff_xml`, implicit root diffing, explicit removal patches, and an `AgentView` trait with no diff methods.

- [ ] **Step 1: Add failing root and generic-empty regression tests**

Lock root behavior explicitly:

```rust
#[test]
fn scalar_root_is_an_implicit_diff_slot() {
    assert_eq!(
        render_agent_view_diff_xml(&"new".to_owned(), &"old".to_owned()),
        Some("new".to_owned())
    );
    assert_eq!(
        render_agent_view_diff_xml(&"same".to_owned(), &"same".to_owned()),
        None
    );
}
```

Add a value whose complete field dynamically becomes empty:

```rust
struct DynamicField(Option<String>);

impl AgentView for DynamicField {
    fn render_root(&self) -> SemanticFragment {
        self.0
            .as_ref()
            .map(|value| SemanticFragment::Text(value.clone()))
            .unwrap_or_else(|| SemanticFragment::Node(SemanticNode::new("none")))
    }

    fn render_field(&self, field_name: &'static str) -> SemanticField {
        self.0
            .as_ref()
            .map(|value| SemanticField::Attr {
                name: field_name.to_owned(),
                value: value.clone(),
            })
            .unwrap_or(SemanticField::Empty)
    }
}

#[derive(AgentView)]
#[agent_view(kind = "dynamic")]
struct DynamicView {
    #[view(diff)]
    value: DynamicField,
}
```

Assert `Some("present") -> None` renders:

```xml
<dynamic rendering_mode="delta">
  <value>
    <none />
  </value>
</dynamic>
```

Also assert `None -> None` returns no patch and `None -> Some("present")`
renders the complete `<value>present</value>` field.

- [ ] **Step 2: Run the focused regressions and verify the empty-delta failure**

Run:

```bash
cargo test --test agentview_diff scalar_root_is_an_implicit_diff_slot
cargo test --test agentview_diff dynamic
```

Expected: the scalar root assertions pass under the legacy default, while the
dynamic removal assertion FAILS with an unaddressable empty delta.

- [ ] **Step 3: Add the internal patch model and root entry point**

Create `src/semantic_diff.rs` with:

```rust
use crate::semantic_view::{
    AgentView, SemanticChild, SemanticDiffSlot, SemanticDiffStrategy,
    SemanticFragment, SemanticNode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum SemanticPatch {
    Unchanged,
    ReplaceRoot(SemanticFragment),
    ReplaceField {
        field_name: &'static str,
        value: SemanticFragment,
    },
    RemoveField {
        field_name: &'static str,
    },
    DeltaNode(SemanticNode),
}

pub(crate) fn diff_agent_views<T: AgentView>(
    current: &T,
    previous: &T,
) -> Option<SemanticFragment> {
    match diff_fragment(&current.render_root(), &previous.render_root()) {
        SemanticPatch::Unchanged => None,
        SemanticPatch::ReplaceRoot(fragment) => Some(fragment),
        SemanticPatch::DeltaNode(node) => Some(SemanticFragment::Node(node)),
        SemanticPatch::ReplaceField { .. } | SemanticPatch::RemoveField { .. } => {
            unreachable!("field patches are consumed by their parent diff slot")
        }
    }
}
```

`diff_fragment` compares non-node roots as complete values. For two nodes it
calls `diff_node`; this is the implicit root slot and requires no type-level
annotation. Define local `operation_node`, `none_field_node`, and
`fragment_as_node` constructors in `semantic_diff.rs`; they build ordinary
semantic nodes and never create new diff slots.

- [ ] **Step 4: Implement recursive node and field-slot comparison**

`diff_node` must follow this exact decision order:

```rust
fn diff_node(current: &SemanticNode, previous: &SemanticNode) -> SemanticPatch {
    if current == previous {
        return SemanticPatch::Unchanged;
    }
    if !same_unmarked_shape(current, previous) {
        return SemanticPatch::ReplaceRoot(SemanticFragment::Node(current.clone()));
    }

    let mut delta = SemanticNode::new(current.tag());
    delta.push_attr("rendering_mode", "delta");
    if let Some(kind) = current.attr("kind") {
        delta.push_attr("kind", kind);
    }

    let mut changed = false;
    for (current_slot, previous_slot) in current.diff_slots().zip(previous.diff_slots()) {
        let patch = diff_slot(current_slot, previous_slot);
        changed |= push_field_patch(&mut delta, patch);
    }

    if changed {
        SemanticPatch::DeltaNode(delta)
    } else {
        SemanticPatch::Unchanged
    }
}
```

`same_unmarked_shape` compares tag, attrs, intrinsic strategy, ordinary
fragments, slot count, slot names, and slot strategies, but not slot values.

`diff_slot` uses these transitions:

```rust
match (&current.value, &previous.value) {
    (None, None) => SemanticPatch::Unchanged,
    (Some(value), None) => SemanticPatch::ReplaceField {
        field_name: current.field_name,
        value: value.clone(),
    },
    (None, Some(_)) => SemanticPatch::RemoveField {
        field_name: current.field_name,
    },
    (Some(current_value), Some(previous_value)) => {
        diff_present_slot(current, current_value, previous_value)
    }
}
```

`push_field_patch` embeds replacements directly, renders removals with
`none_field_node(field_name)`, and embeds nested delta nodes. Unchanged patches
return `false`.

- [ ] **Step 5: Move replace and vector strategies onto rendered nodes**

Implement `diff_present_slot` by dispatching on `SemanticDiffStrategy`:

```rust
match &slot.strategy {
    SemanticDiffStrategy::Recursive => diff_recursive_value(current, previous),
    SemanticDiffStrategy::Replace => diff_replace_value(slot.field_name, current, previous),
    SemanticDiffStrategy::Append => diff_append_list(current, previous),
    SemanticDiffStrategy::Sequence => diff_sequence_list(current, previous),
    SemanticDiffStrategy::Set => diff_set_list(current, previous),
    SemanticDiffStrategy::Keyed(key) => diff_keyed_list(current, previous, key),
}
```

`diff_recursive_value` delegates node pairs with
`IntrinsicDiffStrategy::Map` to map comparison; all other values go through
`diff_fragment`. When that recursive call returns `ReplaceRoot`, convert it to
`ReplaceField` using the current slot name. A recursive `DeltaNode` remains a
nested delta.

Each collection helper reads complete visible child nodes from the two container
nodes and preserves the existing operation order:

- append: inserted current tail;
- sequence: inserted current tail, then removed previous tail, with full-field
  fallback when the common prefix changes;
- set: current-only inserts, then previous-only removals;
- keyed: inserts, removals, then updates, with full-field fallback for missing
  or duplicate keys;
- replace: `<field rendering_mode="delta"><replace>...</replace></field>`.

All full-field fallbacks return `ReplaceField` with the complete current
container; operation results return `DeltaNode`.

- [ ] **Step 6: Give complete map nodes intrinsic key semantics**

In `render_map_node`, set `IntrinsicDiffStrategy::Map`. In `render_map_entry`,
render the key once, clone its normalized semantic fragment into the entry's
hidden `identity`, and apply the original field to the visible entry:

```rust
let key_field = key.render_field("key");
entry.set_identity(field_as_optional_fragment("key", key_field.clone()));
entry.push_field(key_field);
entry.push_field(value.render_field("value"));
```

`field_as_optional_fragment` returns `None` for `SemanticField::Empty` and
otherwise returns `Some(SemanticFragment::Node(field_as_node(...)))`, using the
same normalization as `push_diff_field`.

When a recursive slot contains map nodes, compare entries by hidden identity
and emit insert, remove, and update operations in existing order. Missing or
duplicate rendered identities fall back to the complete current map field.

- [ ] **Step 7: Make derive emit complete fields plus semantic strategies**

Replace generated diff methods and comparisons with one full-render statement
per marked field:

```rust
node.push_diff_field(
    #rendered_field_name,
    #strategy,
    #current_complete_field,
);
```

Generate strategies as:

```rust
SemanticDiffStrategy::Recursive
SemanticDiffStrategy::Replace
SemanticDiffStrategy::Append
SemanticDiffStrategy::Sequence
SemanticDiffStrategy::Set
SemanticDiffStrategy::Keyed(#key_attr)
```

For default fields, `current_complete_field` is
`AgentView::render_field(&self.field, name)`. For compatible `element + diff`,
it is the existing element `SemanticField`. Remove generated `render_diff` and
`render_field_diff` methods, non-diff checks, diff renderer vectors, and
`render_diff_field_expr`.

- [ ] **Step 8: Remove diff behavior from `AgentView` and built-in values**

Reduce the trait to:

```rust
pub trait AgentView {
    fn render_root(&self) -> SemanticFragment;
    fn render_field(&self, field_name: &'static str) -> SemanticField;

    fn render_children(&self) -> Vec<SemanticFragment> {
        vec![self.render_root()]
    }
}
```

Delete the `Option<T>` and `BTreeMap<K, V>` diff overrides and the public typed
helpers `render_vec_*_field_diff` and `render_replace_field_diff`. Keep only
complete renderers in `semantic_view.rs`.

Wire the existing public helper through the new module:

```rust
pub fn render_agent_view_diff_xml<T: AgentView>(view: &T, previous: &T) -> Option<String> {
    crate::semantic_diff::diff_agent_views(view, previous)
        .map(|fragment| render_semantic_fragment_xml(&fragment))
}
```

Declare `mod semantic_diff;` in `src/lib.rs`; preserve the existing
`agentview::semantic_view::render_agent_view_diff_xml` and prelude paths.

- [ ] **Step 9: Run semantic and diff suites**

Run:

```bash
cargo test --test semantic_view --test agentview_diff --test agentview_derive
```

Expected: PASS, including root, removal, nested structured, replace, vector,
and map cases.

- [ ] **Step 10: Run ContextView and Chess regressions**

Run:

```bash
cargo test --test agent_view_session --test chess_agent_session --test agentview_cli
```

Expected: PASS with unchanged prompt XML.

- [ ] **Step 11: Commit the generic semantic diff engine**

```bash
git add src/semantic_diff.rs src/semantic_view.rs src/lib.rs agentview-derive/src/lib.rs tests/agentview_diff.rs tests/semantic_view.rs
git commit -m "refactor(agentview): diff complete semantic trees"
```

---

### Task 4: Document And Verify The New Contract

**Files:**
- Modify: `docs/semantic-agent-view.md`
- Modify: `docs/superpowers/specs/2026-07-10-agentview-semantic-diff-core-design.md` only if implementation names differ without changing semantics

**Interfaces:**
- Consumes: final derive syntax and generic semantic diff behavior.
- Produces: documentation that describes root-first diffing and contains no type-specific diff method API.

- [ ] **Step 1: Rewrite the Field Diffs and Semantic Tree sections**

Document these statements verbatim in substance:

```text
Every AgentView root is the implicit first diff slot. No type-level diff marker
is needed. AgentView renders a complete semantic tree; #[view(diff)] records a
field boundary in that tree. The generic diff engine compares two complete
trees. Unmarked changes replace the current node, marked fields may recurse,
and a removed marked field renders an explicit <none /> patch.
```

Remove documentation that presents `AgentView::render_diff`,
`render_field_diff`, or collection diff helpers as extension points. Add the
compile-error combinations from Task 1.

- [ ] **Step 2: Run formatting and the complete workspace test suite**

Run:

```bash
cargo fmt --all
cargo fmt --all --check
cargo test --workspace
```

Expected: both commands PASS.

- [ ] **Step 3: Run strict linting**

Run:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: PASS. In particular, collapse the identical `key`, `key_attr`, and
`keyed` parser aliases in Task 1 so `if_same_then_else` does not reappear.

- [ ] **Step 4: Confirm type-specific diff APIs are gone**

Run:

```bash
rg -n "render_diff_field|render_field_diff|render_vec_.*_field_diff|render_replace_field_diff" src agentview-derive tests examples
```

Expected: no matches.

- [ ] **Step 5: Inspect the final change set for unrelated edits**

Run:

```bash
git status --short
git diff --check
```

Expected: no whitespace errors; only semantic diff implementation, tests, and
documentation are newly changed by this plan. Pre-existing dirty paths from the
AgentView and Chess work may remain and must not be reverted or included merely
to make the worktree clean.

- [ ] **Step 6: Commit documentation and cleanup**

```bash
git add docs/semantic-agent-view.md docs/superpowers/specs/2026-07-10-agentview-semantic-diff-core-design.md
git commit -m "docs: describe root-first semantic diffing"
```
