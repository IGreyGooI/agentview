# AgentView Prompt Object Model（POM）设计

> 状态：设计已批准，第一版 POM-first request path 已实现。本文同时记录目标契约与 2026-07-24 的实现状态；未落地的目标会明确标为差异，不再用旧迁移阶段描述当前行为。

> 实现状态（2026-07-24）：`#[derive(AgentView)]` 直接生成 typed POM AST；`AgentViewModel` 分别构造完整 system/user `Document`。user resolver 只保存 outermost XML slot baseline，并支持 recursive/replace/append/sequence/set/keyed/map diff。provider-backed `Agent` 将 cursor 与 history 放在同一成功提交边界；`AgentViewApp` 则在稳定、可渲染的 snapshot 返回时发布 cursor。demo、chess、hello、streaming-tool contract 和 turn artifact 已切到 POM。旧 `SemanticNode`、`TemplateEngine` 与 `PromptRenderable` 仍作为显式 compatibility API 存在，但不再参与 `Agent`、`AgentViewApp` 或 prelude 推荐路径。当前实现尚未把 differ 的内部结果抽成公开 typed `XmlPatch`；它直接产生待 resolver materialize 的 optional delta/full `XmlNode`。

## 1. 文档目标

AgentView 原来的 semantic tree 是 XML-only，Markdown 只以未经结构化的 `String` 存在于 template 与最终 envelope。POM 已取代这条 request path：Markdown、XML、Text 和 diff edge 现在先进入同一棵 typed object tree，最后才由统一 renderer 生成 provider text。

下一代模型正式命名为 **POM（Prompt Object Model）**。它借用 DOM（Document Object Model）的思想，为 prompt 提供规范化、可组合、可遍历的 object tree：

```text
DOM: source document -> Document/Element/Text objects -> serializer
POM: Rust view data  -> Document/Markdown/XML/Text objects -> differ/renderer
```

POM 同时表达规范化 Markdown、XML、文本和 XML-only `DiffSlot`。它是 authoring-first semantic model，不是 Markdown/XML source parser，也不追求 source round-trip。

一次 LLM request 持有两个独立 POM `Document`：一个 system document，一个 user document。它们使用同一套 node model，但分别渲染为 provider 的 system/user message；POM 本身不把两者合并成一个总 document。

本文不是 renderer 设计，也不是完整 diff 算法设计。涉及 renderer 或 diff 的内容只用于约束 AST 必须保存哪些信息。

## 2. 已确认的设计边界

以下内容已经在讨论中确认：

- 允许 breaking change，不要求保留现有 `SemanticNode`、`SemanticFragment`、`SemanticField` public API。
- 正式概念名称是 `Prompt Object Model (POM)`；public code namespace 使用 `pom`。
- POM 只表达 Markdown/XML 文档结构，不内置 `System`、`Context`、`Turn` 等 prompt-domain 节点。
- system prompt 与 user prompt 分别构造成两个独立 `Document`，不会先合并成一个总文档。
- system/user role 属于 prompt request assembly，不成为 `SystemNode` / `UserNode` AST variant；每个 document 内部的 section 由 Markdown heading、paragraph、list 等节点表达。
- system `Document` 即使包含 `DiffSlot` 也不执行 diff：system resolver 静默忽略 slot metadata，present value 按完整 `XmlNode` 展开，absent value 省略；不读取或更新 previous state，也不产生 warning。
- user `Document` 是通用的 Markdown/XML mixed document，不再等同于单一 context block；它可以同时包含 task、artifact、临时说明和其他任意 prompt content。
- `<agent_context>` 是 user `Document` 中一个显式 marked `DiffSlot`，不是由 context pipeline 隐式选择的 diff root。
- outermost `DiffSlot` 直接使用 prompt-facing `role` 作为 cursor identity，不增加不可见 `SlotKey`；同一 current user `Document` 中 outermost slot role 必须唯一。
- session 不保存 previous user `Document`。它只保存 `UserDocumentCursor`，其中包含各个已提交 slot 的完整 `XmlNode` baseline；普通 Markdown/XML/Text content 不进入 cursor。
- 在 provider-backed `Agent` 中，`UserDocumentCursor` 与 prompt history 使用同一个事务边界：只有 provider execution 和 `commit_turn` 都成功后才提交；失败保留旧 cursor，history replacement 则使 cursor 失效。
- AST 是规范化的生成模型，不追求 Markdown source round-trip。bullet marker、空行数量、fence 长度等词法细节不进入 AST。
- XML 与 Markdown 必须能够混排；XML subtree 内可以包含 Markdown，delta payload 中也可能出现 Markdown。
- struct instance 不直接生成 prompt string，而是先生成完整 AST。
- 每个 marked slot 的 diff 比较当前完整 `XmlNode` 与 cursor 中该 slot 上一次成功提交的完整 `XmlNode`；不直接比较 Rust struct，也不比较渲染后的字符串。
- AST 中继续保留 `DiffSlot` 概念。
- 只有 `XmlNode` 可以成为 `DiffSlot` 的 value；Markdown 和 Text 不能直接成为 diff value。
- `DiffSlot` 是通用的 ordered child-edge metadata，可以出现在 block、inline 和 mixed child sequence 中，但其 value 仍只能是 `XmlNode`。
- 并非所有 `XmlNode` 都自动参与 diff，必须显式放入 `DiffSlot`。
- 新 POM prompt path 不依赖隐式 root 来寻找 `<agent_context>`；atomic public cutover 时删除现有 implicit-root diff behavior，最终只有显式 `DiffSlot` 能建立 diff boundary。
- 第一版 Markdown 使用覆盖当前生产 prompt 的最小 CommonMark semantic subset；XML 使用 prompt-safe subset，不实现 namespace、DOCTYPE、processing instruction 或 CDATA node。
- POM 不包含 XML comment、`RawMarkdown`、`RawXml` 或其他 opaque markup node；atomic cutover 后 system/user prompt contract 只接受 structured `Document`。
- XML attribute order 不参与 semantic equality 或 diff；AST 保留 author insertion order 作为 renderer hint。
- public builders 强制 canonical text normalization；同一语义不能因空 text 或相邻 text boundaries 不同而产生不同 tree。
- 第一版迁移全部现有 diff strategies；目标边界是 differ 返回 typed `XmlPatch`，resolver 将 patch lower 到 slot-free `ResolvedDocument`，renderer 只接受 `ResolvedDocument`。2026-07-24 的实现已完成策略与 resolver 行为，但内部暂时直接传递 optional delta/full `XmlNode`，尚未抽出独立 `XmlPatch` public type。
- public child model 使用统一 `ContentNode` 和 opaque block/inline/mixed wrappers；同时提供 compositional constructors 与 closure builders，只暴露 read-only traversal。
- `AgentView` 使用 associated root type 和 `build_root` API；structured derive 的 root 静态为 `XmlNode`，不使用 runtime downcast 或 implicit document-root bridge。
- `AgentViewModel` 构造完整 system/user `Document` 并拥有 section order；framework 不再注入固定 `## View` / `## Turn Prompt` envelope。
- POM 类型实现 diagnostic `Serialize`，不实现 `Deserialize`；序列化格式不是 persistence 或 compatibility contract。
- 当前第一版已完成 POM AST、associated-root derive、Document/Markdown derive、system/user resolution、canonical renderer、全部 collection diff、user cursor/session transaction、derived streaming-tool contract 和 typed turn-artifact composition。

## 3. 已有脚手架

