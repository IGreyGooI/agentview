# AgentView Derive: POM AST And Legacy Diff

This document describes the current `AgentView` derive syntax. The derive now
builds a typed Prompt Object Model (POM) AST. During the user/context migration,
ordinary XML and display views also receive a legacy semantic implementation so
the existing context differ can keep operating.

The POM view is an agent-facing frontend tree. A struct's `kind` describes the
concrete XML view model being built. A field name describes the role that a
child value plays inside its parent.

## Core Pipeline

`AgentView` converts Rust values into a complete, typed AST before any prompt
text is produced. The POM path follows this pipeline:

```text
Rust struct instance
    -> #[derive(AgentView)] / AgentView::build_root
    -> complete POM AST
    -> role-specific resolver
    -> ResolvedDocument
    -> prompt renderer
```

`#[agent_view(kind = "...")]` produces `XmlNode`,
`#[agent_view(document)]` produces `Document`,
`#[agent_view(markdown = "paragraph")]` produces `ParagraphNode`, and
`#[agent_view(display)]` produces `TextNode`. The associated `Root` type makes
these distinctions compile-time facts.

The current context delta path remains a compatibility path. It builds current
and previous legacy trees independently, compares those trees, and only then
renders the resulting delta tree:

```text
current Rust instance  -> complete current semantic tree
previous Rust instance -> complete previous semantic tree
                                      |
                                      v
                              semantic tree diff
                                      |
                                      v
                              delta semantic tree
                                      |
                                      v
                                prompt renderer
```

The diff engine does not compare Rust structs directly and does not compare
already-rendered prompt strings. On an XML view, `#[view(diff)]` now retains the
boundary as a real POM `DiffSlot`; the temporary legacy implementation also
retains the same boundary as `SemanticDiffSlot`. POM `DiffSlot::present` only
accepts `XmlNode`, so a `Document`, Markdown root, or `TextNode` cannot become a
diff value. System resolution materializes present slots without diffing. The
stateful POM user-document differ is not integrated yet.

## Derive Syntax

Use `#[derive(AgentView)]` on named structs:

```rust
use agentview::prelude::*;

#[derive(AgentView)]
#[agent_view(kind = "actor")]
struct ActorView {
    id: String,
    name: String,

    #[view(element)]
    goal: String,
}
```

Container modes are mutually exclusive:

- `#[agent_view(kind = "...")]` builds an XML `XmlNode`. If omitted on a
  structured view, the kind is derived from the Rust type name in snake case,
  so `ActorView` becomes `actor_view`.
- `#[agent_view(display)]` builds a scalar `TextNode` using `Display`.
- `#[agent_view(document)]` builds a block-level `Document`.
- `#[agent_view(markdown = "paragraph")]` builds a `ParagraphNode`.

`#[agent_view(tag = "...")]` is not supported. The container metadata is `kind`,
not `tag`.

XML view field modes are:

- default: call the field value's POM field adapter. Scalars normally become
  attributes; structured values become role-wrapped XML children.
- `#[view(attr)]`: render the field as an attribute using `Display`.
- `#[view(attr, name = "...")]`: render the field as an attribute with a custom
  output name. Prefer changing the view struct field name when possible.
- `#[view(element)]`: render the field as a child element using `Display`.
- `#[view(text)]`: render the field as direct text inside the current node using
  `Display`.
- `#[view(flatten)]`: render the field value's children directly into the
  current node. For `Vec<T>`, this flattens the vector items without rendering
  the field-name wrapper. This splices rendered child fragments into the parent;
  it does not merge child attributes into the parent node.
- `#[view(root)]`: insert the child value's own derived `XmlNode` root without a
  field-role wrapper. This is used when a document-owned wrapper selects tool
  contract nodes.
- `#[view(code_span)]`: create a field-role XML element whose child is a
  Markdown code span.
- `#[view(skip)]`: omit the field from the semantic view entirely. Skipped fields
  are not rendered, are not compared for diffing, and do not need to implement
  `AgentView` or `Display`. Use this for runtime-only state that belongs on the
  Rust view model but should not enter the agent-facing tree, such as a reply
  schema or cached handle.

