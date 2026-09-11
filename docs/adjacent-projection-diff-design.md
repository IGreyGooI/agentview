# 普通投影只与上次完整快照比较

日期：2026-09-09

状态：已实现并完成回归验证，接入 Application / Frame compiler。普通 user/developer item 的 XML 内容
在消失时按旧 POM 与空 POM 比较。纯 text 不做语义 diff，其他 item 的删除忽略。
ToolCall、ToolResult、assistant、provider extension 和 System 不生成删除 patch；System 保留独立快照流程。
本文保留设计反例及实现边界，与 [engine.md](engine.md) 和 [HELP.md](../HELP.md) 配套。

## 1. 已确定的方向

Component 每轮声明完整的当前 POM。普通输出默认与上一次成功 handoff 的完整投影比较，
不再通过全部累计历史判断当前内容是否应该省略。

Component 输出契约见 [engine.md：Component 输出契约](engine.md#component-输出契约)：输出是模型当前
应当能够看到的完整内容，不是应用自行计算的增量，也不是要求每次重发的完整 transcript。
交付层根据有效历史 best effort 渲染 diff 或补齐缺失内容；这里的优化不能改变内容声明。
在当前实现中，FrameSession 编译 canonical diff/replay，ReactionPort 管理 wire history 并编码请求。

普通 assistant output 由 provider/runtime 历史路径管理，Component 无需回显旧回复。Component
显式提供的 assistant message 则是 authored projection，参与相邻快照比较；它的删除仍忽略，
不与 provider 自动产生的 assistant output 混为一个来源。
声明方式为 `#[assistant]`，沿用现有 placement 规则。它生成 `Message(Assistant, POM)`，
与 provider 的 `AssistantText` 保持区分；普通内容变化时完整输出，显式 `#[diff]` 可以继续使用字段策略。

```text
previous = 上一次成功提交的 complete RenderedProjection
current  = 本次准备提交的 complete RenderedProjection
changes  = compare(previous, current)

successful handoff:
    previous = current
```

同一 node 的 items 按有序序列比较。旧 item 在当前序列中消失时，将它的旧 POM 与空 POM 比较，
为其中需要撤下的 XML 结构生成删除表达。text / Markdown 不生成语义 patch 或撤回说明。
相同前缀保留；等长序列只有一个同 role item 变化时配对比较 POM，保留其后的相等项；
其他变化后缀撤下旧 XML 后完整重发。

- 普通输出不需要额外声明 `#[diff]` 才能省略与上一轮相同的内容。
- 显式 `#[developer(repeat)]` / `#[user(repeat)]` 的内容在每次提交时发送完整当前 POM。
- 不引入新的 `item` 声明、`#[item]` 或公开 item slot。
- node 继续作为内部归属分组，不增加 node 渲染标签或 item ref 协议；输出仍是 role + POM document。
- 现有 `#[diff]` 和 POM 字段策略继续用于表达结构化变化；它们的产物不能再被历史同值去重吞掉。
- 需要明确增删改、删除或失效语义的业务状态使用 XML。普通 text 不承担这些语义。
- canonical history 记录实际提交和实际交互，继续只追加。它不再决定普通 authored POM 是否变化。
- 历史中的旧消息不会因新快照变化而被物理删除。

这里的“上次”不是最近一次 render、prepare、发出的 delta，也不是最后一次成功完成的模型回答。
基线只在 successful handoff 后推进；handoff 之前的失败或取消不推进，handoff 之后的流错误不回滚。

### 1.1 比较目标

XML diff 的目标是找到能够将旧状态更新成当前状态的 patch。正确表达当前状态优先，减少输出其次；
不要求最小 patch，也不要求所有变化都能生成局部 delta。

```text
旧 XML + patch
    -> 按声明的 diff 策略解释后，与当前 XML 语义等价
```

上式是设计的验证目标，不是已实现的通用 patch apply API。set 等策略不将顺序作为语义，
因此验证的是策略下的等价，不要求所有策略都逐字重建同一份序列化文本。
无变化可以省略；能明确表达的变化生成局部 delta；无法可靠表达时使用完整当前节点作为替换。

文字不做字符或行级编辑脚本。XML 中的文字作为节点的完整值，值变化时由 XML diff 输出新值，
必要时替换它所在的整个节点。这里的“文字不做 diff”不意味着忽略 XML 中的文字变化。

已有完整提示词通过根级 `{ prompt_string }` 转为 `RawTextNode`。它支持多行并在 document 级原样
输出，内部 Markdown/XML 不解析。相邻比较将其视作一个完整值，变化时发送完整原文，消失不产生
删除操作；字符串中出现 `<policy>` 并不会使它成为可删除或可局部 diff 的 POM XML 节点。

任何 POM document 都可以比较，不要求 document 一定只有一个 XML root。
仅含 XML、root 名称唯一且顺序及名称一致时逐 root 比较；其他形状，包括混合 text/Markdown/XML、
改名、重复 root、root 数量或顺序变化，采用先撤下全部旧 XML root、再输出完整当前 document 的回退。
单个 XML root 内也可以同时包含文字和子节点。显式 authored 字段策略仍使用原有结构化 patch 路径。

## 2. 当前对象和边界

```text
RenderedProjection
  node: Component 实例的输出分组，含内部 identity
    item: role / authority + POM document
    item: role / authority + POM document
  node: 另一个 Component 实例的输出分组
    item: role / authority + POM document
```

POM document 是 item 内的内容结构。`RenderedProjectionNode` 是组件归属分组，不是 POM XML 节点。
一个组件可以有 0 到多个 item；子组件的 item 归子组件自己的 node。

当前普通内容按同组件、同 role、同 repeat 设置、连续输出合并。其他组件的普通输出可以打断这个连续性；
空输出不产生 item；`#[diff]` 本身不产生新的 item 边界。System 单独聚合在内部 root 上。
最终按 node 顺序及 node 内 item 顺序展开，不重建父子组件在源码中的交错顺序。

repeat 内容与同 role 的普通内容不合并，避免将兄弟声明一起重发。后文记录合并对相邻比较的影响，
不能假定 item 边界天然稳定。

### 2.1 placement 的作用范围

`#[developer]`、`#[user]`、`#[assistant]` 和支持的 repeat 参数作用于紧接的 `view!` 声明节点及其
子树。声明节点不是 `RenderedProjectionNode`；后者按组件实例分组，可以包含不同 placement 的 item。

```rust
view! {
    #[developer(repeat)]
    policy { "Use the current state." }

    #[developer]
    context { "The workspace is read-only." }
}
```

```text
完整 RenderedProjection
  node: 当前组件
    item: developer + POM(policy)   [repeat]
    item: developer + POM(context)  [普通相邻比较]
```

上例 repeat 只覆盖 policy。若 placement 标在组件调用前，则覆盖该组件及其子组件输出的普通 POM，
不会延伸到调用后的兄弟声明。外层 placement 优先，role 与 repeat 设置作为整体继承。
连续同 role、同 repeat 设置的声明仍可合并成一个 item；不要求每个声明节点有独立 item。

repeat 每次提交完整当前 POM，优先于 `#[diff]` 的 delta/omit 输出选择；diff baseline 仍在成功 handoff
后正常维护。render、prepare 或 handoff 前取消不推进基线。重复内容消失时仍按普通 user/developer
规则撤下 XML，text 不输出删除。repeat 是私有交付元数据，不渲染为标签；assistant/System 不支持
repeat 参数。Chess 示例的 `chess_action_policy` 用 `#[developer(repeat)]` 显式声明每轮重发。

依据：[投影类型](../src/component/execution/port.rs)、
[捕获和合并](../src/component/authoring/attempt.rs)、
[item 构造](../src/component/authoring/capture.rs)、
[现有顺序测试](../tests/component_api_projection_tree.rs)。

## 3. 相邻比较必须满足的例子

以下 A、B、C 表示完整 canonical item 值，包括 role 和 POM；不是 Component 名，也不是 item ID。
列表表示已经完成捕获和合并的 node.items，不表示同一 POM document 里的元素。
假设 node identity 和 execution scope 稳定，且不涉及 provider output 的重复表示。

| 上次完整快照 | 当前完整快照 | 必须成立的行为 |
| --- | --- | --- |
| 无基线 | `[A]` | 提交完整 A |
| `[A]` | `[A]` | 不重复提交普通 A |
| `[A]` | `[B]` | 同 role 比较两个 POM；不同 role 撤下旧 XML 并发送 B；不物理删除历史 A |
| `[B]` | `[A]` | 再次提交 A，不查询更早是否出现过 A |
| `[A, B]` | `[A, B, C]` | 保留前缀的普通增长可以只提交 C |
| `[A]` | `[A, A]` | 新增一个 A，不能把重复值当作集合 |
| `[A, B]` | `[A]` | 对消失的 B 比较旧 POM 与空 POM；需要撤下的 XML 以 B 的原 role 输出删除内容 |
| `[A]` | `[]` | XML 内容需要表达删除；纯 text 停止输出，不合成撤回通知 |
| `[]` | `[A]` | 提交 A，即使更早的快照里出现过 A |
| 任意 | 相同完整快照 | 即使本轮没有普通新增，成功提交后基线仍是这份完整快照 |

对 `[A] -> [B] -> [A]`，三轮都应有对应输出。对 `[A] -> [] -> [A]`，若空快照成功提交，
第三轮也应输出 A；如果空快照只 render 而没有成功提交，第三轮仍与第一轮的 `[A]` 比较。

新 checkpoint 必须完整替换旧快照。不能保留当前已经缺失的 node 或 item，继续把它当作上一轮内容。

### 3.1 XML item 消失时比较旧 POM 与空 POM

以下 workspace 和 policy 都是 XML 内容。

```text
上一轮完整 RenderedProjection
  node: 组件 A
    item: user + POM(workspace)
    item: developer + POM(policy)

当前完整 RenderedProjection
  node: 组件 A
    item: user + POM(workspace)

本轮比较
  workspace item: 内容相同，省略
  policy item: 旧 POM(policy) -> 空 POM document

本轮发出的变化
  item: developer + POM(上述比较产生的删除内容)
```

空 POM 是消失项参与比较时的值，不是在当前投影里补回一个空 item。
产生的删除 POM 只属于本轮 submission；提交后的完整快照仍然只有 workspace item。
若整个 node 消失，也对该 node 上一轮普通 authored items 中的 XML 内容应用这条规则。
System 和 provider/tool facts 继续遵守第 4.7 节的独立规则。

删除内容使用旧 item 的 role / authority。无需新增 canonical RemoveItem 类型、渲染 node，
或给旧消息分配 ref；renderer 仍渲染普通的 role + POM document。
例如删除单个 XML policy root，可以沿用 POM 操作包裹旧内容的形式：

```xml
<remove>
  <policy>只允许读取文件</policy>
</remove>
```

上例由 [pom_diff.rs](../src/pom_diff.rs) 的 document 比较生成，renderer 仍接收普通 POM。
多 XML root 消失时各生成一个 remove；同一 document 内先发全部旧 root 的删除，再发当前内容。
仅普通 user/developer POM 参与删除。ToolCall、ToolResult、assistant、provider extension 和 System
的 item 消失均不生成删除输出。

这条规则表达 POM 内容的撤下，不承诺按消息 ID 精确删除历史中的某次同值 occurrence。
item 重新出现时，上轮完整快照中已没有它，按新增完整内容处理。

### 3.2 text 不做语义 diff

普通 text / Markdown 是直接提供的内容。需要发送变化时，提供完整当前内容，不做逐字、逐行
patch，也不生成自然语言撤回说明或把旧文本包进 XML remove。

普通 item 与上一轮整体相等时仍可省略，这是 item 层的重复判断，不是 text 内部的语义 diff。
纯 text item 消失时，新完整快照不再保留它，也不产生文本删除输出；此前提交的文字仍在历史中。
如果业务需要准确表达一条 policy、状态或记录已经失效，应从声明时就将它表示为 XML。

XML 元素内的文字仍作为该元素的值参与比较。例如 `<status>A</status>` 变成
`<status>B</status>`，可以输出当前完整 status 元素；这不要求建立通用字符串 patch 协议。
混合 document 可以包含普通说明和 XML 状态。变化时 document 回退会撤下旧 XML，并输出完整当前内容；
普通文字本身不生成删除操作。若需要表达文字 policy 失效，应在声明时就把它表示为 XML。

## 4. 反例检查

### 4.1 只替换去重数据源，会漏掉删除和重排

旧实现的普通去重按 canonical value 和出现次数认领。若只把输入从累计 ledger 换成上次快照，
保留这个匹配算法，会得到：

| 上次 | 当前 | 仅按上次同值次数计算的新增 | 缺失的信息 |
| --- | --- | --- | --- |
| `[A, B]` | `[A]` | `[]` | B 消失了 |
| `[A]` | `[]` | `[]` | 当前已经清空 |
| `[A, B]` | `[B, A]` | `[]` | 顺序改变了 |
| `[A]` | `[B, A]` | `[B]` | B 应在 A 前面；追加历史却成为 `[A, B]` |
| `[A, A]` | `[A]` | `[]` | A 的出现次数减少了 |

这不是“和上次比较”本身错误，而是“只发送未匹配的新增值”不能完整表达这些变化。
即使采用 LCS 找到共同子序列，删除和插入位置仍需要输出语义，不能由 append 自动完成。
当 node identity 保持不变但 node 顺序改变时，逐 node 的同值比较也检测不到全局顺序变化；
有序语义需要同时覆盖 node 顺序和 node 内 item 顺序。

对于需要表达撤下的 XML，本文使用第 3.1 节的旧 POM 与空 POM 比较，不静默停止声明。
例如 developer policy 消失时，需要以其原 authority 输出 POM 删除内容。
旧 instruction 仍保留在历史中，删除表达说明其内容已撤下，不物理改写历史。
普通 text 的停止声明按第 3.2 节处理，不承诺结构化状态删除。

此前讨论的 `[-A][B]` 是逻辑变化的记号；删除也通过普通 item 携带 POM 输出，
不增加通用 RemoveItem 消息协议。实现通过刷新变化后缀表达顺序，不提供通用消息位置寻址。

### 4.2 item 的位置不是稳定身份

当前没有普通 item ID。采用 node 内 ordinal 对齐可以检测同位置的值变化，并得到：

```text
[A, B] -> [X, A, B]
按位置比较会提交 [X, A, B]
```

这是可接受的重复成本，但它本身不是一个 prepend 协议。按同值匹配可以少发 A、B，
却又遇到上一节的顺序问题。重复值也不能告诉 runtime 两个 A 分别代表哪个业务对象。

实现保留相等前缀。等长序列只有一个同 role item 变化时配对，其后的相等项继续保留；
其他变化后缀先删除旧 XML，再完整发送当前后缀。
纯增长只发送新增，纯缩短只处理删除。node identity 序列同样保留相同前缀，刷新受影响的当前后缀。
刷新的保留 node 先撤下自己的旧 XML，再完整输出；消失 node 的删除在当前 node 输出之前发送。
不能把 `(node identity, ordinal)` 悄悄升级成已承诺的业务身份，更不能为此要求用户添加 item slot。

### 4.3 相同状态与重复事件无法仅凭 POM 区分

两次独立的用户输入都可能是“继续”：

```text
第一次事件 -> [user: continue]
第二次事件 -> [user: continue]
```

相邻去重会省略第二次。若它们是两个事件，这就是丢事件；若只是同一状态的两次投影，省略才正确。
两份完全相等的 POM 无法提供足够信息来判断是哪一种。

普通快照需要明确承担状态声明的职责。需要保留重复事件时，应用可以在业务 POM 中声明增长的
记录集合，通过现有 append 策略表达新记录，而不是反复覆盖一个相同字符串。
若声明的含义就是每次提交都重新发送完整当前内容，可以使用 `#[user(repeat)]` / `#[developer(repeat)]`。
repeat 以 Frame 提交为单位；它不提供业务事件身份或 exactly-once 事件交付。

### 4.4 自动 merge 会导致没有业务变化的内容重发

同一个父组件在 A、B 之间调用一个子组件。子组件从空输出变成输出 C 时：

```text
之前：parent [POM(A, B)]，child []
之后：parent [POM(A), POM(B)]，child [POM(C)]
```

父组件的 A、B 没变，item 边界却变了。完整 item 比较会把父组件内容重发。
这是当前全局 `RenderCapture::last_run` 的结果，已有测试固定了父子 node 分组行为。

允许这些重复时，它主要是成本和可预测性问题，不应被夸大成丢状态。
若未来要稳定边界，可以单独评估 node 内 merge；本轮不将该改动混入相邻比较规则。

### 4.5 发送 B 不自动意味着替换 A

```text
left: [A], right: [A]
             ->
left: [B], right: [A]
```

runtime 知道 B 属于 left，但 Frame 的普通 item 流已经拍平，没有自动把 node identity 作为
替换地址发给接收方。累计消息 `[A, A, B]` 本身不能证明 B 替换哪个 A。

因此，“变化时发送当前完整 item”可以表达新的完整说明，但不是通用的 item replacement 协议。
对于需要严格增删、顺序或 keyed 更新的集合，应让已有 POM 结构和 diff 策略承载这些语义。
第 3.1 节采用内容层面的 POM 删除，不承诺定位历史中的特定同值消息。
不同业务对象若需要精确区分，应由已有业务 POM 结构表达身份；不能从相等内容推断不同来源。

### 4.6 基线清理也必须覆盖 `#[diff]`

旧 Delta 路径从 previous.clone() 建立 candidate，会保留消失的 slot/item。本次从当前投影建立
candidate，成功提交时删除缺失地址。Full 路径传入 None，重置显式 diff baseline。两条路径均覆盖：

```text
slot s = A -> slot s 缺失并提交 -> slot s = A
```

第三轮应该缺少基线并发送完整 A。不能复用第一轮的 A，误判成 unchanged。
因此 candidate 只包含当前仍存在的 diff 地址及完整值；删除的地址随成功提交一起离开基线。

另外，diff lowering 会省略 item 并压缩提交列表。实现记录完整 item 下标与 submission 下标的映射，
不把 lower 后的第 i 个 item 与上一份完整快照的第 i 个 item 直接 zip。
合并的 Complete fragments 变化或任一 fragment 无法生成字段 patch 时，整项使用 document 回退；
片段稳定且支持字段策略时继续输出原有 semantic delta。

### 4.7 Full、provider facts 与提交生命周期不能混为一种去重

本地完整快照基线和 target 的 DeltaFrom 基线有不同用途：

- 本地 compatible checkpoint 决定普通 authored 内容相对上一轮是否变化。
- target continuity 决定这次传输是否可以依赖先前的 Frame，还是必须 Full。
- 同 scope 下改发 Full 不代表本地上一轮快照消失。当前 Full replay 已包含完整 canonical history。
- Full 中依赖旧 target 的 `#[diff]` patch 必须回退完整值。无兼容 checkpoint 或新 scope 才按缺少基线处理。
- System 继续使用独立快照及 replace/clear 规则，不进入普通 item 匹配。

provider 输出和 staged tool facts 的认领，解决的是同一条事实同时出现在 replay、staged inputs、
component section 时被重复提交的问题。它不能随着普通 authored 历史去重一起删除。

例如某个 provider occurrence 已被 component 认领，随后 component 省略它，再次表达相同值时：
该 occurrence 已不在 unclaimed pool 中；当前同 scope 下依赖累计 node ledger 省略这次重现。
`provider_outputs` 主要参与跨 scope 的 ambiguity 记录，简单保留它并不能替代这个行为。

实现保留只包含 provider claims 的累计 node 记录。先认领同 node 已认领过的 provider occurrence，
再认领本 scope 尚未认领的 provider occurrence；相同 authored 快照不消耗新的 provider occurrence。
普通 authored 状态回归满足第 3 节的 `[A] -> [] -> [A]`。跨 scope 仍延续现有 ambiguity fence。
value-only 表达无法始终区分独立 authored 值与同值 provider fact，本次保留该来源边界。

## 5. 最终规则

保留“只和上一次成功提交的完整投影比较”作为默认基线规则。它解决了累计历史吞掉 A 回归的问题，
也让业务代码继续声明完整 POM。

item 的出现和消失先由有序序列比较识别。消失项的 XML 内容使用旧 POM 与空 POM 的比较结果，
以旧 role 生成本轮普通输出。text 不做语义 diff；需要明确增删改的内容使用 XML。
node 不渲染，不增加 item ID/ref，用户继续声明完整 POM。

三个实现细节已经落定：

- 序列对齐：相同前缀保留，简单同 role 替换比较 POM，复杂后缀撤下 XML 后完整重发。
- XML 删除：旧 root 用 remove 包裹；不扩展 text / Markdown patch，不处理其他 item 类型的删除。
- 来源：普通完整快照与 provider occurrence claims 分开保存；跨 scope 来源歧义继续拒绝。

正确性优先于最小输出。需要精确区分同值业务对象、跨 node 同名 root 或集合位置时，仍由业务 XML
结构及既有集合策略表达，不能把无地址的拍平 item 流当作任意历史消息的 patch apply 协议。

## 6. 实现位置

以下文件承载实现及验证。Application 的 Frame compiler 执行新规则；deprecated ProviderPort
兼容接口保留自己的传输历史路径。

| 位置 | 职责 |
| --- | --- |
| [view_macro.rs](../agentview-derive/src/view_macro.rs) 与 [capture.rs](../src/component/authoring/capture.rs) | `#[assistant]` 捕获为 authored `Message(Assistant, POM)`；解析 user/developer repeat，按 placement 合并并记录私有 repeat item 元数据 |
| [projection_diff.rs](../src/component/execution/projection_diff.rs) 的 `ProjectionReconciliationState` / `reconcile_projection_submission` | 移除累计 node ledger 对普通 authored 内容是否变化的判断职责；以 compatible complete projection 进行相邻比较；provider provenance 独立保留 |
| 同文件的 `ProjectionDiffState::prepare` / `lower_node` / `lower_template_item` | 清理消失的 diff 地址；保留完整 item 与发出形式的对应关系；变化产物不能再次被历史同值吞掉 |
| [pom_diff.rs](../src/pom_diff.rs) 及 projection 的 POM lowering | 补齐消失项中旧 XML 内容与空 POM 的比较和删除编码；不扩展 text / Markdown diff，也不为此要求新的 item 标注 |
| [frame.rs](../src/component/execution/frame.rs) 的 `FrameSession::prepare` | 分别传入本地 compatible complete checkpoint 与 target delta checkpoint，避免把 Full 误当成没有上轮快照 |
| 同文件的 `FrameCheckpoint` / `commit` | 复用已有 `complete_projection`；保持快照、diff 基线、history 和 staged receipt 的原子提交 |
| [application.rs](../src/component/execution/application.rs) 的 `submit_prepared_frame` | 保持成功 handoff 后同步 commit；验证取消、重试和流错误不会错误推进或回滚基线 |
| [HELP.md](../HELP.md) 与 [engine.md](engine.md) | 说明相邻快照、XML 删除及来源边界 |

目前 `FrameCheckpoint.complete_projection` 已经保存上一次成功提交的完整投影，
无需为了持有相邻快照再增加一份同用途状态。

相邻比较本身不要求新 item 语法。独立的 authoring 扩展增加了 `#[assistant]` 和 user/developer repeat；
repeat 与普通内容分开合并，其余捕获和 node 归属规则保持原设计。
XML 删除操作应先由 POM lowering 构造成普通 document，再交给现有 renderer 和 provider role 编码。
只补齐承载 XML 删除所需的能力，不另造 item ref 协议或纯文本撤回 renderer。

## 7. 验证清单

以下为验证要求；XML 语义目标通过具体输出断言验证，不表示提供了通用 patch apply 引擎。
覆盖位于 `projection_diff.rs`、`pom_diff.rs`、`frame.rs` 及 Application 测试层：

- `[A] -> [B] -> [A]`、unchanged、保留前缀的增长、相同值出现次数增加。
- XML patch 应用后的状态与当前 XML 在声明策略下等价；文字变化以完整节点值表达，不生成字符级编辑脚本。
- `[A] -> [] -> [A]`，分别覆盖中间空快照成功提交与未提交；node 缺失和 node.items 为空。
- 消失项的 XML 产生旧 POM 到空 POM 的删除内容，沿用旧 role，且不进入新完整快照；覆盖多 XML root。
- 纯 text 变化时提供完整当前内容，消失时不生成删除通知；混合 document 只为 XML 内容生成语义 diff。
- 删除、清空、prepend、重排、重复值减少，断言选定的输出语义，不能只验证最后剩余的新值。
- `#[diff]` 地址消失后重现；diff item 被省略后，后面的普通 item 不发生比较错位。
- 普通内容与 diff fragment 合并；子组件输出改变导致父组件拆分时的完整重发。
- prepare 后丢弃、submit Pending/Err、handoff 前取消、handoff 后 stream fault。
- 同 scope 的 Full 与 Delta、新 scope、System 更新和清空。
- ToolCall、ToolResult、System、assistant 和扩展 item 消失不产生删除 patch。
- `#[assistant]` 的连续合并、角色继承、相邻变化和显式字段 diff；与同文字的 provider output 区分，Full 时正确重建两者。
- user/developer repeat 的不变重发、声明子树范围、外层优先、普通兄弟隔离、字段 diff 优先级及消失后重现。
- provider/staged occurrence 在同一 Frame 中只表示一次；省略后重现及跨 scope 来源歧义。
- 相同文本的两个业务事件，验证使用增长记录时保留两次，普通相同状态仍省略。

新增公开 Application 路径测试见 [component_api_adjacent_projection.rs](../tests/component_api_adjacent_projection.rs)。
已运行工作区 `--no-default-features` 测试、默认配置测试及定向格式检查。默认配置首次运行出现一个
CLI 测试释放端口后重绑定的 `AddrInUse`；该测试隔离复跑通过，其余默认工作区测试通过。
实时 API 测试保持忽略。本文的语义目标不代表已经提供通用 XML patch apply API。