| 能力 | 当前文件 | 当前行为 |
|---|---|---|
| XML-shaped semantic AST | [`src/semantic_view.rs`](../../../src/semantic_view.rs) | `SemanticNode` 保存 tag、attributes、children、diff metadata 和 identity。 |
| Generic semantic diff | [`src/semantic_diff.rs`](../../../src/semantic_diff.rs) | current/previous 各自先建完整 tree，再比较 tree 并产生 delta tree。 |
| `AgentView` derive | [`agentview-derive/src/lib.rs`](../../../agentview-derive/src/lib.rs) | 根据 container mode 静态生成 `XmlNode`、`Document`、`ParagraphNode` 或 `TextNode`；XML field 可映射为 attribute/element/text/flatten/root 或 `DiffSlot`。 |
| Associated-root `AgentView` | [`src/agent_view.rs`](../../../src/agent_view.rs) | `AgentView::build_root` 把 struct instance 转成具体 POM root；`ViewField` 与 `AgentViewValue` 保留 nested field、flatten 和 diff edge。 |
| Prompt compatibility | [`src/templates.rs`](../../../src/templates.rs) | `TemplateEngine` / `PromptRenderable` / `ContextView` 只保留给 legacy callers；prelude 与 request assembly 不再依赖它们。 |
| Prompt assembly | [`src/agent.rs`](../../../src/agent.rs) | `AgentViewModel` 构造两个完整 `Document`；core 分别 resolve/render，不再注入固定 heading 或 envelope。 |
| Session baseline | [`src/agent_session.rs`](../../../src/agent_session.rs), [`src/pom_cursor.rs`](../../../src/pom_cursor.rs) | `UserDocumentCursor` 只保存 outermost slot baseline，并与 context draft 一起 clone/commit；history replacement、显式 reset/remove 使对应 baseline 失效。 |
| POM differ/resolver | [`src/pom_diff.rs`](../../../src/pom_diff.rs), [`src/pom_resolution.rs`](../../../src/pom_resolution.rs) | current/previous 完整 `XmlNode` 直接比较；user path lower full/delta/deletion 并返回 `ResolvedDocument` 与 candidate cursor。 |
| AST/diff tests | [`tests/pom_user_document.rs`](../../../tests/pom_user_document.rs), [`tests/context_preparation.rs`](../../../tests/context_preparation.rs) | 锁定 slot semantics、全部 collection modes、map identity、rollback、history replacement、fork 与 unchanged omission。 |
| Streaming tool POM contract | [`src/streaming_tool.rs`](../../../src/streaming_tool.rs) | `StreamingTool<C>: AgentView<Root = XmlNode>`；derived `<tool>` root 必须提供 `name` attribute 并用它作为 parser registration identity，其他 contract 使用 XML root name，不再读取第二个手写 tag。 |
| Turn artifact POM bridge | [`src/templates.rs`](../../../src/templates.rs), [`src/agent.rs`](../../../src/agent.rs) | `TurnArtifact::try_from_view` 从 derived XML root 取得 kind，递归拒绝 `DiffSlot` 后保存 slot-free `ResolvedDocument`；`#[view(block)]` 直接拼接其 typed block children，不经过 rendered-string DTO。 |

现有 `SemanticDiffSlot` 不是 XML output node，而是父子关系上的 AST metadata：

```rust
enum SemanticChild {
    Fragment(SemanticFragment),
    DiffSlot(SemanticDiffSlot),
}
```

full render 会透明展开 present slot；delta comparison 才读取 slot 的 field name、strategy 和 complete value。

## 4. 总体数据流

单个 Rust value 到 POM 的 derive 路径不生成 text：

```text
Rust struct instance
        |
        v
complete POM node/tree
```

单个 outermost slot 的 diff 路径是：

```text
current complete XmlNode --------+
                                  |-> XML semantic differ -> XmlPatch
previous complete XmlNode -------+
```

request 级数据流负责把 unresolved document 变成 renderer 可接受的类型：

```text
system Document ---------------------> full slot resolution ----+
                                                                  |
user Document + previous cursor -----> stateful slot resolution --+-> ResolvedDocument -> renderer -> prompt text
                                          `-> next cursor
                                               `-> role-specific successful publication boundary
```

derive/build、AST、differ、resolver、renderer 必须是五个独立职责：

1. derive/build：把 Rust view value 转成完整 AST；
2. AST：只保存语义结构和 diff 所需的 slot metadata；
3. differ：只比较两棵完整 `XmlNode` 并返回 typed `XmlPatch`；
4. resolver：解释 system/user slot policy，把 `Document` 和 optional patch lower 成 slot-free `ResolvedDocument`；
5. renderer：只遍历 `ResolvedDocument` 并生成 text。

任何 `AgentView::build_root` implementation 都不应直接拼 XML/Markdown string；任何 differ 都不应调用 renderer；任何 renderer 都不应读取 cursor 或解释 `DiffSlot`。

prompt request 层明确维护两个 document：

```text
system AgentView -> system Document -> full slot resolution -> ResolvedDocument -> renderer -> system text

user AgentView   -> current user Document ----+
                                                |
previous UserDocumentCursor -----------------> slot resolution
                                                |- ResolvedDocument -> renderer -> user text
                                                `- next UserDocumentCursor
                                                   -> provider commit or stable app snapshot
```

user `Document` 不再是 context 的同义词。`<agent_context>` 只是其中一个有状态 slot；user document 的其他内容可以按本轮需要自由组合，并作为 current content 正常渲染。

这里的 previous state 不是 previous user `Document`。slot-resolution 只从 `UserDocumentCursor` 读取有状态 XML baseline；普通 content 每轮都直接使用 current document 中的版本。

system path 的 full slot resolution 不是 diff：它不访问 slot history，不产生 delta，也不改变 session state。它递归 materialize 所有 present slots、丢弃 absent slots，并返回不含 `DiffSlot` 的 `ResolvedDocument`。

当前 provider-backed path 已完整使用两个 typed documents：

```text
system view struct
    -> AgentView::build_root -> Document
    -> resolve_system_document -> ResolvedDocument
    -> render_pom_document -> AgentTurnRequest.system

user view struct
    -> AgentView::build_root -> Document
    -> resolve_user_document(previous cursor)
       |- ResolvedDocument -> render_pom_document -> AgentTurnRequest.user
       `- candidate UserDocumentCursor
    -> provider success -> commit_turn success
    -> candidate cursor 与 PromptContext 一起原子发布
```

context preparation 若替换 history，会在 draft session 上清空 cursor 并重新 build/resolve 同一 logical turn，因此发给 provider 的 request 与 candidate cursor 总是来自同一份 AST。provider failure、`commit_turn` failure 或 cancellation 都不会推进 cursor。`AgentViewApp` 在 epoch 变化时同样丢弃 candidate cursor 并重新 capture；只有稳定 epoch 的 snapshot 才发布 cursor。

streaming tool 与 ephemeral feedback 使用同一 POM 边界：

```text
derived StreamingTool -> XmlNode
    `-> system Document 的 #[view(xml)] XML wrapper / paragraph XML island
       （wrapper 自己的 XML derive fields 可以使用 #[view(root)]）

derived artifact struct -> XmlNode
    -> TurnArtifact::try_from_view
    -> resolve_artifact_document（递归拒绝任何 DiffSlot）
    -> TurnArtifact { kind, ResolvedDocument }
    -> user Document 的 #[view(block)]
```

`StreamingToolRunner` 不自动把 tool 注入 system prompt；system document view 显式决定 contract 的位置和顺序。`TurnArtifact` 可以包含多个 block，Document derive 会按原顺序直接 splice typed children，不先渲染成 string。

普通 `Document` 没有 `PromptRenderable` implementation，因而 unresolved slot 不能绕过 resolver 直接进入 request。`render_pom_document` 的 public 输入仍然只有 slot-free `ResolvedDocument`。

## 5. 核心 AST

`Document` 是不可嵌套的根容器；`ContentNode` 是唯一的 syntax union；`ContentEdge` 表示容器到 child 的有序关系。

```rust
pub struct Document {
    children: BlockChildren,
}