`#[view(comment)]` is no longer supported because POM has no XML comment node.
Use a prompt-visible Markdown note or an explicit XML element.

Document view fields must choose a block mode:

- `#[view(heading = 1)]` through `#[view(heading = 6)]`;
- `#[view(paragraph)]`;
- `#[view(ordered_list)]` or `#[view(unordered_list)]`, where each item derives
  `AgentView<Root = ParagraphNode>`;
- `#[view(xml)]`, where the field derives `AgentView<Root = XmlNode>`.

A `#[agent_view(markdown = "paragraph")]` view accepts `#[view(text)]` and
`#[view(code_span)]` fields. Field order is AST child order.

`#[view(attr = "...")]` and `#[view(children)]` are still accepted as legacy
aliases, but new examples should use `name = "..."` and `flatten`.

`#[view(skip)]` cannot be combined with rendering, naming, or diff attributes.
If a field is skipped, it has no prompt-facing name and no diff behavior.

The built-in scalar types render as root text and as field attributes:

- `String` and `str`
- `bool`
- `char`
- `u8`, `u16`, `u32`, `u64`, `u128`, `usize`
- `i8`, `i16`, `i32`, `i64`, `i128`, `isize`
- `f32`, `f64`

For scalar fields, the default field behavior is attribute rendering. This:

```rust
#[derive(AgentView)]
#[agent_view(kind = "actor")]
struct ActorView {
    id: String,
    name: String,

    #[view(element)]
    goal: String,
}
```

renders at the root as:

```xml
<actor id="actor.1" name="Rachel Verinder">
  <goal>find the Moonstone</goal>
</actor>
```

For a user-defined scalar, derive `AgentView` explicitly with
`#[agent_view(display)]` and implement `Display`:

```rust
#[derive(AgentView)]
#[agent_view(display)]
struct ActorId(String);

impl std::fmt::Display for ActorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(AgentView)]
#[agent_view(kind = "display_actor")]
struct DisplayActorView {
    id: ActorId,
    name: String,
}
```

renders as:

```xml
<display_actor id="actor.1" name="Rachel" />
```

`kind`, `display`, `document`, and `markdown` are mutually exclusive.
`display` means the type is a scalar view; `kind` means an XML view. Unlike the
other modes, `display` derive does not require a named-field struct; it works
for newtypes and enums as long as the type implements `Display`.

## POM Root And Field Positions

The public trait has one typed AST entry point:

```rust
pub trait AgentView {
    type Root;

    fn build_root(&self) -> Result<Self::Root, PomError>;
}
```

`build_root` always builds a complete root. XML roots use the view's `kind` as
the element name. `Option<T>::Root` is `Option<T::Root>`; nested `None` values
are omitted rather than being converted into a synthetic POM node.

Nested XML positions use a derive-support adapter named `AgentViewValue`. It
returns `ViewField`, not a direct mutation of the parent:

```rust
pub trait AgentViewValue: AgentView {
    fn build_field(&self, role: XmlName) -> Result<ViewField, PomError>;
    fn build_children(&self) -> Result<MixedChildren, PomError>;
}
```

`ViewField` has these shapes:

```rust
pub enum ViewField {
    Empty,
    Attribute(XmlAttribute),
    Content(MixedContent),
    Children(MixedChildren),
}
```

Scalar fields usually return `Attribute`. Structured fields usually return
`Content`. Optional absent fields return `Empty`. Flattened fields return
`Children`, preserving ordinary nodes and `DiffSlot` edges in order. For a
derived struct field, the field name becomes the element name, and the child's
own view kind is preserved as `kind="..."`.

For example:

```rust
#[derive(AgentView)]
#[agent_view(kind = "scene")]
struct SceneView {
    id: String,
    protagonist: ActorView,

    #[view(element)]
    summary: String,
}
```

renders as:

```xml
<scene id="scene.1">
  <protagonist kind="actor" id="actor.1" name="Rachel Verinder">
    <goal>find the Moonstone</goal>
  </protagonist>
  <summary>The Moonstone has vanished.</summary>
</scene>
```

Text fields become direct text. Prompt-visible notes use explicit elements:

