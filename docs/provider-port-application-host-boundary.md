# ProviderPort 与 ApplicationHost 边界确定文档

> **Status: superseded by [`engine.md`](engine.md) and
> [`frame-driven-runtime-plan.md`](frame-driven-runtime-plan.md).** This body and the linked
> [`component-provider-boundary.html`](component-provider-boundary.html) and
> [`component-provider-boundary.architecture.json`](component-provider-boundary.architecture.json)
> visual artifacts are retained only as a historical pre-Frame design snapshot.
> `ProviderPort`, `ApplicationHost`, and `ComponentReactionRuntime` exist only in the temporary,
> default-enabled, deprecated `legacy-provider-port` compatibility surface. New integrations use
> `Application<P>`, `ReactionPort`, ordinary Component roots, and `use_provider_event_handler`.

日期：2026-08-08

本文只记录已确认的边界，不讨论实现步骤。

可视化版本见
[`component-provider-boundary.html`](component-provider-boundary.html)，其可编辑源文件是
[`component-provider-boundary.architecture.json`](component-provider-boundary.architecture.json)。

## 总体边界

```text
ComponentHost（业务权威）
  props + use_signal state
          |
          | render
          v
ComponentHost::render
  +-- RenderedProjection nodes -----> ProviderPort::execute
  `-- reaction-local consumers <---- ProviderEvent stream
                  |
                  v
              Signal 更新
```

Provider context 是可丢弃的运行时优化，不是业务权威。

`RenderedProjection` 是完整的当前 ordered Component node vector，不是已经拍平的全局
conversation。每个 Component node 输出一个有序的 `Vec<CanonicalInputItem>`，其中可以包含该
Component 自己选择保留的业务历史。node vector 保留 Component 边界、runtime `ComponentId`
和 Component 之间的顺序；不保留或重建跨父子 Component 的 item 源码交错顺序。
`ComponentId` 是内部 provenance，不要求 public key API；identity 变化时 Port 把它视为新
node，旧 submitted history 仍不撤回。

`#[diff(slot = "...")]` 标记当前完整 User POM 中一个可增量提交的片段。Component 仍输出完整
`CanonicalInputItem`；projection 只额外携带 `ComponentId + structural path + slot` provenance
和该 item 的 fragment template，不复制 canonical item。Port 保存同一地址上一次成功采用的完整
POM baseline：首次或 context 丢失发送 full，相同值 omit。atomic root 或只有一个字段的 wrapper
发生变化时，直接发送完整当前值，不增加 root-level delta/replace 包装。只有包含多个字段、能够
省略稳定字段的结构化 root 才发送 semantic delta；其中 `<replace>` 只由结构化 root 内显式
`diff(replace)` 字段产生。其他无法安全表达的细粒度变化回退为当前完整 root 或完整字段。
候选 baseline 只在 Provider stream 到达正常 public EOF 时与 Port context 一起采用。
本轮生成的 delta 或 full-fallback 是 operation，必须追加到 Provider 输入；即使历史中存在文本
相同的旧 operation，也不能被 ordinary submitted-history 去重。

Component-owned history 只表示由业务 POM 重新表达的权威 history。assistant/provider
conversation 只属于 Port 私有、可丢弃的 context，不进入 Component 业务状态。Provider context
丢失时，Port 从当前业务 projection Fresh；公共 Component authoring 不新增 `#[assistant]` 或 raw
`CanonicalInputItem` 注入 API。

## ComponentHost

`ComponentHost` 保存 root props、mounted Component identity、`use_signal` state 和当前 POM。
一次 render 同时产生完整 projection 和只属于本次 reaction 的 ProviderEvent consumers。
它没有 application root event bus。

当前只实现一个 state hook：

```rust
let state: Signal<T> = use_signal(|| initial);
```

Signal 本身是可传递的 state handle，可以被 Component、外部 async task 和 LLM feedback
handler 直接更新。`Signal<T>` 是唯一的 state handle，不另设 `SignalSetter<T>` 或 `.setter()`
兼容层。写入只更新 state、标脏并唤醒 scheduler，不会自动调用 LLM。

所有非 Provider 输入，包括外部业务输入、clock 和 application command，只通过 props 或
Signal 更新 Component state，不进入 ProviderEvent stream。这是 Component 输入路由约束，
不会给 `ApplicationHost` 或 `ProviderPort` 增加新的公共时序语义；ProviderEvent 的到达时间与
Clock 写入无关。