pub enum ContentNode {
    Markdown(MarkdownNode),
    Xml(XmlNode),
    Text(TextNode),
}

enum ContentEdge {
    Node(ContentNode),
    Diff(DiffSlot),
}

pub struct BlockContent(ContentEdge);
pub struct InlineContent(ContentEdge);
pub struct MixedContent(ContentEdge);

pub struct BlockChildren(Vec<BlockContent>);
pub struct InlineChildren(Vec<InlineContent>);
pub struct MixedChildren(Vec<MixedContent>);

pub struct ResolvedDocument {
    children: BlockChildren,
}
```

这就是 `BlockChildren` 的完整位置和职责：它不是另一棵 AST，也不是 Markdown block enum；它是一个受约束、有顺序的 child sequence。

三种 content wrapper 在 private `ContentEdge` 上提供不同的合法性证明，sequence 只保存对应 wrapper：

- `BlockChildren`：block Markdown 或 XML；
- `InlineChildren`：Text、inline Markdown 或 XML；
- `MixedChildren`：任意 `ContentNode`；
- 三者都可以保存 value 为 `XmlNode` 的 `DiffSlot`，但不能保存 Markdown/Text diff value。

`ResolvedDocument` 只能由 system、user 或 artifact resolver 构造。其 private invariant 是整棵 tree 中不存在 `ContentEdge::Diff`；renderer 只接受该类型，不接受普通 `Document`。

所有 fields 保持 private。外部代码不能取得 `&mut Vec<_>`、不能通过 `DerefMut` 绕过 invariant，也不能直接构造带 public `children` 字段的 Markdown variant。所有 container 提供 read-only iterator/edge view，使 POM 可遍历但不可绕过 invariant 修改。

递归通过 `Vec` 间接发生，不需要为每个 child 添加 `Box`。

## 6. Markdown 节点

`MarkdownNode` 同时包含 block 和 inline constructs，但每个 variant 的 payload struct 保存正确的 child context：

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

pub struct HeadingNode {
    level: HeadingLevel,
    children: InlineChildren,
}

pub struct ParagraphNode {
    children: InlineChildren,
}

pub struct StrongNode {
    children: InlineChildren,
}

pub struct ListNode {
    kind: ListKind,
    items: Vec<ListItem>,
}

pub enum ListKind {
    Unordered,
    Ordered { start: u64 },
}

pub struct ListItem {
    children: BlockChildren,
}

pub struct CodeBlockNode {
    language: Option<StorageString>,
    body: TextNode,
}

pub struct CodeSpanNode {
    body: TextNode,
}
```

第一版固定实现当前 prompt 已经实际使用的 constructs：

- block：`Heading`、`Paragraph`、ordered/unordered `List`、`CodeBlock`、`ThematicBreak`；
- inline：`Strong`、`CodeSpan`；
- shared leaf：`TextNode`；
- XML island：`XmlNode`。

这些节点遵循 CommonMark 对应 construct 的语义，但 AST 不承诺解析或还原 CommonMark source。`ListKind::Ordered.start` 和 code-block language 是语义信息，必须保留。

`BlockQuote`、`Emphasis`、`Link`、soft/hard break、table、task list、strikethrough、footnote、image 等不进入第一版 public enum；后续增加时单独扩展。

`CodeBlock` 和 `CodeSpan` 保存原子 `TextNode`，不保存 children。代码里的 `<thought>` 只是 literal text，不能在没有显式构造的情况下变成 live `XmlNode`。

`TextNode` 本身保持 context-neutral。第一版没有 soft/hard break node，因此把含 CR/LF 的 `TextNode` 插入 Markdown `InlineChildren` 时必须拒绝；多段 prose 必须构造成多个 paragraph。相同 `TextNode` 进入 XML mixed content 或作为 code body 时可以保留换行。未来加入 break node 后，Markdown inline 换行必须使用对应 node，不能重新允许两种等价表示。

AST 不保存以下 Markdown source 细节：

- `-`、`*` 或 `+` bullet spelling；
- ATX/setext heading spelling；
- ordered-list delimiter；
- blank-line 数量；
- code fence 字符和长度；
- tight/loose list 的原始写法。

## 7. XML 节点

现有 `SemanticNode` 的 XML element 职责迁移到 `XmlNode`：

```rust
pub struct XmlNode {
    name: XmlName,
    attributes: XmlAttributes,
    children: MixedChildren,
    metadata: XmlMetadata,
}
```

POM 使用 prompt-safe XML subset：element/attribute name 必须匹配 ASCII `[A-Za-z_][A-Za-z0-9_.-]*`。第一版不支持 namespace/colon、DOCTYPE、processing instruction、CDATA node 或 XML comment；这些 source constructs 不能通过 opaque node 绕过。

`XmlNode` 是 context-neutral syntax island：

- 可以作为 block child；
- 可以作为 inline child；
- 进入 `XmlNode` 后，外层 Markdown block/inline context 不继续约束 XML children；
- XML children 可以按原顺序混合 Text、Markdown、XML 和 `DiffSlot`；
- XML 内嵌的每个 `MarkdownNode` 仍必须满足该 Markdown node 自己的 child invariant。

`XmlNode` 不保存 `is_block` / `is_inline` flag。同一个 XML node 在哪个 Markdown context 中出现，由承载它的 `BlockChildren` 或 `InlineChildren` 决定。

`XmlName` 是 validated newtype，不能继续使用任意 `String` 作为 tag/attribute name。`XmlAttributes` 拒绝重复名称并隐藏内部存储。attribute name/value 集合参与 semantic equality，insertion order 不参与 equality 或 diff；AST 仍保留 insertion order 作为 renderer hint。

`XmlMetadata` 是 crate-private typed metadata，不是任意 key/value bag。它只保存现有 semantic diff 确实需要、但不属于可见 prompt 的信息，例如 `IntrinsicCollectionKind::Map` 和 map-entry identity。它不能重新引入 `diff_boundary: bool`；boundary 只由 `DiffSlot` 表达。

### 7.1 Canonical hybrid readable XML rendering

POM 的 canonical renderer 使用 context-sensitive hybrid layout，而不是把所有
XML 一律压成单行或一律 pretty-print。结构化 block XML 应当便于模型和人直接
识别层级，同时 inline XML、mixed content 与 XML 内的 Markdown 不能因为美化
而改变语义。

renderer 遵守以下 contract：

- 只有已经完成 system/user resolution 和 semantic diff 的
  `ResolvedDocument` 才进入 formatting。formatting 只生成最终 prompt text，
  不向 POM AST 插入 whitespace `TextNode`，也不改变 semantic equality、
  `DiffSlot`、user cursor、baseline 或 diff result。
- 位于 block position、direct children 为 XML elements 的 element-only
  container 使用 readable layout：opening tag、每个 direct child 和 closing
  tag 分行，嵌套层级固定使用 2 spaces。text leaf 保持
  `<phase>execute</phase>` 这样的单行形式，empty element 保持 `<none />`。
- 位于 `Heading`、`Paragraph` 或 `Strong` 等 inline position 的 XML island
  不应用 readable XML indentation。没有 block Markdown 时整棵保持 compact；
  合法 subtree 自身含 block Markdown 时保留既有 block flow。renderer 不能
  为了展示 XML 层级而额外插入会终止或重解释 Markdown inline flow 的换行。
- 只要 XML node 的 direct children 构成 mixed/phrasing content，例如
  `Text -> XmlNode -> Text`、`Strong` 或 `CodeSpan` 与 XML 相邻，renderer
  就保持其 authored order 和连续 flow，不在 siblings 之间插入 structural
  whitespace。
- 含 block Markdown 的 XML subtree 使用保守 layout，不对其中的 Markdown
  lines 施加 XML indentation，也不做 reflow。特别是不能让 4-space
  indentation 把 paragraph/list/code fence 意外解释成 Markdown code block。