```rust
#[derive(AgentView)]
#[agent_view(kind = "annotated_actor")]
struct AnnotatedActorView {
    id: String,

    #[view(text)]
    description: String,

    #[view(element)]
    debug_note: String,

    #[view(element)]
    goal: String,
}
```

renders as:

```xml
<annotated_actor id="actor.1">
  Rachel Verinder, heiress of the Moonstone.
  <debug_note>loaded from director state</debug_note>
  <goal>find the Moonstone</goal>
</annotated_actor>
```

Read this as:

- `protagonist` is the role of the child inside `SceneView`.
- `kind="actor"` is the concrete view kind of the child.
- `id`, `name`, and `summary` are normal rendered fields.

Prefer making the view struct prompt-facing. Field names should usually be the
names the agent sees, not the names from the domain model. For example, if the
prompt should show `n="8"`, make the projection field `n`:

```rust
#[derive(AgentView)]
#[agent_view(kind = "rank")]
struct RankView {
    n: u8,

    #[view(flatten)]
    squares: Vec<SquareView>,
}
```

renders as:

```xml
<rank n="8">
  <square id="a8">r</square>
  <square id="b8">n</square>
</rank>
```

Use `#[view(skip)]` when a prompt-facing struct still needs to carry
non-prompt state:

```rust
#[derive(AgentView)]
#[agent_view(kind = "turn_prompt")]
struct TurnPromptView {
    #[view(element)]
    task: String,

    #[view(skip)]
    reply_schema: serde_json::Value,
}
```

renders as:

```xml
<turn_prompt>
  <task>Choose a move.</task>
</turn_prompt>
```

If a view struct cannot use the output name directly, use the explicit escape
hatch:

```rust
#[derive(AgentView)]
#[agent_view(kind = "rank")]
struct RankView {
    #[view(attr, name = "n")]
    rank: u8,
}
```

## Collecting Agent-Facing Views

`AgentView` describes how a view renders. It should not also own the runtime
logic for reading application state. Use `AgentViewCollect<Source>` for the
small projection step from a source snapshot into the agent-facing view:

```rust
pub trait AgentViewCollect<Source: ?Sized>: Sized {
    fn collect(source: &Source) -> Self;
}

impl AgentViewCollect<GameState> for BoardView {
    fn collect(state: &GameState) -> Self {
        // Build prompt-facing fields from runtime/domain state.
    }
}
```

Then a `ViewModel` capture path can stay direct:

```rust
async fn capture_view(&self, source: &GameSource) -> BoardView {
    BoardView::collect(&source.snapshot())
}
```

This keeps the boundary explicit:

- domain/source state owns runtime facts and handles.
- view structs own prompt-facing names and rendering shape.
- `AgentViewCollect` owns the conversion between the two.

## Collections

`Vec<T>` builds a field container whose tag is the field name. Each item is
built from `T::build_root`.

For a vector of view structs:

```rust
#[derive(AgentView)]
#[agent_view(kind = "cast")]
struct CastView {
    actors: Vec<ActorView>,
}
```

renders as:

```xml
<cast>
  <actors>
    <actor id="actor.1" name="Rachel">
      <goal>find the Moonstone</goal>
    </actor>
    <actor id="actor.2" name="Franklin">
      <goal>protect Rachel</goal>
    </actor>
  </actors>
</cast>
```

If an item renders as root text, such as `String`, the item is wrapped in a
stable `<item>` node:

```xml
<notebook>
  <notes>
    <item>first note</item>
    <item>second note</item>
  </notes>
</notebook>
```

When a vector is rendered directly as the root view, it uses `<list>` as the
fallback container:

```xml
<list>
  <item>first note</item>
  <item>second note</item>
</list>
```

`BTreeMap<K, V>` builds a field container whose tag is the field name. Each pair
builds an `<entry>` node. The key and value pass through the normal nested-value
adapter:

```rust
push_view_field(&mut entry, key.build_field(XmlName::try_from("key")?)?)?;
push_view_field(&mut entry, value.build_field(XmlName::try_from("value")?)?)?;
```

For scalar strings, that becomes key/value attributes:

```xml
<metadata>
  <properties>
    <entry key="location" value="drawing_room" />
    <entry key="mood" value="tense" />
  </properties>
</metadata>
```

For structured keys or values, the same rule naturally expands into key/value
slots:

```xml
<assignment_set>
  <assignments>
    <entry>
      <key kind="actor" id="actor.1" name="Rachel">
        <goal>recover the Moonstone</goal>
      </key>
      <value kind="goal" id="goal.1">
        <description>find the Moonstone</description>
      </value>
    </entry>
  </assignments>
</assignment_set>
```

When a map is rendered directly as the root view, it uses `<map>` as the
fallback container.

## Optional Values

`Option<T>` delegates to `T` when it is `Some`.

For scalar values:

```rust
#[derive(AgentView)]
#[agent_view(kind = "optional_actor")]
struct OptionalActorView {
    id: String,
    alias: Option<String>,
}
```

renders a present value as a normal attribute:

```xml
<optional_actor id="actor.1" alias="Rosanna" />
```

When an optional field is `None`, it is omitted:

```xml
<optional_actor id="actor.2" />
```

For structured values, `Some` follows normal field rendering:

```xml
<optional_actor id="actor.1">
  <goal kind="goal" id="goal.1">
    <description>find the Moonstone</description>
  </goal>
</optional_actor>
```

This also applies when `T` renders a container node. For example,
`Option<Vec<ActorView>>` renders `Some` as the normal vector container and
omits the whole container when it is `None`:

```xml
<optional_actor id="actor.1">
  <actors>
    <actor id="actor.2" name="Franklin">
      <goal>protect Rachel</goal>
    </actor>
  </actors>
</optional_actor>
```

In POM, a root `Option<T>` has `Root = Option<T::Root>`: `Some` builds the
inner root and `None` returns `None`. The temporary legacy renderer preserves
its old compatibility output for root `None`:

```xml
<none />
```

## Field Diffs

In POM, `#[view(diff)]` records a real `DiffSlot` edge in the complete
`XmlNode`. A POM root is not automatically stateful: request assembly must place
the XML root in an explicit outer `DiffSlot` when it wants a document-level
boundary.

The currently shipped `render_agent_view_diff_xml` compatibility API still
treats every legacy root as the implicit first diff slot. Its generic diff
engine compares two complete legacy trees: unmarked changes replace the current
node, marked fields may recurse, and a removed marked field renders an explicit
`<none />` patch.

Diff rendering is requested explicitly:

```rust
render_agent_view_diff_xml(&current, &previous)
```

For example, this root can produce either a complete replacement or a delta:

```rust
#[derive(AgentView)]
#[agent_view(kind = "scene")]
struct SceneView {
    id: String,
    title: String,

    #[view(diff)]
    summary: String,

    #[view(diff(append))]
    actors: Vec<ActorView>,

    #[view(diff)]
    protagonist: ActorView,
}
```

`#[agent_view(diff)]` is not supported. On the compatibility API, the root
already participates; on POM, an outer boundary is an explicit `DiffSlot`
owned by document assembly. `AgentView` has no type-specific diff methods or
collection diff helpers to implement.

The current legacy comparison contract is:

- Equal values produce `None`.
- The root is always eligible for recursive comparison. A changed scalar or
  other non-node root is replaced with its complete current rendering.
- If a node's tag, attributes, or unmarked content changes, the diff replaces
  that complete current node.
- `#[view(diff)]` marks a field that can be expanded. If only diff fields
  changed, the current level renders a delta node and includes only changed diff
  fields.
- On a default scalar field, `#[view(diff)]` implies node rendering. This keeps
  the field patch shape stable in both full and delta output.
- A structured diff field can recurse into its own `#[view(diff)]` fields.
- `#[view(diff(replace))]` marks a field that should be replaced as a whole
  when it changes, instead of recursively diffing inside it.
- `Vec<T>` diff fields must choose a collection mode:
  `#[view(diff(append))]`, `#[view(diff(seq))]`, `#[view(diff(set))]`, or
  `#[view(diff(key = "attr_name"))]`.
- `BTreeMap<K, V>` diff fields use their map keys as identity and render
  `insert`, `remove`, and `update` operation wrappers.

