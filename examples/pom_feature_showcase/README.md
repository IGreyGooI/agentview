# POM feature showcase

> 范围说明：这是当前 POM AST/diff/renderer 的 compatibility showcase，不是
> replacement Component authoring 或 Provider request 规格。下文的两份 System/User
> `Document` 与 prompt golden 只验证 renderer 输出，不是 canonical transcript 或
> Codex HTTP body oracle。当前语义边界见
> [`../../docs/semantic-agent-view.md`](../../docs/semantic-agent-view.md)。

这是一份离线、可重复运行的“typed view → POM → prompt”可执行规格。
System 和 user 始终是两份独立的 `Document`；role 是调用哪一个 resolver
决定的，不存放在 POM 节点里。

```text
typed SystemView
  -> authored Document（仍保留 DiffSlot）
  -> resolve_system_document
       present slot: materialize
       absent slot:  omit
       no cursor / never delta
  -> slot-free ResolvedDocument ─┐
                                 ├-> render_pom_document -> provider prompt
typed UserView                   │
  -> authored Document（仍保留 DiffSlot）
  + committed UserDocumentCursor │
  -> resolve_user_document       │
       first / delta / omit / delete
  -> slot-free ResolvedDocument ─┘
  + candidate UserDocumentCursor
```

默认运行会打印紧凑的 POM 结构摘要和完整 provider prompt：

```bash
cargo run --example pom_feature_showcase
```

需要查看每一阶段完整的 authored/resolved AST 和 candidate cursor JSON 时：

```bash
cargo run --example pom_feature_showcase -- --diagnostic-json
```

JSON `Serialize` 只用于诊断，不是稳定 wire format，也不应被反序列化成
持久状态。Golden 验证：

```bash
cargo test --example pom_feature_showcase
```

## 一个最小的端到端对照

Typed user document 声明的是一个 durable XML edge：

```rust
#[view(name = "agent_context", diff)]
context: Option<WorkspaceStateView>,
```

derive 生成的 authored `Document` 中，它仍是带完整 current value 的 edge
（下面是 diagnostic JSON 的缩略片段）：

```json
{
  "Diff": {
    "role": "agent_context",
    "strategy": "Recursive",
    "value": {
      "name": "agent_context",
      "attributes": [
        { "name": "kind", "value": "workspace_state" }
      ]
    }
  }
}
```

第二轮 resolver 将当前、previous 两棵完整 XML AST 比较后，产生 slot-free
delta POM；canonical renderer 把 block-position 的 element-only XML 展开为
2-space readable layout，再放进本轮 Markdown + XML user prompt：

```xml
<agent_context rendering_mode="delta" kind="workspace_state">
  <phase>execute</phase>
  <next_command>`apply edge.owner-v2`</next_command>
  <focus rendering_mode="delta" kind="focus">
    <summary>Attach the verified owner edge.</summary>
    <rationale rendering_mode="delta">
      <none />
    </rationale>
  </focus>
  <plan rendering_mode="delta">
    <replace>
      <plan kind="plan" revision="r2">
        <plan_step n="1">Insert the verified edge.</plan_step>
        <plan_step n="2">Render one delta prompt.</plan_step>
      </plan>
    </replace>
  </plan>
  <observations rendering_mode="delta">
    <insert>
      <observation id="obs.2">
        <detail>The workspace owner is agent.a.</detail>
      </observation>
    </insert>
  </observations>
  <timeline rendering_mode="delta">
    <remove>
      <event id="event.2">Queued an edge inspection.</event>
    </remove>
  </timeline>
  <capabilities rendering_mode="delta">
    <insert>
      <item>urgent</item>
    </insert>
    <remove>
      <item>legacy</item>
    </remove>
  </capabilities>
  <agents rendering_mode="delta">
    <insert>
      <agent id="agent.c" role="operator">
        <status>ready</status>
      </agent>
    </insert>
    <remove>
      <agent id="agent.b" role="reviewer">
        <status>ready</status>
      </agent>
    </remove>
    <update>
      <agent id="agent.a" role="planner">
        <status>executing</status>
      </agent>
    </update>
  </agents>
  <facts rendering_mode="delta">
    <insert>
      <entry key="new" value="verified" />
    </insert>
    <remove>
      <entry key="obsolete" value="yes" />
    </remove>
    <update>
      <entry key="mood" value="tense" />
    </update>
  </facts>
  <transient_hint rendering_mode="delta">
    <none />
  </transient_hint>
</agent_context>
```