- attribute 始终保留在所属 opening tag 的同一行；layout 新增的换行统一使用
  LF (`\n`)，formatter 不会合成 trailing spaces。attribute escaping 和
  insertion order、text/code body 的 authored whitespace 继续由既有
  renderer contract 保证。

例如，block-position 的 element-only delta tree canonicalize 为：

```xml
<agent_context rendering_mode="delta">
  <focus rendering_mode="delta">
    <summary>Inspect the owner edge.</summary>
  </focus>
  <transient_hint rendering_mode="delta">
    <none />
  </transient_hint>
</agent_context>
```

而 paragraph 中的 executable tool call 仍保持 inline：

```markdown
Call <inspect_edge edge_id="edge.owner" /> before updating the graph.
```

这个 layout distinction 属于 resolved renderer 的 textual contract，不属于
`XmlNode` 的 semantic state。调用者不能通过比较 rendered strings 代替 POM
semantic diff。

## 8. Text 节点

```rust
pub struct TextNode {
    value: StorageString,
}
```

`TextNode` 保存未转义的语义文本：

- 在 Markdown context 中，由 Markdown renderer 决定 escaping；
- 在 XML context 中，由 XML renderer 决定 escaping；
- 构造 AST 时不能因为文本长得像 `<tag>` 就自动升级成 XML；
- structural whitespace 由 renderer 产生，`TextNode` 只保存作者有意提供的内容；
- `CodeBlock` / `CodeSpan` 内的 text 保持 literal，不递归解析。

所有 public builders 必须执行 canonical text normalization：sequence 中的空 `TextNode` 被删除，相邻 `TextNode` 精确拼接。normalization 不能 trim 内容、不能跨越 XML/Markdown/`DiffSlot` edge 合并，也不能改变 XML mixed-content 或 code body 中有意义的空白。atomic code body 即使为空也仍属于其 code node。

## 9. Child Context 与合法嵌套

POM 采用 immediate-child validation，而不是对整棵 subtree 强行套一个 Markdown mode：

| Parent | Child context | 允许的 direct children |
|---|---|---|
| `Document` | block | block Markdown、XML、XML `DiffSlot` |
| `ListItem` | block | block Markdown、XML、XML `DiffSlot` |
| `Heading` | inline | Text、inline Markdown、XML、XML `DiffSlot` |
| `Paragraph` | inline | Text、inline Markdown、XML、XML `DiffSlot` |
| `Strong` | inline | Text、inline Markdown、XML、XML `DiffSlot` |
| `XmlNode` | mixed | 任意 `ContentNode`、XML `DiffSlot` |
| `CodeBlock` / `CodeSpan` | atomic | 只有 literal `TextNode`，没有递归 children |

因此：

- `Document -> Text` 非法，应显式构造 `Paragraph(Text)`；
- `Paragraph -> Heading` 非法；
- `Strong -> Paragraph` 非法；
- `Paragraph -> XmlNode -> List` 合法，因为 XML 开启新的 syntax island；
- `ListItem -> Paragraph -> Text` 合法。

新增 `MarkdownNode` variant 时必须通过 exhaustive match 指定它是 block 还是 inline；不允许 wildcard 让新 variant 静默进入错误 context。

## 10. DiffSlot 在 AST 中的位置

`DiffSlot` 是 ordered child-edge metadata，不是第四种 syntax node，也不是 `XmlNode` 上的 boolean：

```rust
pub struct DiffSlot {
    role: XmlName,
    strategy: DiffStrategy,
    value: Option<XmlNode>,
}

pub enum DiffStrategy {
    Recursive,
    Replace,
    Append,
    Sequence,
    Set,
    Keyed(XmlName),
}
```

核心 invariant：

- `value` 的类型直接限定为 `Option<XmlNode>`，不能用 `Option<ContentNode>` 再靠 runtime validation 判断；
- `DiffSlot::present(strategy, value)` 从 `value.name` 派生 `role`，不接受第二份 role 参数；
- `DiffSlot::absent(role, strategy)` 在没有 value 可供派生时显式接收 role；
- 如果未来增加同时携带 role/value 的 dynamic import DTO，转换必须拒绝二者不一致，不能静默 retag；当前 typed API 不提供这种双重输入；
- absent slot 必须保留 `role` 和 `strategy`，不能在 AST normalization 时被过滤，否则无法表达 deletion；
- full resolution 对 slot 透明：present 递归 materialize，absent 省略；renderer 不接收 unresolved slot；
- Markdown/Text 如需独立更新，必须先包在一个 addressable `XmlNode` 中；
- 普通 `XmlNode` 和 marked `XmlNode` 必须通过不同 builder 方法显式区分，不能自动把所有 XML 变成 boundary。

同一个 slot 在两个 document role 下有不同的处理策略：

- system `Document`：忽略 `strategy` 和 previous state；present value 完整 materialize，absent value 省略，返回 `ResolvedDocument`；
- user `Document`：由 slot-resolution 阶段读取 previous state，消费 `XmlPatch` 并返回 `ResolvedDocument` 与 next cursor。

request assembly 决定一份 `Document` 以 system 还是 user role 发送，对应 resolver 再执行该 role 的 slot policy；role 和 policy 都不进入 `DiffSlot` 或 `Document` 类型。POM 仍然只有一种 `Document` 和一种 `DiffSlot`。

已确认将 `ContentEdge::Diff` 放进 block、inline、mixed 三种 child sequence，因为 `<agent_context>` 本身可能是 Markdown document 的 direct block child；这个设计仍然保证 slot value 只能是 XML。

相较于旧的 XML-only semantic AST，mixed POM 把 slot 泛化为“任何可放置 XML 的 child edge”；它仍然不是 `ContentNode`，也不会让 Markdown/Text 本身变成 diff value。legacy `SemanticChild::DiffSlot` 仍只会出现在 `SemanticNode.children`。

### UserDocumentCursor

user document 的 previous state 使用独立 cursor，不保存整份 previous document：

```rust
pub struct UserDocumentCursor {
    slots: BTreeMap<XmlName, SlotBaseline>,
}

struct SlotBaseline {
    strategy: DiffStrategy,
    value: XmlNode,
}
```

`DiffSlot.role` 就是 outermost slot 的稳定 identity，也是 cursor map 的 key。present slot 的 role 来自 element name，因此 prompt-facing element 与 state lineage 不会分叉。cursor 中的 `value` 始终是上一次成功提交的完整 `XmlNode`，不是 `XmlPatch`、resolved node 或 rendered text。

同一 current user `Document` 中的 outermost slot roles 必须唯一；重复 role 是 resolution error。nested slots 不进入 cursor，因此只需要在其所属 XML parent 内按 semantic tree structure 对齐，不受 document-level role uniqueness 约束。

slot-resolution 接收 current user `Document` 和 previous `UserDocumentCursor`，返回两个值：本轮要渲染的 `ResolvedDocument`，以及仅供成功提交使用的 next cursor。first turn 使用空 cursor；next cursor 从 previous cursor clone 开始，再应用本轮明确出现的 slots。遍历规则是：