在 Chess 业务适配中，`set_props(new_turn)` 返回后新 props 立即是当前业务输入，已有 Clock
Signal handle 继续有效。Clock 不绑定某次 render generation，而是直接更新 Component-owned
Signal。若需要 per-turn Clock identity，它属于业务 state/update（例如 `turn_id`），不通过
binding 表达。普通 render 保留该 Signal，remount 或 Component 删除才使旧 handle stale。

框架不 fork 或回滚 Component state。每次 Signal 更新后，业务状态本身必须自洽。

## ApplicationHost

`ApplicationHost` 负责：

- 调用 `ComponentHost::render` 从当前 state 产生完整 ordered node projection 和 reaction-local
  consumers；
- 驱动选定的 `ProviderPort`；
- 将 `ProviderEvent` 派发给本次 reaction 的 Component consumers；
- 管理运行任务和取消。

通用 Host command 是 `dispatch_llm_reaction`。

一次 `dispatch_llm_reaction` 在 `ProviderPort` 发完 ProviderEvents，并且这些 Events 全部处理完成后
结束。同一次 reaction 不自动 rerender，也不再次调用 `ProviderPort::execute`。

普通 ProviderEvent dispatch 对 reaction 采用 sync-blocking 语义：按 Provider 发出的顺序逐个
Event 处理；一个 Event 的匹配 handlers 按 Component 结构顺序逐个 `await`。当前普通 Event
的所有 handlers 完成前，不消费下一个 Provider Event。这里允许 handler 本身是 async，但不
允许通用 Event dispatcher 并行执行 handlers。ToolCall 使用下面确定的特殊 Component consumer
和 reaction-local async lane，不受这条普通 Event handler 串行规则约束。

`ProviderEvent` 是 framework-owned、provider-neutral 的 LLM Provider 输出事件模型。
Application 不定义 root Events enum，也不定义 Provider 输出语义。

Provider native ToolCall 是一种特殊 Event，必须由专门的 Component consume，不能只走普通
typed Event 的通用 `EventListener` 路径。支持 native-tool lowering 的具体 ProviderPort（当前是
Responses）只在 projection 中有匹配的 native ToolCall Component 时开启相应 capability；没有
匹配 Component 时不向 Provider 开启，而不是开启后再把返回结果判为 unsupported。Chat
Completions 当前没有这项 lowering；projection 声明 native tool 时，它在 HTTP、history 和 diff
memo handoff 前由 Input Gate 本地 fail closed。

当前 public authoring 是 name-only 的
`NativeToolCall::named(name).on_call(handler)`，没有 input-schema 参数。能力开启后，具体
ProviderPort 负责接收并保留 Provider native output，在一次 call 的 provider lifecycle 完整
闭合后转换成一个 provider-neutral `ToolCall(call_id + name + raw_arguments)` Event。
ApplicationHost 把这个 Event 直接交给匹配的 `NativeToolCall` Component，不进入普通
`EventInput` / `EventListener` 路径；每个 call 启动一个受 reaction 管理的 async lane。
handler 返回 `Result<ToolOutput, E>`，通常通过 `call.output(content)` 把 output 绑定到原
`call_id`。lane 完成后，ApplicationHost 把这一个 output 交回 Provider-owned sink；Responses
Port 在下一次 Input Gate 按 call 顺序提交它。handler 也可以显式更新业务 Signal，但
ApplicationHost 不把 tool result 当成业务状态，`RecordLog` 也不保存或自动推进它。

多个 reaction-local tool lanes 可以与后续 Provider Event 和彼此并行推进；reaction 返回前会
等待所有 lane 闭合或失败。这是 Host 对已经完成的 ToolCall handler 的内部调度，不是 Provider
request 的 `parallel_tool_calls` policy。

Streaming XML authoring 现在有两个并存入口。`StreamingXml::tag(tag)` 是 prompt-free 的
specific-tag lifecycle subscription，提供 `on_open`、`on_stream`、`on_complete` 和
`on_invalid`；`XmlStreamingToolCall::contract(...)` 是 prompt-producing typed declaration，
会投影 example syntax 并解码匹配的 empty element。相同 event route 上的两者注册到同一个
parser hub，按 XML source order 分发。它们都消费 XML text stream，而不是 Provider native
ToolCall Event；native ToolCall 仍是另一条独立管线。

当前没有 public `ParallelToolCallComponent`，也没有 request-level `parallel_tool_calls`
authoring；Responses request 当前将该字段保持为 `false`。未来如果增加这项 policy，它只控制
Provider 是否可以在同一个 response 中产生多个 call，不负责开启 tool capability，也不改变
上述 reaction-local lane 生命周期。