### Invalid Diff Combinations

`diff` requires an independently addressable node. The following combinations
are compile errors:

```rust
#[view(attr, diff)]
#[view(text, diff)]
#[view(flatten, diff)]
#[view(skip, diff)]
```

`#[view(comment)]` is invalid with or without `diff`.
`#[view(element, diff)]` remains a supported compatibility spelling and has the
same node-shaped behavior as `#[view(diff)]`. Conflicting rendering modes, such
as `#[view(attr, element)]`, are also compile errors.

For example, if an unmarked field such as `title` changes, the root is rendered
as the current full view:

```xml
<scene id="scene.1" title="A different title">
  <summary>The Moonstone has vanished.</summary>
  <protagonist kind="actor" id="actor.1" name="Rachel">
    <goal>find the Moonstone</goal>
  </protagonist>
</scene>
```

If only a scalar diff field changes, unchanged fields are omitted:

```xml
<scene rendering_mode="delta">
  <summary>Rachel has a new lead.</summary>
</scene>
```

A nested structured diff field can recurse. On field delta nodes,
`rendering_mode="delta"` is first and `kind` is second when present:

```xml
<scene rendering_mode="delta">
  <protagonist rendering_mode="delta" kind="actor">
    <goal>question the servants</goal>
  </protagonist>
</scene>
```

If a field should be reread as one unit, use `replace`:

```rust
#[derive(AgentView)]
#[agent_view(kind = "status_panel")]
struct StatusPanelView {
    id: String,

    #[view(diff(replace))]
    state: StatusStateView,
}
```

This emits a replacement operation for the current field value:

```xml
<status_panel rendering_mode="delta">
  <state rendering_mode="delta">
    <replace>
      <state kind="status_state">
        <status>changed</status>
      </state>
    </replace>
  </state>
</status_panel>
```

A diff list field with `append` mode renders append-only tails as explicit
inserts:

```xml
<scene rendering_mode="delta">
  <actors rendering_mode="delta">
    <insert>
      <actor id="actor.2" name="Franklin">
        <goal>protect Rachel</goal>
      </actor>
    </insert>
  </actors>
</scene>
```

`seq` mode is for ordered lists where only the tail should normally change. It
renders inserted tail items and removed tail items. If the common prefix changes,
it falls back to rendering the full current list for that field.

`set` mode treats item order as non-semantic and renders inserted and removed
items:

```xml
<inventory rendering_mode="delta">
  <items rendering_mode="delta">
    <insert>
      <item>moonstone</item>
    </insert>
    <remove>
      <item>lantern</item>
    </remove>
  </items>
</inventory>
```

`key` mode treats a vector like a keyed collection. The key is read from an
attribute on each item's rendered root node:

```rust
#[derive(AgentView)]
#[agent_view(kind = "cast")]
struct CastView {
    #[view(diff(key = "id"))]
    actors: Vec<ActorView>,
}
```

This uses each actor root node's `id` attribute and renders `insert`, `remove`,
and `update` operation wrappers:

```xml
<cast rendering_mode="delta">
  <actors rendering_mode="delta">
    <update>
      <actor id="actor.1" name="Rachel">
        <goal>question the servants</goal>
      </actor>
    </update>
  </actors>
</cast>
```

If keyed diff cannot find the requested key attribute, or if a key appears more
than once, it conservatively falls back to rendering the full current list field.

Map fields use keys as identity:

```xml
<metadata rendering_mode="delta">
  <properties rendering_mode="delta">
    <insert>
      <entry key="mood" value="tense" />
    </insert>
  </properties>
</metadata>
```

When a diff field changes from `Some(value)` to `None`, the field renders an
explicit none patch:

```xml
<maybe_alias rendering_mode="delta">
  <alias>
    <none />
  </alias>
</maybe_alias>
```

## POM Tree And Legacy Compatibility

The derive macro's primary result is POM AST: `XmlNode`, `Document`,
`ParagraphNode`, or `TextNode`. It does not render prompt text. Resolution and
rendering are later steps.

Until the user/context path migrates, XML and display derives also generate an
implementation of `semantic_view::AgentView`. That compatibility tree retains:

- `Node(SemanticNode)`: an element with attributes and children.
- `Text(String)`: escaped text.

For a derived XML record, ordinary fields become ordinary POM content and each
`#[view(diff)]` field becomes a `DiffSlot` with its complete XML value and
selected strategy. The generated compatibility tree records the equivalent
`SemanticDiffSlot`. Present slots appear as ordinary children in full output;
absent optional slots are omitted. During legacy comparison, unchanged slots
disappear from the delta, changed slots emit their patch, and a
present-to-absent transition becomes `<field><none /></field>`.

Legacy-compatible XML/display views also implement `AgentViewRoot`, which lets
them enter the existing `ContextView` / `PromptRenderable` pipeline without
hand-written rendering boilerplate. `Document` and Markdown roots do not
implement that legacy marker. An XML `#[derive(AgentView)]` struct can therefore
still be the `ContextViewBuilder::View` type directly:

```rust
#[derive(AgentView)]
#[agent_view(kind = "hello")]
struct HelloView {
    greeting: String,

    #[view(diff)]
    name: Option<String>,
}

impl ContextViewBuilder for HelloViewBuilder {
    type View = HelloView;
    // capture returns HelloView
}
```

This bridge is intentionally marker-based. Built-in scalar `String` still
renders as plain prompt text through the legacy `PromptRenderable for String`
implementation, so ordinary prompts are not XML-escaped just because `String`
also implements `AgentView`.

## Derived POM System Prompt Path

System prompts use the same derive to author structured Markdown and XML. This
path does not replace the legacy user/context differ yet.

The implemented system pipeline is:

```text
POM Document
    -> resolve_system_document
    -> ResolvedDocument
    -> render_pom_document / PromptRenderable
    -> AgentTurnRequest.system
```

`Document` is the authoring type and may contain `DiffSlot` edges.
`resolve_system_document` applies system semantics recursively:

- a present slot is expanded to its complete `XmlNode`;
- an absent slot is omitted;
- slot roles and strategies are ignored;
- no previous state, cursor, delta, warning, or session mutation is involved.

The result is the opaque, slot-free `ResolvedDocument`. Only that resolved type
implements `PromptRenderable`; an unresolved `Document` cannot be sent directly
through the prompt bridge. The renderer then emits canonical Markdown plus XML,
including Markdown/XML context escaping, ordered and unordered lists, dynamic
code fences, code spans, and XML mixed content. Structures with no canonical
CommonMark representation, such as an empty paragraph, zero-item list, or empty
code span, return `PomRenderError` instead of being silently dropped.

The shape used by the streaming tool-loop example is:

```rust
#[derive(AgentView)]
#[agent_view(document)]
struct DemoSystemPromptView {
    #[view(heading = 1)]
    title: String,

    #[view(paragraph)]
    task: String,

    #[view(ordered_list)]
    workflow: Vec<DemoWorkflowStepView>,

    #[view(xml)]
    response_contract: DemoResponseContractView,
}

let document = system_prompt_view.build_root()?;
let system_prompt = resolve_system_document(document);
```

Both the response contract and every streaming tool contract are derived
`XmlNode` views. `StreamingTool<C>` requires `AgentView<Root = XmlNode>`; there
is no separate `build_prompt_node` or required `tag` method. Registration uses
the `name` attribute for a derived `<tool>` contract; every other contract uses
its derived XML root name. An ordinary payload attribute named `name` therefore
cannot silently change the parser registration identity. A generic `<tool>`
contract without `name` is rejected during registration.

The chess example uses the same derived system-document path while deliberately
keeping its `ChessTaskView` turn prompt and semantic board diff on the legacy path.
Chess is an externally controlled `AgentViewApp`, not a provider-backed
`Agent`, so `AgentViewApp` does not consume `build_system_prompt`. The standalone
example explicitly builds and prints the resolved chess system prompt to make
that boundary visible.

This is deliberately a system-focused integration slice. The current user
prompt, context capture, semantic diff, and fixed `## View` / `## Turn Prompt`
composition still use the legacy `SemanticNode` pipeline. User-document
resolution, a `UserDocumentCursor`, and the final POM differ cutover remain
separate work.