- ordinary edge 完整保留本轮内容，不读取也不写入 cursor；
- traversal 从 `Document` 向下寻找 outermost slots；一旦遇到 slot，就把它的整个 `XmlNode` value 当作一个 cursor boundary，不再把该 value 内的 nested slots 独立登记进 cursor；
- cursor boundary 内的 nested slots 继续作为完整 `XmlNode` 的 semantic metadata，由 XML differ 在递归比较 current/previous value 时消费；
- resolution 在产生 output 前验证所有 outermost roles 唯一；同一 role 不能在本轮不同位置重复引用同一 baseline；
- present slot 没有 baseline 时递归 materialize 完整 current value，并把 unresolved、完整 current `XmlNode` 写入 next cursor；
- present slot 有 baseline 时执行 XML semantic diff；`Unchanged` 时省略，其他 `XmlPatch` 由 resolver lower 并递归 materialize 后放入 `ResolvedDocument`，next cursor 始终保存 unresolved、完整 current `XmlNode`；
- present slot 的 `strategy` 与 baseline 不同时不复用旧 diff strategy，保守地完整展开 current value，并用 current strategy/value 替换 baseline；
- explicit absent slot 有 baseline 时产生 deletion patch，并从 next cursor 删除该 baseline；没有 baseline 时直接省略；
- 本轮 document 完全没有出现某个 slot，不等于删除该 slot，也不清除其 baseline；需要删除时必须构造 explicit absent slot。

修改 `role` 会开启新的 state lineage：新 role 走 first-seen full path，旧 role baseline 按“slot 未出现”规则继续保留。若这是一次正式 rename，本轮应同时提供旧 role 的 explicit absent slot，或者显式清空整个 cursor。

slot 在 user document 中移动位置但保持相同 role/value，不构成 state change，也不会 full resend。cursor 不保存 placement fingerprint 或 TTL；长期未出现的 baseline 继续保留，直到 explicit absent、`UserDocumentCursor::remove(role)`、`clear()` 或 history replacement 清理。

next cursor 不能在 render 或 provider execution 开始时直接写回 session。它与当前 `AgentSession` 的 draft/commit 规则一致：

- provider execution 成功且 `commit_turn` 成功：原子提交 prompt context 与 next cursor；
- provider execution 或 `commit_turn` 失败：丢弃 draft 和 next cursor，保留 previous cursor；
- context preparation 替换 history：清空 draft cursor，使下一次 resolution 对所有 present slots 走 full path；
- fork session：prompt context 与 cursor 一起 clone；
- system document expansion：完全不接收 cursor，也不产生 next cursor。

`AgentViewApp` 没有 provider/`commit_turn` 阶段。它会先验证 resolved document 可以由 canonical renderer 接受，只在 view epoch 仍稳定时返回 snapshot 并发布 candidate cursor。`act_with_sink` 在应用外部副作用前消费当前 `turn_id`，因此后续 snapshot 构建失败也不能用旧 id 重放动作。snapshot 返回后的 transport delivery/ack 仍由 daemon adapter 负责，不是 cursor API 的隐式事务。

因此 `pom_diff` 的底层比较单位仍然是两棵完整 XML semantic trees；只是 session persistence 已从“整个 previous view/document”收窄到“每个 stateful slot 的完整 XML baseline”。

### XmlPatch

`XmlPatch` 是 differ 与 resolver 之间的 typed semantic IR，不是 `ContentNode`，也不能直接交给 renderer。它区分 unchanged、replacement、node delta 和 collection operations；payload 可以包含带 Markdown children 的完整 `XmlNode`。`rendering_mode="delta"`、`<insert>`、`<remove>`、`<update>`、`<replace>`、`<none>` 等最终 textual vocabulary 不进入 patch type contract，由 lowering/renderer 设计决定。

nested slots 按其 XML parent 内的 structural position 对齐。unmarked shape change、key 缺失/重复或 collection strategy 无法安全应用时，differ 返回 conservative full replacement，不返回 partial guess，也不直接报 rendering error。

这是目标边界，尚不是 2026-07-24 的代码形状。当前 `pom_diff` 已实现相同 comparison/fallback 语义，但返回 `Option<XmlNode>`：`None` 表示 unchanged，`Some` 已是 full 或带 operation wrapper 的 delta tree；`pom_resolution` 再递归 materialize nested slots。后续抽出 `pom_patch.rs` 时应保持现有 resolver、renderer 与 output tests 不变。

## 11. 构造 API

公开构造路径应尽量让非法 Markdown placement 无法表达，而不是允许任意 `Vec<ContentNode>` 后再依赖 renderer 猜测。

POM 同时提供 context-specific closure builders 和 compositional constructors；两者复用同一组 private constructors：

```rust
let document = Document::build(|blocks| {
    blocks.heading(2, |inline| {
        inline.text("Known relationships");
    });

    blocks.paragraph(|inline| {
        inline.text("Alice is ");
        inline.strong(|strong| strong.text("suspicious"));
        inline.text(" of ");
        inline.xml(person_ref);
        inline.text(".");
    });

    blocks.xml_slot(
        DiffSlot::present(DiffStrategy::Recursive, agent_context),
    );
});
```

builders 的能力边界：

- `BlockBuilder`：`heading`、`paragraph`、`list`、`code_block`、`thematic_break`、`xml`、`xml_slot`；
- `InlineBuilder`：`text`、`strong`、`code_span`、`xml`、`xml_slot`；
- `MixedBuilder`：接受任意 `ContentNode`，并提供对应 convenience methods 与 `xml_slot`；
- `ListBuilder::item`：进入新的 `BlockBuilder`；不自动把 text 猜成 paragraph。

compositional constructor 负责底层组合，closure builder 是其 convenience façade：

```rust
let title = BlockContent::heading(
    HeadingLevel::H2,
    [InlineContent::text("Known relationships")],
);
```

closure builder 与 compositional constructor 必须复用同一套 private node constructors，不能各自维护一套 validation。

typed constructors 接收已经 validated 的 `XmlName`、`HeadingLevel` 等值，因此常规路径 infallible。接受 raw `str`、整数或 dynamic `ContentNode` 的入口使用 `try_*` / `TryFrom`，不能 panic。

dynamic/import 路径先使用 `TryFrom` 取得正确的 context wrapper，再调用 typed builder：

```rust
let block = BlockContent::try_from_node(node)?;
block_builder.push(block);

let inline = InlineContent::try_from_node(other_node)?;
inline_builder.push(inline);
```

`from_parts_unchecked` 只允许 `pub(crate)`，且不使用 Rust `unsafe`：这里违反的是 AST invariant，不是 memory-safety contract。

所有 container 公开 `iter()` 或 typed read-only edge view；不公开 raw `ContentEdge` mutation。这样 traversal、diagnostics 和 `Serialize` 可以观察完整结构，而 authoring 只能走保持 invariant 的 API。

不要提供：

- public `children_mut()`；
- `DerefMut<Vec<_>>`；
- `From<ContentNode>` 到 block/inline wrapper 的无条件转换；
- `From<&str> for BlockContent` 自动猜 paragraph；
- generic `diff(ContentNode)`；
- 根据 text 内容自动识别 Markdown/XML。

## 12. AgentView 与 derive 的迁移方向

`AgentView` 负责 Rust value -> complete AST，并用 associated `Root` 保留具体 root 类型：

```rust
pub trait AgentView {
    type Root;

    fn build_root(&self) -> Result<Self::Root, PomError>;
}
```

`Root` 不带 `Into<ContentNode>` bound，因为不可嵌套的 `Document` 也是合法 root。需要 XML 的边界使用 equality bound，例如 `AgentView<Root = XmlNode>`。因此 `DiffSlot::present(strategy, view.build_root()?)` 与 `TurnArtifact::try_from_view` 在 compile time 已经知道 value 是 XML，而不是从 `ContentNode` runtime downcast。

活跃的 POM request path 不再使用 implicit `AgentViewRoot -> PromptRenderable/ContextView` document bridge。`Document` 必须由 `#[agent_view(document)]` view 显式构造；scalar/display root 不会自动升级成 document block。旧 blanket bridge 暂时仍留在 `templates` compatibility module 中。

当前 implementation 不存在独立 `DocumentProducer` trait。system prompt struct 本身使用 `#[derive(AgentView)] #[agent_view(document)]`，`build_root` 直接返回 unresolved `Document`。streaming demo 与 chess 的 system prompt 都走这条路径。