ApplicationHost 不保存 Component 业务状态、render baseline、Provider context 或 `RecordLog`。

`observe / act` 不是通用 ApplicationHost API。它只属于下面的 external 特例；external
`observe` 在内部调用 `dispatch_llm_reaction`。

## ProviderPort

公共 Provider 边界只有一个方法：

```rust
#[async_trait]
pub trait ProviderPort: Send {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault>;
}
```

`ProviderPort` 的完整边界是：把 provider-neutral projection 转换成一次具体 Provider
execution，再把 Provider 输出转换成 provider-neutral `ProviderEvent` stream。

确定的语义：

- 每次 `execute` 都接收完整、provider-neutral 的当前 ordered node projection；
- Port 可以通过 `&mut self` 保存自己的 runtime context；
- Port 返回 framework-owned、provider-neutral 的 `ProviderEvent` stream；
- Port 根据投影控制 request-time Provider capabilities，并私下保留 continuation 所需的
  Provider output；
- Port 没有公共 `observe`、`act`、`Action`、`render`、`Rendered`、commit 或 rollback API。

`RenderedProjection` 只包含 Provider 可见的数据。Signal、closure、Component consumer 和
其他 reaction-local runtime 不进入 ProviderPort。Provider response id、remote artifact id、
wire frame、continuation state 和其他具体 Provider 数据也不越过 Port 进入 Host。

projection 只保留 ordered `Vec<RenderedProjectionNode>`；每个 node 内的 items 有序。
它不携带跨 node 的 item-order addresses 或重复的 flat items。新增 items 跨 node 按 node vector
顺序处理。diff provenance 是 node-local POM fragment sidecar，不是第二份 item order 或业务历史。

## 三条生产 ProviderPort 实现线

当前有三条 Provider 实现线：

1. Responses API；
2. `ExternalProviderPort`；
3. Chat Completions API。

三者都实现同一个 `ProviderPort`。实现优先级也是这个顺序。

另有诊断用 `DebugProviderPort`：它不调用模型，只使用和 External 相同的 provider-neutral
canonical renderer 捕获当前完整 projection 的可读 prompt preview。它不冒充 Responses/Chat
私有 retained wire history，也不会自动打印可能包含敏感业务内容的 prompt。

人工验收入口是 `cargo run --example provider_port_visual_acceptance`。它用本地 mock endpoint
执行真实 Responses ProviderPort，同时并排显示每轮 Debug complete prompt 与新增 wire
submission，覆盖 typed multi-field state 的 field patch、append insert、omit、append fallback、
重复 delta 和新 Provider Fresh full，并明确展示稳定 `objective` 被 delta 省略。报告同时声明
atomic root 变化直接提交完整当前值；该路径由 Responses/Chat focused tests 锁定。运行不需要
API key 或外网。

## ExternalProviderPort 特例

`ExternalProviderPort` 专门用于 skills/CLI 调用外界 agent。`observe / act` 是包在它外面
的一层特例，不是通用 Host 或 ProviderPort API。

```text
external wrapper observe
  -> 当前 external reaction 以 normal EOF 结束（如果存在）
  -> ApplicationHost 执行 finish_normal
  -> ApplicationHost::dispatch_llm_reaction 开启下一条 reaction
  -> ExternalProviderPort
  -> 返回基于 external rendering baseline 的 Full 或 Delta observation

external wrapper act(one external text protocol stream)
  -> 把整条外界大模型输出流注入当前 external reaction
  -> ExternalProviderPort 在 Port 以下解析 wire 并按顺序转换成 text ProviderEvents
  -> 多个 TextDelta 和可选 TextComplete 进入正常串行 dispatch
  -> 显式 TextComplete 在 dispatch 后结束，不再读取后续输入
  -> 正常 protocol EOF 且尚无 TextComplete 时，Port 从有序 delta 累计文本合成一次 TextComplete
  -> 异常断流保留为 stream error，不冒充正常 EOF
  -> ApplicationHost 执行 finish_normal，当前 reaction 结束
  -> 开启下一条 reaction，并返回新的 observation

external wrapper observe --full-re-render
  -> 为当前 pending reaction 重发完整 Full observation
  -> 重建 external rendering baseline
  -> 不注入 EOF，不执行 finish_normal
  -> 不结束当前 reaction，不开启新 reaction
```

