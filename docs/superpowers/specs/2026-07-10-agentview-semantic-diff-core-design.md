# AgentView Semantic Diff Core Design

## Goal

Make diffing a property of the rendered semantic view tree rather than custom
behavior implemented by each Rust value type. The user-facing model remains:

- `AgentView` renders a complete semantic view.
- every `AgentView` root is the implicit first diff slot.
- `#[view(diff)]` marks a field boundary that may be expanded in a delta.
- collection diff arguments select the comparison algorithm for that field.

This change covers three contracts:

1. `diff` always implies node rendering.
2. `AgentView` only renders complete semantic values.
3. deletion, replacement, and no change are distinct patch outcomes.

Collection XML behavior already covered by tests must remain unchanged. Prompt
composition, `ContextView`, collection identity, and transport epochs are out of
scope.

## Derive Syntax

The canonical scalar syntax is:

```rust
#[view(diff)]
summary: String,
```

It renders as a node in both full and delta output:

```xml
<summary>Current summary</summary>
```

`#[view(element, diff)]` remains accepted as a compatibility spelling and is
normalized to the same behavior. `name = "..."` may still rename the node.

The following combinations are compile errors because they do not produce an
independently addressable node:

```rust
#[view(attr, diff)]
#[view(text, diff)]
#[view(comment, diff)]
#[view(flatten, diff)]
#[view(skip, diff)]
```

`diff(replace)` and collection modes also imply node rendering and follow the
same validation. Conflicting rendering modes remain compile errors rather than
depending on attribute order.

`#[agent_view(diff)]` remains unsupported because it would only repeat the
default: an `AgentView` root always participates in diffing.

## Full Semantic Tree

`AgentView` keeps only complete-render operations:

```rust
pub trait AgentView {
    fn render_root(&self) -> SemanticFragment;
    fn render_field(&self, field_name: &'static str) -> SemanticField;

    fn render_children(&self) -> Vec<SemanticFragment>;
}
```

The diff-specific methods are removed from this trait. Derived implementations,
scalars, `Option`, vectors, and maps all produce complete semantic values only.

A `SemanticNode` retains fields marked with `#[view(diff)]` as semantic child
slots rather than flattening them into indistinguishable fragments:

```rust
enum SemanticChild {
    Fragment(SemanticFragment),
    DiffSlot {
        field_name: String,
        strategy: DiffStrategy,
        value: Option<SemanticFragment>,
    },
}
```

Each diff slot records:

- the field name;
- the complete current field value, with `None` representing
  `SemanticField::Empty`;
- the selected diff strategy.

`SemanticNode::push_diff_field` normalizes the complete `SemanticField` to one
addressable node, stores it as a `DiffSlot`, and does not also copy it into the
ordinary child list. XML rendering expands a present slot exactly like an
ordinary child and skips an absent slot. This gives the generic diff engine
enough information to distinguish an omitted optional field from an unmarked
child and to recurse into a derived child's own diff slots without maintaining
two copies of the tree.

Collection nodes retain an internal intrinsic strategy when their complete
renderer establishes one, such as map-key identity. An explicit field strategy
such as `append`, `seq`, `set`, `key`, or `replace` takes precedence. Plain
`#[view(diff)]` uses the child node's intrinsic strategy when present and
otherwise uses recursive field expansion. These are semantic-tree annotations,
not methods on the Rust value type.

## Generic Diff

`render_agent_view_diff_xml` renders current and previous values completely,
then compares their semantic fragments. It treats the root as an implicit
recursive diff slot even though the root is not stored inside a parent
`SemanticNode`.

Root behavior is therefore always enabled:

- equal roots produce no patch;
- a change to unmarked root content replaces the complete current root;
- if unmarked root content is equal, changes may expand through the root's
  marked diff slots and produce a root delta;
- scalar and other non-node roots compare as complete values and are replaced
  when changed.

For a node:

1. Compare the node tag, attributes, and all unmarked content.
2. If any unmarked content changed, replace the node with its complete current
   rendering.
3. Otherwise compare each marked diff slot using its strategy.
4. Omit unchanged slots.
5. If at least one slot changed, emit a node with
   `rendering_mode="delta"` followed by the changed field patches.
6. If no slot changed, return no patch.

Recursive diff behaves naturally for scalar nodes. Their text is unmarked
content, so a changed scalar field emits the complete current field node:

```xml
<scene rendering_mode="delta">
  <summary>Rachel has a new lead.</summary>
</scene>
```

Existing collection algorithms operate on the rendered child nodes captured in
the semantic tree. They no longer call diff methods on `Vec<T>` or
`BTreeMap<K, V>`.

## Patch Outcomes

Full-tree absence and delta deletion must not share one meaning. The internal
comparison result is:

```rust
enum SemanticPatch {
    Unchanged,
    ReplaceRoot(SemanticFragment),
    ReplaceField {
        field_name: String,
        value: SemanticFragment,
    },
    RemoveField {
        field_name: String,
    },
    DeltaNode(SemanticNode),
}
```

Collection insert, remove, and update operations are children of a
`DeltaNode`; they do not require another public patch type.

`SemanticField::Empty` continues to mean "omit this field" in a complete tree.
When a marked slot changes from a value to `Empty`, the diff engine converts it
to `Remove` and renders the established explicit-none form:

```xml
<scene rendering_mode="delta">
  <summary>
    <none />
  </summary>
</scene>
```

An `Empty` to `Empty` transition is `Unchanged`. An `Empty` to a value emits the
complete current field node. No changed field may disappear into an empty delta
node.

## Compatibility

The public helpers `render_agent_view_xml` and
`render_agent_view_diff_xml` keep their current signatures and XML contract.
Existing collection and chess tests should continue to pass without expected
output changes.

Removing diff methods from `AgentView` is a source-level breaking change for
manual implementations that override them. The repository contains such
implementations only for built-in collection/optional behavior; those move into
the semantic diff engine. This is preferable to preserving two competing diff
extension mechanisms.

## Tests

The implementation is accepted when tests cover:

- root diffing without any type-level annotation;
- an unmarked root field change replacing the complete root;
- a marked child-only change producing a root delta;
- compile failures for every incompatible `diff` field mode;
- `#[view(diff)]` and `#[view(element, diff)]` producing identical nodes;
- scalar, structured, and optional recursive field diffs;
- value-to-empty, empty-to-value, and empty-to-empty transitions;
- a custom `AgentView` returning `SemanticField::Empty` without producing an
  unaddressable empty delta;
- unmarked changes replacing the complete current node;
- unchanged fields being omitted from a delta;
- all existing append, sequence, set, keyed-vector, and map diff outputs.