`ViewField` 是 derive 的 nested-value adapter，不是 AST syntax node：

```rust
pub enum ViewField {
    Empty,
    Attribute(XmlAttribute),
    Content(MixedContent),
    Children(MixedChildren),
}
```

`MixedContent` 可以保留普通 node 或 `DiffSlot` edge，因此 `ViewField` 不会像 `Vec<ContentNode>` 一样在 flatten/build_field 边界丢失 diff metadata。derive 中的 field name 在 macro expansion 时验证，并以 `XmlName` 进入 build API；runtime 不再把任意 `&'static str` 当作合法 XML name。

当前 derive container mode 与 root type 是：

- `#[agent_view(kind = "...")]` -> `XmlNode`；
- `#[agent_view(display)]` -> `TextNode`；
- `#[agent_view(document)]` -> `Document`；
- `#[agent_view(markdown = "paragraph")]` -> `ParagraphNode`。

XML derive 保持原 field 语义并增加 mixed-POM authoring：

- structured struct -> `XmlNode`；
- default scalar field -> XML attribute；
- `#[view(element)]` -> child `XmlNode`；
- `#[view(text)]` -> `TextNode`；
- `#[view(flatten)]` -> children；
- `#[view(root)]` -> 插入 child 自己的 derived `XmlNode` root，不增加 field-role wrapper；
- `#[view(code_span)]` -> field-role XML element 内的 Markdown `CodeSpanNode`；
- `#[view(diff)]` 及带 strategy 的 diff attributes -> `DiffSlot`，并把 value 规范化为 addressable `XmlNode`。
- `#[view(comment)]` 已是 compile error；需要表达 prompt-visible note 的调用点迁移为 Markdown 或显式 XML element。

Document fields 使用：

- `heading = N`、`paragraph`、`ordered_list`、`unordered_list` 和 `xml`；
- `block`：拼接一个 block-shaped root、`Option`、另一个 `Document`，或直接声明的 `Vec<T>` 中每个 item 的 block children；
- `diff`：把 XML-root value 放入 outermost `DiffSlot`；只有这个 mode 可同时使用 `name = "..."` 来固定 cursor role。

列表 item 必须是 `AgentView<Root = ParagraphNode>`。paragraph fields 使用 `text`、`code_span` 与 `xml`，因此 Markdown 与 XML 仍由 typed node 组合，不接收 raw markup string。`#[view(block)] Vec<T>` 当前由 derive 对直接写出的 `Vec` 类型展开；type alias、slice 与 array 不承诺相同 special handling。

Document diff field 必须是 `AgentView<Root = XmlNode> + AgentViewValue`，原生 `XmlNode` 也受支持。`Option<T>` 的 `None` 生成 explicit absent slot。`Vec<T>` 必须选择 append/seq/set/key strategy；collection strategy 用在非 `Vec`、脱离 `diff` 使用，或与 `replace` 同时使用都会在 derive 阶段报错。

POM 不需要第二个 `XmlAgentView` marker trait。associated `Root` 已经表达 concrete AST root；是否参与 diff 仍由 user `Document` 内显式 `DiffSlot<XmlNode>` 决定。

`AgentViewCollect<Source>` 只负责 domain/source -> prompt-facing struct，不涉及 AST 形状，保持不变。

### AgentViewModel document ownership

新 prompt path 删除当前 `PromptRenderable` associated prompt types 和 framework-owned `compose_user_message` 固定 envelope。`AgentViewModel` 构造完整 documents：

下面是当前 `AgentViewModel` 的 document ownership 签名；`commit_turn` 等现有生命周期方法为聚焦而省略：

```rust
async fn build_system_document(
    &self,
    ctx: &PromptContext<I, Self::ContextState>,
    source: &Self::Source,
) -> anyhow::Result<Document>;

async fn build_user_document(
    &self,
    ctx: &PromptContext<I, Self::ContextState>,
    call_id: &str,
    task: StorageString,
    current_view: &Self::View,
) -> anyhow::Result<Document>;
```

`build_user_document` 决定 heading、context slot、task、artifact、临时说明和其他 content 的顺序。framework 可以提供 section/context helper，但不能自动注入 `## View`、`## Turn Prompt` 或固定三段结构。

app context view 的 structured derive root 是 `XmlNode`；user document 通常直接声明 `#[view(name = "agent_context", diff)] context: AppContextView`，由 derive 建立 slot，不需要业务代码手写 `DiffSlot::present`。一个 user document 也可以没有 context slot，或拥有多个 role 唯一的 outermost slots。

system document 每次按当前 derived view 结果构造并走 full resolution；POM 不强制 custom view model 的 system 永远静态。复用已经成功提交的 stable rendered system text 可以作为后续 adapter 优化，但不是当前 `Agent` 行为。

## 13. 模块边界

不要继续让一个 `semantic_view.rs` 同时拥有 AST、derive support、diff entry 和 XML serializer。目标模块拆分为：

```text
src/
  agent_view.rs
  pom/
    mod.rs
    document.rs
    content.rs
    markdown.rs
    xml.rs
    text.rs
    children.rs
    diff_slot.rs
    resolved.rs
  pom_cursor.rs
  pom_patch.rs
  pom_diff.rs
  pom_resolution.rs
  pom_renderer.rs
```

职责：

- `agent_view.rs`：associated-root `AgentView`、`ViewField`、derive 支撑 API；
- `pom/*`：`Document`、`ResolvedDocument` 和所有 node/child types，不依赖 renderer；
- `pom_cursor.rs`：`UserDocumentCursor` 和 private `SlotBaseline`；
- `pom_patch.rs`：typed `XmlPatch` semantic IR；
- `pom_diff.rs`：current/previous complete `XmlNode` -> `XmlPatch`；
- `pom_resolution.rs`：按 system/user policy 解释 slots、lower patch；user path 返回 `ResolvedDocument` 与 next cursor；
- `pom_renderer.rs`：`ResolvedDocument` -> `String`。

当前 implementation 已新增 `agent_view.rs`、`pom/*`、`pom_cursor.rs`、`pom_diff.rs`、`pom_resolution.rs` 和 `pom_renderer.rs`。`ResolvedDocument` 目前与 `Document` 一起定义在 `pom/document.rs`；`XmlPatch` 尚未按目标目录草图拆出，`pom_diff.rs` 暂时直接返回 optional delta/full `XmlNode`。resolver 已实现 system materialize、artifact reject 与 stateful user resolution；renderer 只接收 `ResolvedDocument`。

旧 `semantic_view.rs` / `semantic_diff.rs` 和 `templates.rs` 内的 Minijinja/`PromptRenderable` API 仍保留为 compatibility surface。它们不再参与 `Agent`、`DefaultAgentViewModel`、`AgentViewApp`、demo、chess 或 hello 的 request path。`ContextView`、`PromptRenderable` 和 `TemplateEngine` 需要显式 module import；`LegacyAgentView`、`AgentViewRoot`、`Semantic*` 与 legacy render helpers 暂时仍由 prelude 导出。

## 14. 错误与规范化

普通 typed builder 路径应尽量 infallible；当前 dynamic/import 路径的 AST 构造错误是：

```rust
pub enum PomError {
    InvalidXmlName { value: StorageString },
    DuplicateXmlAttribute { name: XmlName },
    InvalidHeadingLevel { value: u8 },
    InvalidInlineNewline { value: StorageString },
    WrongContentContext {
        expected: ContentContext,
        actual: ContentKind,
    },
}
```

`DiffSlot` value/role 的不合法组合由 typed constructors 避免；重复 user slot role 在 resolution 阶段返回 `PomResolutionError::DuplicateUserDiffSlotRole`。renderer 对空 paragraph/list、非法 code span 和 XML 字符另行返回 `PomRenderError`。错误必须携带足以定位 offending value/context 的结构化信息；具体诊断文案不进入 POM compatibility contract。