`ExternalProviderPort` 自己不提供 `observe` 或 `act`。外层如何把输入交给它属于该特例的
实现，不进入 `ProviderPort` trait，也不建立第二套事件流程。普通 `observe` 和 `act` 都会以
normal EOF 结束当前 reaction，运行 `finish_normal`，再开启下一条 reaction；区别只是
`act` 在结束前先注入一整条外界 text protocol stream。一次 `act` 覆盖整条流，
delta 不是多次 `act`。因此 `act` 具有 `act then observe` 语义，连续调用时每次都作用于
wrapper 当前持有的 reaction。

外界 protocol 明确产生 `TextComplete` 时，Port dispatch 该 Event 后立即结束当前输出流，
不再 poll 后续 wire。正常 protocol EOF 未产生 `TextComplete` 时，Port 在自己边界以下累计
已有序 dispatch 的 delta text，合成且 dispatch 正好一次 `TextComplete`，再进入 normal EOF。
异常断流必须转换为 Provider stream error，不合成 completion，不执行 normal EOF 路径。

`ExternalApplication` 是长期 wrapper。它保存 external rendering baseline，记录外部 Host/AI
已知的 observation 内容，并向外提供 rendering generation。普通 observation 可以是
`Full` 或 `Delta`；`Full` 携带可建立 baseline 的 generation，`Delta` 必须同时携带
base generation 和 new generation。外部缺少匹配的 base 时，使用
`observe --full-re-render` 重发当前 pending reaction 的完整 observation 并重建 baseline。

rendering generation 只用于 `Full / Delta` 恢复，不是 `observation_id`，不绑定 pending
reaction，`act` 请求也不回传 generation。当前 reaction 本身就是内部关联。具体
Rust stream 类型、protocol wire、归一化细节、diff 编码和 Markdown 渲染格式都不在此冻结。
这些 protocol stream 和归一化类型不出现在外层 wrapper 的长期公共 API 中。

## Port 以下

具体 ProviderPort 私下拥有完成 `execute` 所需的、可丢弃的 execution context，例如 wire
encoding、submitted history/order、prompt cache、continuation、inline instructions、remote
artifact reference、compaction、recovery、retry、receipt 和可选持久化。Port 保留这些状态是
为了复用具体 Provider 的 prompt cache 和会话上下文，不是为了承载业务权威；它们不是 Host
状态，也不形成平级公共模块或额外 ports。

只有具体 Provider 确实把 canonical artifact 解析成 inline payload 或 remote reference 时，
才需要在内部保存相应映射。Inline 实现可以只保留必要的 request material；remote-retained
实现可以保留 Provider reference。两者都不形成通用或公共 ArtifactBinding 系统。

Port 有兼容 context 时保留已经 submit 的历史。一次新的 full render 可以重复历史、增加
items，也可以省略之前出现过的 items；省略不表示删除或撤回，已经 submit 的历史保持原顺序。
Port 根据自己私有的 submitted history 找出各 Component node 尚未 submit 的 items，保持 node
内部顺序，并按当前 node vector 顺序追加。跨 reaction 的累计提交顺序只由 Port 私下保存。

对于 `#[diff]` item，Port 先用 accepted complete POM baseline lower 出本次 full/delta/omit
submission，再进入上述 submitted-history reconciliation。Port 保存的新 baseline 始终是当前完整
POM，而不是发出的 delta。

例如 submitted history 是 `[A, O]`，新 node vector 是 `[[A, B], [O, P]]`，下一次 submit 是
`[A, O, B, P]`。之后 projection 变成 `[[A, B, C], [O]]`，缺少的 `P` 不撤回，结果是
`[A, O, B, P, C]`。

projection 内容减少或没有公共前缀本身不会触发 Fresh。Provider context 丢失或开始新的
Provider session 时，Port 从本次完整 node projection Fresh。此时只能恢复当前 full render 实际携带的
历史；业务继续所必需的历史必须由 Component state 保留并重新 render。远端 attachment id
不是业务权威。

`#[system_once]` renderer 仍会执行。有效 Port context 内默认采用 Port 已保留的 provider-side
表示；Fresh 时采用当前 render 的值。

## 失败原则

- Provider context 丢失不能破坏业务状态；
- 已成功的 Signal 更新和外部 effect 不回滚；
- 不自动重放不确定的 provider execution；
- 业务继续所需的信息必须存在于 Component state。

## 非目标

当前不引入外层 AgentLoop、`RecordLog`、fork、revision/CAS、通用 effect system、额外 hook
或 durable business workflow。