inline tool islands 与 XML mixed content 保持 compact；含 block Markdown 的
subtree 则保留既有 block flow，但不接受 readable XML indentation。这样
formatting 不会改变 Markdown 或 text whitespace 语义。

derive 从不生成这段 patch 字符串；delta boundary 和 operations 都由 POM
resolver/diff engine 在 AST 上产生。

## Generated POM shape 对应关系

| Typed input | Generated POM shape | 最终作用 |
|---|---|---|
| `#[agent_view(display)]` | `TextNode` scalar root | leaf lexical value；不拼 Markdown/XML |
| `#[agent_view(markdown = "paragraph")]` | `ParagraphNode` | text/code span/typed inline XML |
| `#[agent_view(kind = "...")]` | `XmlNode` | XML island，内部也可承载 Markdown |
| 无显式 `kind` 的 structured derive | inferred-name `XmlNode` | 例如 `InferredKindView` → `<inferred_kind_view>` |
| `#[agent_view(document)]` | `Document` | system/user 的 ordered block document |
| `#[view(diff...)]` | `DiffSlot` edge whose value is `XmlNode` | user resolution 后 full/delta/omit/delete |

System document 覆盖 heading、paragraph、ordered/unordered list、typed block、
XML block、present/absent system slot；structured XML 覆盖 default/renamed
attribute、element、text、flatten、root、code span、skip、Option、Vec、
`BTreeMap` 和 typed streaming-tool contract。

## 四轮 user document 与 cursor

1. `agent_context` 首次出现：发送完整 current XML，并返回 candidate baseline。
2. 下一份 complete struct instance 改变：从两棵完整 XML AST 产生 delta。
3. 再次给出相同 instance：slot 在 resolved prompt 中完全省略；旧 baseline
   仍保留。
4. `context: None`：发出显式 `<none />` patch，并从 candidate cursor 删除
   baseline。

`resolve_user_document` 只返回 candidate cursor，不会自行提交。真实 runtime
应在自己的成功边界发布它；这个离线 example 把每一个展示阶段视为成功。
普通 title/task/artifact 每轮照常发送，而且不进入 cursor。

精确 provider prompts：

- [`system_prompt.golden.md`](system_prompt.golden.md)
- [`user_turn_1_full.golden.md`](user_turn_1_full.golden.md)
- [`user_turn_2_delta.golden.md`](user_turn_2_delta.golden.md)
- [`user_turn_3_unchanged.golden.md`](user_turn_3_unchanged.golden.md)
- [`user_turn_4_delete.golden.md`](user_turn_4_delete.golden.md)

第二轮同时覆盖：

- `#[view(diff)]` scalar、nested recursive 和 nested optional deletion；
- XML `DiffSlot` 内的 Markdown `CodeSpanNode`；
- `diff(replace)`；
- `diff(append)`；
- `diff(seq)`；
- `diff(set)`；
- `diff(key = "id")` 的 insert/remove/update；
- `BTreeMap` intrinsic key identity 的 insert/remove/update；
- typed single-block 与 multi-block `TurnArtifact`。

这是 supported authoring surface 和主要 successful diff shapes 的 showcase。
保守 full-replacement、collection fallback、invalid/duplicate role 和 transaction
rollback 的完整矩阵由
[`tests/pom_user_document.rs`](../../tests/pom_user_document.rs) 与
[`tests/context_preparation.rs`](../../tests/context_preparation.rs) 锁定，不在
这个可读输出里逐一重复。

## Derive、typed builder 与 streaming tool 的边界

正常 prompt authoring 由 derive 生成 POM。现有 POM 中还没有 derive field
mode 的四种形状是 `StrongNode`、`CodeBlockNode`、`ThematicBreak` 和
multi-block `ListItem`。`RichMarkdownView` 只为它们调用 typed builders；
它没有拼接或渲染 Markdown/XML。

同一个 derived `InspectEdgeTool` 同时：

- 作为 system/user prompt 中的 typed XML contract；
- 让 `StreamingToolRunner` 从 `<tool name="inspect_edge">` 派生 parser
  dispatch identity；
- 在 example 中真实 dispatch 一次 `<inspect_edge ... />` 到 `on_open`。

contract identity 不会自动验证参数；`edge_id` 的 runtime validation 仍由
`StreamingTool` callback 明确实现。这样 prompt contract、dispatch identity
和 handler 类型来自同一个 POM root，但业务验证边界仍然清晰。