规范化规则集中在 private constructors/builders：

- `HeadingLevel` 限定为 1..=6；
- XML element/attribute name 必须通过 `XmlName` validation；
- attribute name 唯一；
- sequence 中的空 `TextNode` 必须省略；
- 相邻 text 必须在不跨 edge 的情况下合并；
- absent `DiffSlot` 永远不能因 value 为 `None` 被删除；
- 不 trim prose；
- 不解析 raw Markdown/XML source；
- 不保存 self-closing、CDATA、entity spelling 等 renderer/source 细节。

POM public value types、`ResolvedDocument` 和 `UserDocumentCursor` 实现 `Serialize`，用于 diagnostics、snapshot debugging 和工具观察。目标 `XmlPatch` 类型落地后遵循同一规则。serialization 必须能够观察完整 ordered structure 与 typed metadata，但其 wire shape 不承诺稳定，也不能作为 persistence contract。

第一版不实现 `Deserialize`。直接 derive `Deserialize` 会绕过 private constructor invariant；未来若出现持久化需求，必须先 deserialize 到 versioned raw DTO，再 `TryFrom` 成 validated model，不能给当前 public types 直接补无验证反序列化。

legacy host DTO 可以保留自己的 `Deserialize` compatibility，但必须对 typed POM artifact field 使用 `skip_deserializing`。当前 `DefaultContextState` 会恢复 task 等普通状态，并把 ephemeral artifacts 还原为空集合；它不会借宿主 DTO 绕过 POM constructor。

## 15. 测试策略

所有实现 phase 强制使用 TDD，不允许先写 production code 再补 tests。每个独立行为遵循：

1. **RED**：先写一个只描述该行为的最小测试；
2. 运行精确 test target，确认它因为缺少目标行为而失败，而不是 compile typo、fixture error 或错误 assertion；
3. **GREEN**：只写让该测试通过的最小实现；
4. 重新运行精确 target 和受影响 suite，确认全部通过且没有 warning/error；
5. **REFACTOR**：只在 green 后整理结构，并再次保持全绿；
6. 再进入下一个行为。

implementation plan 必须把每个 task 的 RED command、expected failure、GREEN command 和 broader regression command 写出来。若测试在实现前已经通过，必须修正测试直到观察到正确失败；不能把先写好的 production code 留作“参考”。

测试分为 AST constructors、patch/diff、resolution、renderer、session transaction 五层。Phase 2 只实现并运行 AST 层，不用尚未存在的 renderer string snapshot 代替结构断言。

### 类型与结构

- `Document` 只能接收 block content；
- `BlockContent` / `InlineContent` / `MixedContent` 在 compile time 限制合法 placement；
- paragraph/heading 只能接收 inline content；
- `XmlNode` 同时可放在 block 和 inline context；
- XML mixed children 保持 Text -> Markdown -> XML -> Text 的原始顺序；
- code block/span body 是 literal text；
- ordered list 保留 start，code block 保留 optional language；
- 第一版 public enum 不包含 BlockQuote/Emphasis/Link/break/GFM extension；
- list item 使用 block context；
- invalid XML name、namespace colon、duplicate attribute、invalid heading level 和 Markdown inline newline 被拒绝；
- empty text 被删除、相邻 text 被合并且不会跨 edge；
- attribute reorder 不改变 semantic equality；
- XML comment 与 opaque Markdown/XML 没有 public construction path；
- read-only traversal 能观察所有 nodes/edges，但不能取得 raw mutable child vector；
- POM/cursor/patch/resolved values 可以 `Serialize`，但不能 `Deserialize`。

### DiffSlot invariant

- present slot 只能接收 `XmlNode`；
- present slot 从 value name 派生 role，API 不接受重复 role 参数；
- absent slot 显式接收 role；
- Markdown/Text 没有转换成 `DiffSlot` 的 API；
- absent slot 保留 role/strategy 和 sequence position；
- Recursive/Replace/Append/Sequence/Set/Keyed 全部有结构测试；
- ordinary XML 与 marked XML 是两个显式构造路径；
- Markdown subtree 包进 XML 后可以成为 slot value；
- text merge 不能跨过 slot；
- system document 中的 present slot 完整展开且不访问 diff state；
- system document 中的 absent slot 省略且不产生 warning；
- system slot expansion 不更新 session state。

### XmlPatch 与 resolution

- equal XML roots 产生 `XmlPatch::Unchanged`；
- unmarked shape change 和不安全 collection comparison 产生 typed full replacement；
- nested slots 按 structural position 递归产生 typed delta；
- patch payload 可以保留 Markdown children；
- system resolution 递归 materialize slots，并返回 slot-free `ResolvedDocument`；
- user resolution lower diff result，renderer-facing tree 中不残留任何 `DiffSlot`；
- 普通 `Document` 不能直接传给 renderer；
- renderer tests 只接收 `ResolvedDocument`。

### UserDocumentCursor

- 普通 user document content 每轮完整渲染且不进入 cursor；
- first-seen present slot 完整展开；
- unchanged slot 从 resolved document 省略；
- changed slot 输出 semantic delta 或 conservative full replacement，并在 next cursor 中保存完整 current value；
- strategy change 触发 full replacement 并更新 baseline；
- explicit absent slot 输出 deletion 并删除 baseline；
- document 未出现某个 slot 时保留已有 baseline；
- 同 role slot relocation 不推进或重置 baseline；
- outermost slot 的完整 baseline 包含 nested slot metadata；nested slots 不创建独立 session baseline；
- duplicate outermost role 在 render 和 cursor mutation 前返回 error；
- role rename 将新 role 视为 first-seen slot，旧 role 必须 explicit absent 或通过 cursor reset 清理；
- provider failure 和 `commit_turn` failure 都不能推进 cursor；
- history replacement 清空 cursor；
- `remove(role)` 与 `clear()` 显式清理 cursor；
- session fork 同时 clone prompt context 与 cursor。

### Compile-fail

使用 trybuild 锁定：

- 不能把 `TextNode` 传给 `DiffSlot::present`；
- 不能给 `DiffSlot::present` 额外传 role；
- 不能直接构造 private Markdown payload fields；
- 不能把 `Heading` 放进 `InlineChildren`；
- 不能取得 raw mutable child vector；
- 不能把 `Document`、`ResolvedDocument` 或 `UserDocumentCursor` 作为 `serde::Deserialize` target；未来 `XmlPatch` 落地后遵守同一约束；
- 不能通过 public fields 给 `TurnArtifact` 注入 raw `PromptRenderable` / string payload；
- `#[view(comment)]` 当前已是 compile error；
- Document `#[view(diff)]` 不能接收 Text、Markdown 或另一个 Document root；
- Document diff 不能与 `block` / `xml` 等 block mode 组合；
- Document `Vec` diff 缺少 collection strategy、非 `Vec` 使用 collection strategy、strategy 脱离 `diff` 或 `replace + collection` 都是 compile error。

derive/diff/renderer 迁移分别增加：

- struct -> expected complete AST；
- current/previous `XmlNode` -> expected diff result（当前断言 optional delta/full `XmlNode`；抽出 patch type 后改为 typed `XmlPatch`）；
- current `Document` + cursor -> expected `ResolvedDocument` + next cursor；
- `ResolvedDocument` -> expected prompt text。

这些测试必须分开，避免一个 string snapshot 同时掩盖 derive、differ、resolver 和 renderer 的错误。session transaction suite 还必须覆盖 history replacement full resend、provider/commit/cancellation rollback、fork、并发 turn serialization 和 observer publication。

## 16. 分阶段迁移

### Phase 1：Characterize current behavior

- 保留现有 full/delta output tests；
- 补充本文开头的 Core Pipeline 文档；
- 冻结 `DiffSlot` full expansion、empty omission 和 nested recursion 行为。

### Phase 2：Additive POM

- 新增 `pom/*`；
- 按最小生产 subset 实现 `Document`、`ContentNode`、Markdown/XML/Text nodes；
- 实现 prompt-safe XML validation、typed child wrappers、compositional constructors、closure builders 和 read-only traversal；
- 实现完整 `DiffStrategy` enum 与 `DiffSlot<Option<XmlNode>>`，present role 从 value name 派生；
- 实现 mandatory text normalization、attribute semantic equality 和 diagnostic `Serialize`；
- 不加入 comment/raw node，不实现 `Deserialize`；
- 每个 constructor/invariant 严格走 RED -> GREEN -> REFACTOR，完成 AST-only tests；
- runtime 仍使用旧 `semantic_view.rs`。

### Phase 3：AgentView derive migration

- 新增 associated-root `build_root` API 和 edge-preserving `ViewField`；
- 把现有 built-in scalar、Option、Vec、BTreeMap 和 derive expansion 切到新 AST；
- structured derive 静态返回 `XmlNode`；display derive 返回 `TextNode`；
- 新 POM path 保持现有 XML element/attribute/text/flatten behavior，并增加 `root`、`code_span` 与真实 `DiffSlot` edge；
- `#[view(comment)]` 在 POM derive 中删除；
- system prompt 所需的最小 `Document` / Markdown paragraph/list derive 与本阶段一起完成，避免保留另一套手写 document 构造 trait。

本阶段已完成。普通 XML/display derive 暂时仍同时生成 legacy semantic implementation，供独立 compatibility API 与 characterization tests 使用；`Agent` / `AgentViewApp` 的 context/user path 已不再依赖它。Document/Markdown 与 POM-only XML modes 不生成 legacy implementation。

### Phase 4：Differ migration

本阶段的运行时行为已落地：

- XML differ 改成显式接收 current/previous 完整 `XmlNode`；
- 完整迁移 recursive/replace/append/sequence/set/keyed/map behavior；
- 新增 `ResolvedDocument`、system full resolution 和 user cursor resolution；
- user resolution 遍历 current document，并通过 `UserDocumentCursor` 查找每个 outermost slot 的 previous baseline；
- tests 先断言 resolved AST，不调用 renderer；
- 所有 collection strategies 完成 old/new semantic parity，并在 chess 实际增量路径验证。

与批准设计的一个明确差异是：独立 typed `XmlPatch` 尚未抽出；当前 differ 直接产生 optional delta/full `XmlNode`。这是后续内部重构，不影响 document、cursor 或 renderer public boundary。

### Phase 5：Renderer 与 prompt integration

本阶段已完成：

- canonical Markdown/XML renderer；
- renderer 只接受 slot-free `ResolvedDocument`；
- `AgentViewModel` 改为构造完整 system `Document` 与 user `Document`；
- 允许 user `Document` 组合 context、task、artifact、临时说明和其他 Markdown/XML content；
- 删除 framework-owned `## View` / `## Turn Prompt` 固定拼接；
- 让 `<agent_context>` 的 marked slot 接入 `UserDocumentCursor`；
- 复用现有 session draft/atomic commit/history replacement 生命周期提交或清空 cursor；
- 将 template variables 和 artifacts 迁成 typed POM content；raw string/template output 不能进入新 structured path；
- 分开验证 POM -> text snapshots 与 session transaction behavior。

demo、chess、hello 与 CLI 示例均已使用这一 contract。streaming tool 直接实现 `AgentView<Root = XmlNode>`；tool/parser feedback 通过 typed `TurnArtifact` 进入 user Document。Chess 的 task、context 与 diff 也已离开 legacy path。

### Phase 6：Atomic public cutover

- derive、request traits、prelude 推荐项、tests 与 examples 已完成 POM-first cutover；
- 删除旧 `SemanticNode` / `SemanticFragment` / `SemanticField`；
- 删除旧组合式 XML render helpers；
- 删除 legacy `SemanticPatch` / semantic diff implementation；
- 删除旧 implicit `AgentViewRoot -> PromptRenderable/ContextView` bridge；
- `RenderedTurnArtifact`、`PromptLayout` 与旧 system/user vars 已删除；`String: PromptRenderable`、Minijinja engine 与 implicit legacy view bridge 暂时仅在 module-scoped compatibility API 中保留；
- 删除 `#[view(comment)]` 及对应 legacy comment fragment；
- 删除 implicit-root diff entry 和 synthetic-root compatibility；迁移完成后的所有 stateful boundaries 都必须在 `Document` 中显式构造；
- 一次性更新 TDD/characterization suites 后删除 legacy implementation，不长期维护两套 AST alias。

因此 Phase 6 仍未完全结束：上面保留的 `SemanticNode`/legacy render API 是当前剩余清理项，不是新 prompt path 的依赖。

## 17. 本阶段明确不做

- 除第 7.1 节的 canonical hybrid XML layout contract 外，不在本文中冻结 Markdown/XML renderer 的全部 lexical 细节；escaping 和错误边界继续由 renderer tests 锁定；
- 不决定 delta prompt 的最终 textual vocabulary；
- 不改变现有 session draft、atomic commit 和 fork 生命周期；cursor 的持久化内容收窄为 slot baselines；
- 不在本文中设计 history compaction 除“history replacement 清空 cursor”以外的策略；
- 不实现 Markdown parser 或 XML parser；
- 不追求 CommonMark source round-trip；
- 不实现完整 CommonMark/GFM 或 XML 1.0 document model；
- 不实现 XML comment、namespace、DOCTYPE、processing instruction 或 CDATA node；
- 不引入 `SystemNode`、`ContextNode`、`TurnNode`；
- 不加入 ChatML/provider message protocol；
- 不加入通用 `RawMarkdown` / `RawXml` escape hatch；
- 不实现 `Deserialize` 或稳定 persistence wire format；
- 不扩展第一版 prompt 所需范围之外的 Markdown derive subset；当前只支持 paragraph root、heading、paragraph、ordered/unordered list、text、code span、typed XML island 与 block composition；
- 不修改 Forgotten City runtime integration。

## 18. 已关闭的设计决策

本 POM spec 没有剩余设计问题。已关闭的关键选择包括：

- 第一版使用最小 CommonMark semantic subset 与 prompt-safe XML subset；
- 不保留 comment/raw markup bypass；
- attribute order 不参与 semantic equality，text representation 强制 canonicalize；
- resolved renderer 对 block element-only XML 使用 2-space readable layout；inline/mixed XML 保持 compact，含 block Markdown 的 XML 保留既有 block flow 且不接受额外 XML indentation；
- 全量迁移现有 diff strategies，present role 从 XML name 派生；
- differ 返回 typed `XmlPatch`，resolver 返回 slot-free `ResolvedDocument`；
- public API 使用统一 node union、typed content wrappers、双 builder façade 和 read-only traversal；
- `AgentView` 使用 associated root type，`AgentViewModel` 拥有完整 system/user documents；
- role-key cursor 不跟踪 placement/TTL，只由 explicit absent/remove/clear/reset 清理；
- atomic cutover 删除 comment、raw prompt bridge 和 implicit-root compatibility；
- POM 实现 diagnostic `Serialize`，不实现 `Deserialize`；
- 所有 implementation phases 强制执行可观察的 RED -> GREEN -> REFACTOR。

当前 renderer 的 whitespace/escaping 已由实现与 tests 锁定，但不属于本文冻结的 POM type/state boundary。完整 user integration 与 collection semantics 已关闭；剩余的内部设计工作是把现有 optional delta/full `XmlNode` 结果抽成 typed `XmlPatch`，以及最终删除不再被 request path 使用的 legacy semantic compatibility layer。
