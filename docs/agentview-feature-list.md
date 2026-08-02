# AgentView 面向 LLM Application 的 Feature List

> Status: working product and engineering reference | Last reviewed: 2026-08-01

## 1. 文档目的

本文定义 AgentView 作为 **Application for LLM** 应提供的产品能力，并回答三个问题：

1. AgentView 自己负责什么，不负责什么；
2. 每项顶层 Feature 的用户可观察行为是什么；
3. 当前实现距离 Cube Stage、Forgotten City agents 和 Forgotten City semantic graph 的
   生产需求还有多远。

本文以 Feature 和验收行为为主，不以 Rust 类型或内部模块为目录。`POM`、cursor、channel、
lease、facade 等术语只用于解释实现证据，不作为产品 Feature。

本文也是后续逐项测试的索引。只有通过目标 public boundary 的可执行测试，才能把一项能力
标记为消费者可用；roadmap、private unit test 和 isolated example 只能证明设计方向或局部机制。

快速结论：AgentView 的 typed View、semantic update 和 compatibility turn loop 已较成熟；
stream/tool/effect 已有可执行局部实现。Chess 的 canonical runnable references 现在属于
AgentView：daemon-backed in-memory `agentview chess` CLI/repository skill，以及 scripted
AgentLoop example。CLI 支持 `attach -> attach-ack -> observe -> ack -> act(handle, raw XML)
-> hook -> resync`；它可以跨 client subprocess，但只在同一 daemon 生命周期内保持状态。
Forgotten City 的 SQLite Chess 则是 consumer integration/durability test，不是用户运行的
example entrypoint。它证明 durable System/User lineage、replay、outbox 和 Stockfish job；
managed remote transport、常驻恢复 supervisor、真实远端 provider、显式 transport handoff
和统一 retained component definition 仍未完成。

## 2. 产品定义与责任边界

AgentView 的核心定义是：

```text
Agent != Application + LLM
Agent = Application for LLM
```

```text
Application state
      | capture
      v
AgentView View + Action Surface ----> LLM
      ^                                |
      | update/result                  | structured output / tool call
      |                                v
Application state <---- AgentView reducer / effect lifecycle
```

传统应用把状态渲染成人能阅读的页面，把可执行操作呈现为按钮、表单和菜单。AgentView
做同一类工作，但使用者是 LLM：应用把领域状态投影成 LLM 可读的 View，把允许的操作声明成
typed action，把执行反馈组织成下一轮可理解的更新。

| Human Application | AgentView Application |
| --- | --- |
| 页面及页面结构 | typed System/User View |
| 页面状态更新 | full/delta/delete semantic update |
| 按钮、表单和菜单 | structured action / provider tool contract |
| 用户操作 | streamed model output / tool call |
| event handler | reducer / dispatcher |
| optimistic UI | tentative Live effect |
| 提交事务 | publication-gated Commit |
| 错误提示 | typed Diagnostic / retry feedback |
| 应用会话 | AgentView session / mounted application |
| 刷新或重开 | reload / reopen / recovery |

### AgentView 负责

- 定义 LLM 能看到的 Application View 和能够执行的 action；
- 从应用状态捕获一份一致的 turn snapshot，并生成当前交互界面；
- 解析 LLM 的流式文本、结构化事件和 provider-native tool call；
- 编排 action result、实时效果、最终提交、诊断和取消；
- 维护 view lineage、turn、call、session 和 application epoch 的一致生命周期；
- 给 provider、persistence 和 effect host 定义可替换的接入契约；
- 提供足够的观察和回放信息，让一次 Application 交互可以被验证。

### AgentView 不负责

- 不替 LLM 决定剧情、角色意图、graph mutation 或其他领域策略；
- 不拥有 Cube Stage、Forgotten City 或 semantic graph 的权威业务状态；
- 不内置某个模型供应商的网络客户端、鉴权、限流或计费逻辑；
- 不内置 MySQL/PostgreSQL schema，也不替消费者实现具体数据库 adapter；
- 不把 NPC、GM、Director 等产品概念写进通用 runtime；
- 不把“多 Agent 协作”抽象成隐式共享记忆。每个 Application instance 保持隔离，协作通过
  应用显式暴露的 state 和 action 完成。

边界原则是：**Application 拥有事实和业务语义；AgentView 拥有这些事实如何被 LLM 看见、
操作和可靠提交的交互语义。**

## 3. Feature 总览

优先级针对当前三个消费者：`P0` 是生产迁移的必要能力，`P1` 可以在首个生产迁移后完成，
但当前架构不能阻断它。

| ID | Feature | LLM 可观察的能力 | 优先级 |
| --- | --- | --- | --- |
| `AV-F01` | Application Definition | 同一个 Application 具有稳定的规则、能力、输出协议和身份。 | P0 |
| `AV-F02` | Typed Application View | LLM 看到的是从领域状态生成的 typed、可验证视图，而不是拼接字符串。 | P0 |
| `AV-F03` | Semantic View Update | 首轮看到完整状态，后续看到有语义的新增、修改和删除。 | P0 |
| `AV-F04` | Turn Composition | 每轮清晰组合当前状态、任务、artifact、反馈和本轮约束。 | P0 |
| `AV-F05` | Action Surface and Validation | LLM 明确知道当前允许执行哪些 action；过期或越权 action 不产生副作用。 | P0 |
| `AV-F06` | Structured Streaming | 结构化输出在生成过程中被增量识别、验证和处理。 | P0 |
| `AV-F07` | Provider-native Tools | LLM 可以调用模型原生工具，并在同一交互中收到对应结果。 | P0 |
| `AV-F08` | Effect Lifecycle | 输出、实时效果、最终提交和诊断具有不同且明确的生命周期。 | P0 |
| `AV-F09` | Reactive Observe/Act | 外部 LLM、CLI 或 daemon 可以观察 Application、执行 action、等待更新。 | P0 |
| `AV-F10` | Model-backed Turn Loop | 内嵌 LLM 可以重复观察、行动和接收结果，直到等待或完成。 | P0 |
| `AV-F11` | Session, Fork and Isolation | 历史、View 基线、parent/child work 和并发实例保持连续且互相隔离。 | P0 |
| `AV-F12` | Safe Call Lifecycle | 过期输入被拒绝；取消、失败、重试和 replay 不破坏已提交状态。 | P0 |
| `AV-F13` | Durable Application Lifecycle | Application 可以重开、恢复未完成 call，并可靠发布外部副作用。 | P0 |
| `AV-F14` | Provider-independent Application | 更换模型 Provider 不改变 Application 的 View、action 和 effect 语义。 | P0 |
| `AV-F15` | Versioned Reconfiguration | 更新规则或能力时形成明确的新 epoch，不污染正在运行的旧 call。 | P1 |
| `AV-F16` | Inspection and Replay | 一次交互的输入、输出、action、effect、提交和失败可以检查与比较。 | P0 |

这些 Feature 共同组成一个完整的 LLM Application。只会渲染 prompt 的库只能覆盖
`AV-F01` 到 `AV-F04`；只有继续覆盖 action、effect、loop、session 和 recovery，才能支持
长期运行的 Agent application。

## 4. Feature 详细定义

### Application authoring 与 View

| Feature | 必须成立的行为契约 |
| --- | --- |
| `AV-F01` Application Definition | Application author 定义一棵完整、可组合、带版本的长期 Application definition。它包含 System policy、输出协议和 capability declaration。一个 application epoch 只生成并附加一次 System；普通 turn、retry、continuation 和 reopen 不得悄悄追加第二份 System。 |
| `AV-F02` Typed Application View | 领域对象先投影成 typed View，再进入统一的结构化文档和 canonical renderer。非法 XML 名称、非法节点组合和不完整文档应在类型检查、构建或渲染阶段失败。业务代码不把手写 markup string 当作 View AST。 |
| `AV-F03` Semantic View Update | View 支持 full、unchanged、delta 和 delete。集合更新保留稳定 identity。provider baseline 只在严格 reducer 与 mounted publication 成功后推进；external baseline 只在同一 consumer 明确确认 immutable Actionable delivery 后推进，之后的 domain action 失败不会抹掉已经发生的 delivery。render 本身和 Passive frame 永不推进 baseline。 |
| `AV-F04` Turn Composition | Application author 决定当前 context、artifact、retry feedback、task 和其他区块的顺序。每次 preparation、history replacement、provider-requested User resync 和 continuation 都重新捕获一份 owned state snapshot；被舍弃的 preparation 不创建运行时 action handler。 |

### Interaction 与 action

| Feature | 必须成立的行为契约 |
| --- | --- |
| `AV-F05` Action Surface and Validation | action contract、LLM 可见 schema 和实际 handler 来自同一声明。action 绑定当前 turn/epoch 和 captured props；Application 可以在执行前针对最新权威状态再次验证。未知、非法、过期或越权 action 返回 model-visible feedback，且不产生副作用；基础设施故障才终止当前 attempt。 |
| `AV-F06` Structured Streaming | streamed text 按 wire order 进入同一个 parser。`open`、内容增量、`complete` 和严格 EOF 都有明确事件；chunk 边界不改变语义，畸形或未闭合结构不能被当成成功。每个 attempt 使用全新的 parser/reducer state。 |
| `AV-F07` Provider-native Tools | 一个 Application 可以声明多个 native tool。调用按 Provider 顺序执行，tool name、invocation identity、参数、结果 correlation 和 transcript 保持一致。同一 invocation 的完全相同 replay 不重复 I/O；identity 相同但 payload 不同是 terminal collision。 |
| `AV-F08` Effect Lifecycle | `Output` 是本轮产物；`Live` 是 provider 仍在运行时必须 await 的实时效果；`Commit` 只能在 turn publication 成功后进入 durable delivery；`Diagnostic` 描述非终止问题。多个 Live/child effect 具有稳定 scope、顺序和 acknowledgement；失败或取消必须对已执行的 effect 做显式 abort/compensation。 |
| `AV-F09` Reactive Observe/Act | Application 可向外部控制者提供完整 `observe`，接收针对当前 turn 的 `act`，并通过 `hook` 等待 application-side awake。旧 epoch 或旧 turn 的 action 被拒绝。partial update 是优化，完整 snapshot 永远是正确 fallback。 |
| `AV-F10` Model-backed Turn Loop | Provider-backed Application 执行“准备 View -> 请求模型 -> 处理 action/result -> 提交 -> Wait/Continue”。loop 有明确上限；context replacement 重新准备同一逻辑 turn，但不提前提交 history、View baseline 或 side effect。 |

### Session、可靠性与运行环境

| Feature | 必须成立的行为契约 |
| --- | --- |
| `AV-F11` Session, Fork and Isolation | 每个 Application instance 独立拥有 prompt history、working state、View baseline、active epoch 和 call ledger。Application 可以从已提交 baseline 派生 child/fork work；parent、child、多个 NPC 或多个 graph session 不能通过隐式全局状态互相污染，child 的取消与结果归属必须明确。 |
| `AV-F12` Safe Call Lifecycle | 一个逻辑 call 具有稳定 identity 和 immutable input revision。重复相同输入返回已接受结果，不重新执行；同一 call identity 配不同输入必须拒绝。取消只影响目标 call，并等待 Provider、tool 和 Live cleanup 真正退出。 |
| `AV-F13` Durable Application Lifecycle | session mutation 与待发布的外部 effect 在同一原子 publication 中落盘。owner 丢失或进程重启后，Application 能区分尚未开始、已经运行、已提交和结果未知的 call；只有能够证明未提交时才允许重做外部工作。 |
| `AV-F14` Provider-independent Application | Provider adapter 在 epoch attach 时接收一次 System 和 tool catalog，普通 request 只接收 User、history 和 provider cursor。reopen 使用 durable receipt/cursor 重新绑定，不要求 Application 重新生成或发送 System。 |
| `AV-F15` Versioned Reconfiguration | policy 或 capability 变化通过显式的新 epoch 完成。新 definition 经过校验和 durable admission 后原子激活；旧 call 继续绑定旧 epoch。失败、重试和 reopen 不能造成重复 System 或半激活配置。 |
| `AV-F16` Inspection and Replay | runtime 暴露稳定的 turn/call/epoch identity，并记录 preparation、request、stream、tool、effect、publication、flow、failure 和 cancellation。迁移测试可以用这些记录比较 legacy 与 mounted runtime 的外部行为。 |

## 5. 当前实现评估

### 状态口径

| 状态 | 含义 |
| --- | --- |
| `implemented` | 主要行为已经通过推荐 public boundary 提供，并有可执行测试。 |
| `partial` | public API 已覆盖一部分行为，但 Feature 的关键路径仍不完整。 |
| `local-proof` | process-local host 或 isolated harness 已证明行为，不能据此声明 production 支持。 |
| `internal-proof` | private runtime 和测试已有实现，但尚未形成消费者可用的 public contract。 |
| `missing` | 目标行为还没有可执行实现。 |

Feature 状态描述 AgentView 本身，不等于消费者已经完成接入。例如 `AV-F02` 已实现，
但 Cube Stage 仍可能因为使用旧 API 而不能消费它。

### 当前能力矩阵

| Feature | 当前状态 | 已有实现 | 仍缺少 |
| --- | --- | --- | --- |
| `AV-F01` Application Definition | `local-proof` | Public `DurableSystem`、`DurableEpochDefinition`、`MountedFeature` 和 local mounted owner 已证明 Create 时只 render/attach 一次 System，reopen 不执行 System/User POM；external consumer test 证明 feature 可按作者顺序同时贡献 durable System/runtime 和 per-turn User POM。`#[view(component)]` 现在把同一 retained component scope（含可选 key）投影到 durable System 和一棵 fresh User subtree；black-box coverage 包含 positional siblings、duplicate key、keyed Create/reopen、key drift 和 keyed composed parent。 | production owner、真实 provider transport、至少一个生产消费者；async capture 仍属于 host boundary。 |
| `AV-F02` Typed Application View | `implemented` | Public POM AST、`AgentView` derive、typed Markdown/XML composition、role resolution、canonical renderer 和 compile-fail tests。 | 继续收敛 legacy rendered-string API；完成消费者迁移。 |
| `AV-F03` Semantic View Update | `implemented` | Public `DiffSlot`、多种集合 diff、full/delta/delete、`UserDocumentCursor` 和失败回滚；AgentView daemon-backed Chess CLI/skill 已实跑 `System attach/ack -> full Actionable ack -> full Passive -> delta Actionable`，证明 exact delivery ack、base receipt 和 Passive non-advancement，但只覆盖 daemon 存活期间。consumer 丢失 baseline 时会 tombstone 当前未确认 delta，只 full-resync 下一张 User，不重发 System；确认该 full 后恢复 delta。Forgotten City SQLite integration 另证明该 lineage 的 durable reopen/replay；provider Chess 证明严格 semantic gate 与 publication 成功后才推进 baseline。 | replacement logical-consumer/transport handoff policy，以及真实 remote provider 丢失 baseline 后的 resync trace。 |
| `AV-F04` Turn Composition | `implemented` | `AgentViewModel` 可组合 context、artifact、feedback 和 task；mounted capture 会为 preparation/continuation 重新捕获 owned turn props；`Component`/`DurableComponent`/`DurableSystem` 的 pure borrowed `.project_props(...)` 已由 external factory/dispatcher test 覆盖；`MountedFeature` 可组合 durable System/runtime 与 retained per-turn User tree，并通过 `map_channels` 把 feature-local contract 提升到 harness root；macro-created feature scope 及 key 同时保留在 System/User projection，keyed parent 只包裹多个 User child 一次，drop/reopen 不执行 System/User renderer。 | 将 real AgentLoop 从 compatibility authoring 迁到 mounted User root；保持 capture/provider/store 为 host-owned I/O boundary；在 API freeze 前完成消费者和独立 API review。 |
| `AV-F05` Action Surface and Validation | `partial` | Typed XML contract、streaming handler、provider tool schema、turn props 与 dispatcher declaration 已存在。 | 真实消费者的 authoritative revalidation、stale action feedback，以及统一稳定的 mounted action authoring surface。 |
| `AV-F06` Structured Streaming | `local-proof` | XML `open/stream/complete`、strict EOF、fresh reducer state、typed diagnostics 与 chunk-boundary tests 已实现；Forgotten City mounted SelectIntent 已用同一条 wire/parser/reducer path 做 consumer-shaped proof。Chess AgentLoop 进一步证明 open 时实时 Live，且 exact-envelope/one-Output/no-Diagnostic gate 通过后才生成 Commit；非法 content、第二个 move 或 surrounding prose 会撤回 Live，且不进入 publication/outbox。SQLite/OpenAI host 的 recording trace 还证明已接线到 `PlayerRuntime`。 | 迁移 Phrase 的增量 reducer 与 compensation，并补齐真实 Chess provider 的失败/cancellation trace。 |
| `AV-F07` Provider-native Tools | `local-proof` | Grouped tool catalog、ordered dispatch、correlation、expected error、attempt-local replay/collision 已有 executable proof；author 通过纯 `ProviderCapabilityContract` 声明 prompt/schema，host 通过独立版本化的 `ProviderDispatcherRegistry` 绑定实现。统一 `MountedHostBindings<C, TurnProps>` 已进入 generic factory open contract；local/durable Create 与 reopen 会在 System render/attach/rehydrate 和 dispatcher construction 前验证 missing/version/schema/implementation drift。dispatcher-instantiating builders 已移出普通 `component::prelude`，compile-fail test 锁定 component author 不能绑定 async host I/O。 | 真实 Provider/Cube/Forgotten City adapter 接入、跨 attempt/durable replay policy，并将旧 dispatcher-carrying builders 明确收进 compatibility/advanced 生命周期。 |
| `AV-F08` Effect Lifecycle | `partial` | Typed Output/Live/Commit/Diagnostic；public local host 已证明 awaited Live、cancel compensation，以及 continuation/replay/reopen 使用 fresh Live runtime；Chess provider loop 已证明 semantic rejection 会实际撤回 attempt-local Live，并把有效 strict streaming Commit 原子发布到 replay-safe typed outbox。Forgotten City 的 SQLite host 以 stable `OutboxItemId` 幂等应用 player Commit，并在同一 domain transaction 更新棋盘、写 dedup receipt、创建 Stockfish job；player/engine worker 均有 lease fencing、retry/dead-letter，engine move 与 job completion 原子提交。 | 常驻 recovery-scanned supervisor、passive engine-failure observer、child effect scope 和 authoritative production effect adapter。 |
| `AV-F09` Reactive Observe/Act | `partial` | Public compatibility `AgentViewApp` 仍支持 `observe/hook/act_with_sink`。advanced `MountedExternalHarnessDefinition::new(root, reply, epoch_id)` boundary 现在消费普通 `PromptComponent` root，并在 epoch finalization 前绑定 shared typed `ExternalReply<Contract>`；普通 component 类型没有 external mode 或 finalizer，也不存在 per-component `ExternalPromptComponent`。Actionable/Passive 由 host 与 source revision/props 一起捕获在 `ExternalObservation`，POM renderer 不再选择 delivery lane；同 observation identity 的 kind flip 与未执行的 provider/streaming runtime 在 Create/reopen 均 fail closed。AgentView's canonical runnable reference is `agentview chess`: its loopback daemon exposes `attach/attach-ack/observe/ack/act/hook/resync`, returns `kind: "chess_frame"` with protocol data in nested `frame`, and proves System/User acknowledgement, exact action-handle raw XML, Passive non-advancement and full/delta/resync while the daemon remains alive. Daemon exit discards that state. Forgotten City's concrete SQLite port remains the consumer integration proof for durable logical-consumer identity, replay/collision, reopen and storage semantics, not the CLI example entrypoint. | managed remote/server transport、bounded receipt retention、process-kill recovery、replacement-consumer/transport handoff，以及把 generic provided reply/streaming component 的 runtime binding 统一到 AgentLoop/external 两种 harness。 |
| `AV-F10` Model-backed Turn Loop | `implemented` | Compatibility `Agent` 支持 transactional turn、context replacement、bounded Wait/Continue loop、observer 和 rollback；public local mounted call 已证明按 harness-owned `TurnLoopPolicy` 执行 Continue，并为每轮重新 capture/render User。`MountedCallInput::with_turn_cap` 只能收紧上限；Forgotten City selector 显式保留三轮预算。上限进入 epoch manifest 与 artifact fingerprint，changed-policy reopen 会成为 `ContractMismatch`，不会重发 System。 | mounted production loop 替换 side-effecting compatibility `commit_turn`，并明确 caller 消失后的 durable continuation ownership；若要支持 policy migration，需定义独立于普通 epoch reconfigure 的 owner-policy transition。 |
| `AV-F11` Session, Fork and Isolation | `partial` | Compatibility `Agent` 的 fork/cursor behavior、public `AgentSession` 和 factory-scoped local mounted isolation 已测试。 | mounted child-session creation、parent/child cancellation ownership、session-id scoped production registry 和真实多消费者隔离测试。 |
| `AV-F12` Safe Call Lifecycle | `local-proof` | Public local mounted facade 已证明 admission、same-input replay、input mismatch、dropped wait、call-scoped cancel 和 joined cleanup；`start` future 在 admission 前后被丢弃会释放 reservation。provider exit 显式携带 `Unchanged`、`ResumeFrom` 或 `Indeterminate` cursor authority；managed owner 会校验 requested/provider reason，并在 Provider/Live cleanup join 后提交 exact session/epoch/call/input/turn/request/lease/revision settlement。`Unchanged` 会写入 `Stopped` 并放行 successor；`ResumeFrom` 会原子保存 cursor，live successor 与 reopen 都使用该值；`Indeterminate`、reason mismatch、provider join timeout、stale revision、pending publication、epoch mismatch、invalid stored/replacement cursor 和 expired lease 都有 recovery proof；foreign lease/request 会保持原 Running fence 不变。`MountedAgent::lookup` 现在公开投影一个无 lease 的 durable call snapshot，覆盖 create/reopen 的 settled 查询，并从同一 owner 实例即时报告已 admission 的活跃进度。同一 owner 可在旧 handle 丢失后重新签发唯一 attached handle；重叠或跨 owner reattach 只返回只读 observation。真实 process-local `InMemoryMountedStore` 的 20 个直接测试覆盖 cancellation、recovery、provider binding 与 fault parity，并证明 same-fence recovery retry 不再次推进 revision；`NeverAccepted` reconciliation tests 证明强 negative provider proof 只能在 exact session/epoch/call/input/turn/request/lease/revision fence 且无 pending publication 时恢复 New/continuation checkpoint，legacy missing-origin 保持 fenced，并覆盖 durable-backend CAS 持久化。借用式 raw execution 仅在单元测试中编译且模块私有，production entry 必经 detached owner。Forgotten City runtime tests 另证明 stale Live delivery 不会复活已清理选择。 | 仍缺 fenced 跨 owner/process recovery/control、public provider-operation controller/supervisor wiring、production lease renewal/deadline policy、concrete durable store，以及真实 transport cancellation adapter。 |
| `AV-F13` Durable Application Lifecycle | `partial` | Private state machine 覆盖 call ledger、lease、CAS、atomic session/outbox publication、indeterminate resolution 和 recovery；advanced public `DurableMountedAgentFactory` 已通过 external backend proof。Forgotten City `SqliteMountedStateOutboxBackend<P>` 把 opaque mounted state 与 typed JSON Commit rows 放进同一 SQLite transaction；durable Chess repository 另以 `(session, OutboxItemId)` 去重 player apply，并原子创建/完成 fenced Stockfish job。其 SQLite consumer integration 同库持久化 System/User delivery、ack、opaque controller CAS、reply ledger 和 wake，并在同一 logical consumer 的进程重开后恢复 delta lineage；它不是 public CLI entrypoint。测试覆盖 apply-before-ack crash、lease expiry/stale worker、retry/dead-letter、poison-row quarantine、pending/delivering capture barrier、dead-letter recovery barrier、reopen、User-only resync 和 v3 delta continuation。 | recovery-scanned 常驻 supervisor、真实 process-kill fault matrix、server installation、public id-keyed recovery/control 与 durable reconfiguration。host 的 typed outbox fingerprint 仍必须采用 versioned canonical encoding。 |
| `AV-F14` Provider-independent Application | `partial` | Public durable provider contract 区分 epoch attach、ordinary request 和 POM-free rehydrate，并持久化 provider receipt/cursor；AgentView coordinator 与 Forgotten City stateful adapter 均证明 lost attach reply resume。recording OpenAI Conversations/Responses HTTP server 已证明一次 System-bearing conversation Create、同 conversation 上 ordinary User-only full 后 delta、独立 operation idempotency key，以及 replacement host 不二次 attach System 且继续 v3 User delta。 | 用真实远端凭据重复 Chess trace、验证 remote baseline 丢失后的 `ResyncUserDocument`，并由服务端强制 durable epoch/artifact idempotency key。 |
| `AV-F15` Versioned Reconfiguration | `internal-proof` | Private owner 已证明 admission、tombstone、activation、recovery、superseded state 和 POM-free retry；public local lifecycle test 证明同一 `EpochContractId` 下的新 POM authoring 值不会替换已安装 System。 | public submit/query/watch contract；local host 当前明确拒绝 reconfigure；production retention policy；`EpochContractId` 的人工版本责任仍需在生产 migration 中执行。 |
| `AV-F16` Inspection and Replay | `partial` | Compatibility loop 有 preparation/request/assistant/failure/flow observer event；internal mounted runtime 保留稳定 call/epoch/publication identity，public facade 现在提供 lease-free call snapshot（含 terminal replay proof 或 recovery category）；Forgotten City SelectIntent 已比较 legacy/mounted 的 System、initial/retry User prompt bytes，以及双方接受的 XML callback ordered Output/Live trace。 | public epoch/turn lookup beyond the call snapshot、统一 mounted trace、invalid-input/abort/tool/world/durable side-effect golden harness，以及其他消费者的 equivalence tests。 |

### 当前最成熟的部分

- typed Application View：Rust state 到 POM，再到 canonical Markdown/XML；
- semantic full/delta/delete，以及只有成功 turn 才推进 View baseline；
- compatibility provider-backed turn loop 和 external `observe/hook/act` loop；
- component authoring、typed streaming reducer 和 provider-native tool dispatch；
- process-local mounted lifecycle，包括 replay、reload、Live compensation 和 joined cancel；
- durable call/publication/reconfiguration 的内部状态机和故障测试。

### 当前不能声称的能力

- 不能声称 AgentView 已提供 production durable host；
- 不能声称 reopen 已在真实 Provider 上物理复用同一个 System session；
- 不能声称 Chess Commit/Stockfish 已形成 production 服务；本地 SQLite 已证明 stable
  item-id 幂等 domain apply、durable engine job 和 worker fencing，但仍缺常驻 supervisor、
  passive failure publication 与 server/CLI 安装；
- 不能声称 Cube Stage 已受支持；Director 的 POM API migration 已编译通过，但 provider/ToolServer/mounted migration 尚未完成；
- 不能把 Forgotten City compatibility AgentLoop 编译通过描述成 mounted production migration；
- 不能把 fake stateful provider 或 opt-in `PlayerRuntime` branch 描述成真实 provider/production install；
- 不能声称所有 provider-started cancel 都可直接继续：只有 joined cleanup 加可验证的
  `Unchanged`/`ResumeFrom` 才能停止 call；不确定或冲突路径必须保持 `RecoveryRequired`；
- 不能把 semantic graph snapshot persistence 描述成完整 provider/tool turn recovery；
- 没有完整 consumer golden trace 时，不能声称迁移保持了全部外部行为。

### 最近一次双向核对

2026-08-01 在同一 AgentView source snapshot 上执行：

| 核对对象 | 命令 | 结果 |
| --- | --- | --- |
| AgentView default workspace | `cargo test --workspace --all-targets --quiet` | 通过；library 301/301，默认 integration、compile-fail 与 examples 全部通过。 |
| AgentView durable backend adapter | `cargo test --test component_durable_backend --quiet`、`cargo test --test component_durable_factory --quiet` | 3/3 与 4/4 通过；external backend proof 覆盖 CAS conflict retry、atomic outbox、replay no-op、cross-factory reopen、System-once、cursor rehydrate、provider registry manifest/version validation、replacement-owner v3 User delta、v2 full-resync migration，以及默认 reducer failure 的 recovery fence/no-outbox。 |
| AgentView real local-store/backend parity | `cargo test component::local_mounted::tests --lib` | 20/20 通过；除 cancellation/recovery fault parity 外，还覆盖 owner-local User cursor persistence、malformed backend 的 unchanged-generation rejection、v1 blob migration rejection、mutation/provider success/provider error 的 canonical final fingerprint，以及 exact-fence `NeverAccepted` checkpoint restore 与 durable-backend persistence。该实现和 test backend 均不是 production persistence。 |
| AgentView mounted owner | `cargo test component::mounted_agent::tests --lib --quiet` | 127/127 通过；覆盖三种 cursor disposition、reason mismatch、provider timeout、revision/publication/epoch/cursor/lease conflict、foreign fence、User-only resync、pure provider registry binding 与 exact-fence recovery retry 幂等。 |
| AgentView public mounted consumer | `cargo test --test component_public_authoring --quiet` | 38/38 通过；同一 `EpochContractId` 的 reopen 保留首次已安装的 System，ordinary/durable feature 的 props projection 会抵达 binding/provider-dispatcher context；`MountedFeature` composition、cross-channel mapping、feature-specific POM-free reopen、positional/duplicate-key behavior、keyed composed parent，以及 keyed feature 在 reopen 的 stable `BindingId` 均有覆盖；key 变更但复用同一 epoch contract 会在 attach 前被拒绝。pure provider contract/host registry 的 missing/version/schema/implementation drift 也会在 System render/attach 或 dispatcher construction 前失败。fallible User authoring 会在 preparation 失败且不会启动 provider。public cancellation 同时证明 `Indeterminate -> RecoveryRequired`、invalid `ResumeFrom` recovery、`Unchanged -> Cancelled -> successor admitted`，以及有效 `ResumeFrom` 到 successor/reopen 的 cursor preservation；per-call cap 只能收紧 harness loop policy，改变 durable policy 的同-id reopen 会在 System rerender 前被拒绝。 |
| AgentView mounted external controller | `cargo test --test component_external --quiet` | 9/9 通过；fake atomic host port 覆盖 System-once、host-owned immutable User outbox、receipt/bytes exact replay、ack-before-act、first-full/ack/delta、passive full/non-advancing、安全取消后的 full、User-only resync 后的一次 full 与确认后恢复 delta、typed reply decode、source stale、indeterminate recovery/race，以及 non-advancing CAS generation 的 fail-closed recovery。v4 state 会被 v5 contract 明确拒绝；该 port 仍不是 production database 或 transport。 |
| AgentView mounted Hello World | `cargo run --example hello_world`、`cargo test --example hello_world --quiet` | 通过；prompt-only 主文件用一个 `hello_agent` component、`PromptComponent<Props>` 和 `prompt_component(system, |props| user)` 定义 typed System/User POM，不再让初学者命名 `NoTurnChannels`、`DurableSystem` 或 `UserTurnContext`。example test 直接断言一份 System 与两份 freshly rendered User。example host plumbing（包括 direct-props capture）被隔离在共享的 `examples/support/mounted_prompt_trace.rs`，不作为 author API；Hello 路径刻意不配置 token script。独立 full-streaming rewrite 虽可编译，但因未观察 Live/Output 而被评审拒绝作为 Hello World。 |
| AgentView mounted chess streaming | `cargo run --example chess_agent_mounted_turn`、`cargo test --example chess_agent_mounted_turn --quiet` | 通过；窄 author prelude 声明 durable System/User、typed XML contract 与同步纯 reducer。example host 将 `e2e4` XML 拆成两段送入 public `FallibleProviderWirePort`；测试断言 `LiveApplied` 位于第二段 submit/ack 之间，并从 `MountedCallOutcome::Executed.records` 读取相同 typed Output。单轮让该例只增加 streaming 概念；每轮 fresh User POM 已由 Hello World 覆盖。该证据覆盖 public local streaming，不代表完整 chess `observe -> act -> hook` 或 production provider。 |
| AgentView mounted chess authoring | `cargo run --example chess_engine_mounted` | 通过；独立 consumer review 将主路径收敛为一个 `chess_agent` component、一次 `mount` 和两次 `run_turn`。该 prompt-only component 通过 `try_prompt_component` 复用既有 fallible chess User-document builder，与 Hello World 保持同一作者形状。同一 chess POM 在 `e2e4` 前后捕获两份 board snapshot，mounted trace 只 attach 一份 786-byte System，生成 5124/4100-byte User。完整 `observe/ack/act/hook` 的 canonical runnable reference 是 AgentView `agentview chess` CLI/skill；本例仍刻意只教 prompt authoring。 |
| AgentView mounted chess compatibility | `cargo test --example chess_engine_mounted --quiet` | 2/2 通过；legacy `ChessViewModel` 的 byte-for-byte golden 已移入 `#[cfg(test)]`，继续验证 mounted System/User 输出而不干扰作者阅读主线。 |
| AgentView daemon Chess CLI + skill | `cargo build --bin agentview`、`target/debug/agentview chess attach`、`cargo test --test agentview_cli --quiet` | canonical playable external reference。loopback daemon 可让独立 CLI subprocess 共享 in-memory Chess state；它的 `kind: "chess_frame"` nested `frame` surface 覆盖 System attach/ack、explicit User ack、exact action handle/raw XML、Passive、full/delta/resync。daemon exit 后状态不会持久化。`chess_engine_mounted_external` 的 9/9 tests 继续验证同一 local host transaction shape，CLI tests 为 4/4。 |
| Forgotten City durable Chess integration | `cargo test -p engine player::mounted_chess -- --nocapture` | 20 passed、1 ignored；这是 SQLite/production-shaped consumer integration，而非 public example entrypoint。它覆盖 durable logical consumer、System attach/ack、exact action handle/raw XML、full/Passive/delta、receipt/reply replay、active-delta tombstone、User-only full resync、确认后恢复 delta、wake 边界与 SQLite reopen。真实 OpenAI facade/config 仅用于 consumer integration 和测试；本次未提供远端凭据。 |
| AgentView mounted provider Chess | `cargo test --example chess_engine_mounted_agentloop --quiet`、`AGENTVIEW_STOCKFISH_BIN=/usr/games/stockfish cargo run --example chess_engine_mounted_agentloop --quiet` | 7/7 与真实 Stockfish run 通过；一份 System、首轮 full、第二轮 committed delta、typed Commit/outbox replay 均可观察。semantic-invalid content、multiple move 和 surrounding prose 会补偿 Live，且 final publication 计数保持为零；reject-before-baseline 保持 full，reject-after-success 保持旧 delta baseline。provider 仍为 scripted。 |
| AgentView component compile-fail | `cargo test --test component_compile_fail --quiet` | 31 个 case 通过。 |
| AgentView public boundaries | `cargo test --test component_public_compile_fail --quiet` | 9 个 external compile-fail case 通过：默认 feature 无法导入 raw IR、durable provider 不能走 ordinary attach、component/root prelude 不提供 legacy turn bridge、mounted host integration、external control 或 async provider-dispatcher binding，并限制 stateful-provider rehydrate 读取 System。 |
| AgentView raw-IR compatibility | `cargo test --all-targets --features raw-component-ir --quiet` | 通过；legacy raw IR tests/examples 与默认 contracts 同时编译。 |
| AgentView POM compile-fail | `cargo test --test pom_compile_fail --quiet` | 22 个 case 通过。 |
| Forgotten City agents | `cargo check -p engine --all-targets` | 无 warning 通过；mounted OpenAI adapter 仍保留 `dead_code` allow，因为默认 engine 不会隐式安装它。 |
| Forgotten City engine local suite | `cargo test -p engine --lib --quiet` | 99 passed、1 ignored；mounted Chess 证明 OpenAI conversation System-once、replacement-owner v3 `full -> structural delta`、invalid reply 不推进 cursor/outbox、pending/delivering Commit 在 User render/provider I/O 前阻断、dead-letter reopen 后进入 recovery。SQLite domain/worker tests 证明 stable `OutboxItemId` 幂等 apply、apply-before-ack crash replay、Stockfish job lease fencing/expiry/retry/dead-letter、poison payload/metadata quarantine、unknown-status fail-closed admission 和 per-session ordering。ignored 的 adapter test 已通过本次真实 `/usr/games/stockfish` CLI trace 覆盖；真实远端 OpenAI、常驻 supervisor 与 server 安装仍未完成。 |
| Forgotten City mounted SelectIntent | `cargo test -p engine mounted_select_intent --quiet` | 21/21 通过；包含 cancel/join/stale-delivery withdrawal、`Indeterminate` cancel 进入 recovery 后对 different call-id successor 的 admission fence、durable definition 的三轮 loop policy、drop/reopen no-second-System，以及 legacy/mounted System、initial/retry User POM 和 accepted-stream callback trace golden comparison。 |
| Forgotten City stateful provider scaffold | `cargo test -p engine mounted_provider --quiet` | 8/8 通过；fake remote operation ledger 额外覆盖 Running inspect、remote cancel joined `Cancelled::Unchanged`，以及 Completed operation 不被 cancel 改写。 |
| Forgotten City semantic graph | `cargo check -p agent_runtime --all-targets` | 通过。 |
| Forgotten City semantic graph local suite | `cargo test -p agent_runtime --lib --bins --test embedding_provider_client --test node_card_contract --test node_card_provider_client --test query_application --test query_tools --quiet` | 210/210 个 database-free tests 通过；PostgreSQL tests 未计入。 |
| Cube Stage | `cargo check --all-targets` | 通过（2026-07-31）；Director 已通过 current POM System/User path 和 `AgentViewValue` 编译。`cargo test --lib --quiet` 亦为 80 passed、18 ignored。 |

这组结果只证明 public local lifecycle 和消费者编译边界，不替代 production Provider、数据库、
restart 或 golden-trace tests。

## 6. Consumer 需求映射

`P0` 表示该消费者进入 production 前必须具备；`P1` 表示首轮迁移后需要；`-` 表示当前
没有明确需求。这里表达的是需求，不表达当前已经支持。

| Feature | Cube Stage | Forgotten City agents | Semantic graph |
| --- | --- | --- | --- |
| `AV-F01` Application Definition | P0 | P0 | P0 |
| `AV-F02` Typed Application View | P0 | P0 | P0 |
| `AV-F03` Semantic View Update | P0 | P0 | P0 |
| `AV-F04` Turn Composition | P0 | P0 | P0 |
| `AV-F05` Action Surface and Validation | P0 | P0 | P0 |
| `AV-F06` Structured Streaming | - | P0 | - |
| `AV-F07` Provider-native Tools | P0 | P0 | P0 |
| `AV-F08` Effect Lifecycle | P0 | P0 | P0 |
| `AV-F09` Reactive Observe/Act | - | P1 | P0 |
| `AV-F10` Model-backed Turn Loop | P0 | P0 | P0 |
| `AV-F11` Session, Fork and Isolation | P0 | P0 | P0 |
| `AV-F12` Safe Call Lifecycle | P0 | P0 | P0 |
| `AV-F13` Durable Application Lifecycle | P0 | P0 | P0 |
| `AV-F14` Provider-independent Application | P0 | P0 | P0 |
| `AV-F15` Versioned Reconfiguration | P1 | P1 | P1 |
| `AV-F16` Inspection and Replay | P0 | P0 | P0 |

### Cube Stage

Cube Stage 的核心形状是一个长期运行的 Director Application：System 固定导演规则和 tool
surface；每个 User turn 提供最新 stage、timeline、角色和任务；LLM 连续调用工具，直到
`complete_task` 或 `finalize_dialogues`。因此它最依赖 native tools、bounded loop、action
idempotency 和 durable publication。

Cube Stage Director 已完成第一步 POM authoring migration：System policy 和 per-turn projected
state 分别由当前 `build_system_document`/`build_user_document` 生成，并通过当前 AgentView
surface 编译和库测试。它仍不是 mounted Application：当前 Director sink 仍在 turn commit 前
顺序执行 `ToolServer` 并写 SQL observer event。后续迁移必须明确哪些写入属于可补偿 Live、
哪些属于 publication-gated Commit，不能只把旧 sink 包进新 facade。

### Forgotten City agents

Forgotten City 的关键形状是实时、可取消的 Player/NPC/GM Application：Application View
来自最新 world snapshot；SelectIntent 和 Phrase 在 provider stream 中产生；部分结果可能立即
启动 phraser 或 option 工作，但在最终 publication 前仍是 tentative。

现有 compatibility AgentLoop 可以编译并通过本地测试；Player SelectIntent 也已有 mounted
component、pure reducer、typed Live/compensation、opt-in `PlayerRuntime` branch，以及
SQLite-backed OpenAI Conversations/Responses host。recording-server trace 已验证它从一次
System attach、User-only request 到 PlayerRuntime 的可见选择；production mounted migration
仍未完成。第一个迁移目标应保持为 Player SelectIntent，因为它同时覆盖 typed View、structured
streaming、Live effect、cancellation、stateful provider session 和 durable call lifecycle。
一个 selector turn 还会 fork 多个并行 phraser work；新的 batch 到来时，parent 与全部 child
必须按 scope 取消并完成补偿。SelectIntent 可以先针对 captured intent pool 做 pure validation，
但真正向 world 提交前仍要执行 authoritative revalidation。

### Semantic graph

Semantic graph 的关键形状是数据密集型 Application：LLM 观察 graph/source workspace 的
full 或 delta View，通过 read/query/mutation/commit action 多轮推进，并保留 provenance 和
snapshot identity。它最适合验证大型 semantic diff、external observe/act 和 durable mutation。

当前 compatibility 路径的 database-free 行为较完整，但 PostgreSQL 集成仍受环境门控；已有
graph snapshot 不能替代 provider/tool turn 的 durable recovery。mounted migration 还需要
统一 action/effect publication 和 cross-runtime golden trace。当前 persistent external loop
使用 `AgentViewApp` 完成 `observe`/`hook`，但 consumer-owned `act` 直接执行 action 后再次
`observe`，没有经过 `act_with_sink` 的 turn consumption；因此 stale-action fencing 仍需独立验收。

## 7. 实现路线

实现顺序按风险闭环安排，不按内部模块数量安排。每个阶段都必须形成一个通过 public boundary
的可执行 Application vertical slice。

| 阶段 | 目标 | 主要 Features | 完成条件 |
| --- | --- | --- | --- |
| `M0` | 固定产品语义与测试口径 | 全部 | 本文成为顶层 Feature 索引；底层 roadmap 和 consumer matrix 反向引用 Feature ID。 |
| `M1` | 收敛 mounted authoring surface | `AV-F01`-`AV-F08`, `AV-F10`-`AV-F12` | 一个 public example 只用推荐 API 定义 System、User、stream、native tool、Live 和 call loop；无 raw experimental owner 泄漏。 |
| `M2` | 接入 stateful Provider session | `AV-F07`, `AV-F10`, `AV-F12`, `AV-F14` | recording adapter 证明 Create 发送一次 System；turn、retry、Continue、reload 和 reopen 都不重发 System；cancel 能 join provider。 |
| `M3` | 接入 production persistence | `AV-F08`, `AV-F11`-`AV-F13`, `AV-F15` | concrete store 原子提交 session mutation 与 outbox；进程重启覆盖 call claim、pending publication、receipt resolve、delivery retry 和 reconfiguration status。 |
| `M4` | 迁移 Forgotten City Player SelectIntent | `AV-F01`-`AV-F08`, `AV-F10`-`AV-F14`, `AV-F16` | `PlayerRuntime` 使用 mounted Application；success、validation failure、provider failure、cancel 和 reopen 的 golden trace 通过。 |
| `M5` | 迁移 Phrase、NPC 与 GM | `AV-F06`, `AV-F08`, `AV-F10`-`AV-F13`, `AV-F16` | Phrase 的 open/append/close、tentative readiness、compensation 和 publication 可验证；NPC/GM 不再使用 legacy prompt assembly。 |
| `M6` | 迁移 Cube Stage | `AV-F05`, `AV-F07`-`AV-F08`, `AV-F10`-`AV-F14`, `AV-F16` | Director/Script Writer 编译并使用 mounted loop；provider retry 不重复 ToolServer I/O；timeline/dialogue golden trace 通过。 |
| `M7` | 迁移 semantic graph 并冻结 API | `AV-F02`-`AV-F05`, `AV-F07`-`AV-F16` | graph query/mutation/snapshot 和 provider-turn recovery 通过；三个消费者完成 compatibility comparison 后再冻结 public API。 |

### 最近的实现重点

1. 把已有的 public id-keyed lookup 接到 production supervisor，并增加 durable-id
   reattach/recovery；同时把当前 private reconfiguration state machine 接到 backend CAS port；
2. 把已经验证的 SQLite Chess facade 接入 recovery-scanned supervisor/server，补 process-kill
   fault matrix、retention 和显式 logical-consumer/transport handoff；
3. 收敛 AgentLoop `chess_feature` 与 external harness binding 为一个 retained generic
   provided-component source，再用 Forgotten City Player SelectIntent 验证 author、provider、
   persistence 和 action ingress，
   而不是先增加新的 authoring DSL；
4. 把现有 one-shot player/Stockfish worker 接入 recovery-scanned 常驻 supervisor、wake 与
   passive failure observer，并建立真实 process restart/golden trace；
5. 在第一个 production migration 完成前保持 public mounted API unfrozen。

### 暂不扩大的范围

- 通用 planner、reflection、RAG、long-term memory 产品；
- 隐式 Agent-to-Agent message bus 或共享 memory；
- 内置特定 Provider SDK；
- 内置游戏、graph 或数据库业务模型；
- 在真实消费者验证前增加新的 authoring DSL 层。

## 8. 逐项验收计划

每个 Feature 使用一个稳定的 acceptance ID。底层可以有很多 unit test，但顶层结论只引用
这一条行为验收。`当前证据` 是截至本文 review date 的事实，不代替下一次 release run。

| Acceptance ID | Feature | 端到端验收场景 | 当前证据 |
| --- | --- | --- | --- |
| `AV-A01` | `AV-F01` | Create、两个 turn、context retry、Continue、reload、drop/reopen 全部执行后，System render 和 provider attach 都恰好一次；User 每次需要时重新 capture/render。 | public local lifecycle suite 38/38 通过，包含 fallible User preparation、`MountedFeature` cross-channel mapping、positional/duplicate-key/keyed-parent composition、keyed feature stable `BindingId`/key-change rejection、pure provider registry preflight、invalid resume-cursor recovery、有效 resume-cursor 的 successor/reopen preservation、durable harness-owned loop bound 和专属 reopen 不执行 System/User renderer；production adapter 未测。 |
| `AV-A02` | `AV-F02` | 一个混合 Markdown/XML View 可 canonical render；非法结构在 compile/build/render 边界被拒绝；业务输入被正确转义。 | public runtime、golden 和 compile-fail tests 通过。 |
| `AV-A03` | `AV-F03` | 同一 View 依次产生 full、unchanged、change、insert、remove 和 delete；provider/commit/cancel 失败后再次生成相同未提交 delta。 | public diff/cursor tests 通过。 |
| `AV-A04` | `AV-F04` | task、artifact、feedback 和 context 按 authored order 出现；history replacement 和 Continue 捕获最新 source；被舍弃的 candidate 不创建 handler state。 | compatibility 与 local lifecycle tests 通过。 |
| `AV-A05` | `AV-F05` | 同一 declaration 生成 LLM-visible contract 和 handler；合法 action 被路由；在 capture 后改变权威状态，过期 action 被重新验证并以 feedback 拒绝，且没有 side effect；handler infrastructure fault 终止 attempt。 | XML/native 两条 isolated path 已测；无 consumer authoritative-revalidation test。 |
| `AV-A06` | `AV-F06` | 同一结构用不同 chunk 切分得到相同 ordered events；覆盖 open、append、complete、自闭合、malformed 和 incomplete EOF；retry 使用 fresh state。 | parser、streaming component、local host，以及 Forgotten City mounted SelectIntent 20 项 focused tests（含 legacy accepted-stream callback trace、mounted loop policy 与 reopen no-second-System）通过。 |
| `AV-A07` | `AV-F07` | Provider 连续发出多个 native tool call；结果顺序和 correlation 正确；完全相同 invocation replay 不执行第二次 I/O，冲突 payload 终止 attempt。 | grouped dispatcher tests 通过；真实 Provider adapter 未测。 |
| `AV-A08` | `AV-F08` | Live apply 完成前 provider 不收到 ack；多个 child effect 保留 scope 和顺序；cancel/provider/parser failure 会补偿已执行 Live；Commit 仅在 atomic publication 后进入 outbox，delivery failure 可重试完整 batch。 | Fresh Live runtime、await 和 compensation 已通过 public local test；Forgotten City runtime queue 已证明 cancel join 与 stale-delivery fence；child scope 与 Commit/publication 尚无 public production proof。 |
| `AV-A09` | `AV-F09` | `observe` 返回稳定 full snapshot；capture 期间 awake 会丢弃 stale candidate；旧 turn 的 `act` 被拒绝；`hook` 只在新 epoch 返回。 | public `AgentViewApp` tests 通过；partial patch 未实现。 |
| `AV-A10` | `AV-F10` | 单个 logical call 完成 Wait 和多次 Continue；每轮 action result 进入下一轮；达到 loop/context-preparation budget 时确定性终止。 | compatibility loop 与 public local two-turn continuation 通过；harness policy、caller tightening 和 over-budget termination 均有 external test，Forgotten City selector 在 mounted harness 上保留 legacy 三轮预算；production continuation recovery 未测。 |
| `AV-A11` | `AV-F11` | 从同一 committed baseline fork 多个 child work；parent 与 child 的 history、View baseline、tool state 和结果互不污染；取消一个 scope 不终止无关 scope；reopen 只恢复目标 instance。 | compatibility fork 与 factory local isolation 通过；mounted child-session 和 production registry 未测。 |
| `AV-A12` | `AV-F12` | start 后丢弃 waiter 不取消工作；same call/input 返回 replay；same call/different input 被拒绝；cancel 等待 provider 和 Live cleanup join 后才完成；只有可证明 cursor 连续性的 exact Running lease 才停止并放行 successor，其他情况进入 recovery。 | public mounted black-box tests 与 start-future drop guards 通过；owner tests 已覆盖三种 cursor disposition、reason mismatch 和 timeout，真实 local store 的 fault tests 覆盖 revision、publication、epoch、stored/replacement-cursor、lease、foreign-fence fault 和 exact-fence recovery retry。id-keyed public control、production persistence 与真实 transport adapter 尚未验证。 |
| `AV-A13` | `AV-F13` | 在 admission、provider start、publication write、receipt loss 和 outbox delivery 各阶段杀死进程；重启后不丢 committed state、不重复不确定 I/O，并最终投递 outbox。 | external durable-factory test 已覆盖 conflict retry、atomic outbox、cross-factory reopen、System-once 与 v3 persisted-cursor delta continuation；Forgotten City SQLite proof 覆盖 apply-before-ack replay、stable item-id dedup、engine lease fencing、retry/dead-letter、capture admission 和 poison quarantine。仍缺真实进程 kill/restart、常驻 recovery supervisor、passive failure observer 和 production install。 |
| `AV-A14` | `AV-F14` | recording Provider 在 Create 收到一次 System/tool catalog；ordinary turn、retry、Continue、reload 和 reopen 均无 System bytes，并使用 receipt/cursor 恢复。 | AgentView contract/local rehydrate 与 lost-attach-reply resume test、Forgotten City fake remote 的 4 项 stateful adapter tests 通过；其中一项驱动真实 mounted selector composition path，真实 production transport 尚未测试同一 failure path。 |
| `AV-A15` | `AV-F15` | 新 epoch 在并发 call、owner crash、admission retry 和 activation failure 下保持原子切换；terminal status 可按 reconfiguration ID 重查，System 最多 render/attach 一次；同一 `EpochContractId` 下改变本地 POM 不替换 durable System，只有显式新 epoch 才替换。 | private owner tests 加 public local same-id System-preservation test 通过；loop policy 已进入 durable manifest，同-id policy drift 会在 System rerender 前以 contract mismatch 拒绝；public reconfiguration API 和 production retention 缺失。 |
| `AV-A16` | `AV-F16` | 同一 consumer fixture 分别运行 legacy 与 mounted path，比较 prompt bytes、stream event、tool transcript、observer trace、world/graph mutation 和 durable side effect。 | `partial`：Forgotten City SelectIntent 已比较 System、initial/retry User prompt bytes 和双方接受的 XML callback ordered Output/Live trace；invalid-input/abort/tool/world/durable side effect 及其他消费者的完整 golden harness 仍缺失。 |

### 推荐执行顺序

1. 先执行 `AV-A02` 到 `AV-A08`，确认 View、action、stream 和 effect contracts；
2. 再执行 `AV-A01`、`AV-A10` 到 `AV-A15`，确认长期 Application lifecycle；
3. 然后运行消费者自己的 build、local behavior 和 database-backed integration tests；
4. 最后执行 `AV-A16`。只有 golden comparison 通过，迁移才算完成。

仓库当前具名命令和最近一次结果记录在
[consumer-feature-matrix.md](consumer-feature-matrix.md) 的“可执行测试清单”中。该文件保留
详细 test receipt；本文只维护稳定的产品 Feature 与验收语义，避免测试数量变化导致产品定义漂移。

## 9. 支持声明标准

“实现了某个 Feature”和“支持某个消费者”是两种不同声明。

| 声明 | 最低证据 |
| --- | --- |
| AgentView 实现 Feature | 推荐 public boundary 可达；对应 acceptance test 通过；失败和取消路径有断言。 |
| AgentView locally supports consumer | consumer 当前版本可编译；consumer-shaped local test 通过；所需 Feature 均至少为 `implemented` 或经该消费者验证的 `local-proof`。 |
| AgentView production supports consumer | 真实 Provider、persistence、effect host 和 restart path 通过；环境门控测试完成；legacy/mounted golden trace 通过。 |

以下证据不足以单独形成支持声明：

- roadmap checkbox 或设计文档；
- 只能从 crate-private owner 到达的 unit test；
- isolated example 或 process-local in-memory store；
- 消费者仅仅能够编译；
- 因缺少数据库或 credentials 而没有执行的 integration test；
- 只比较最终文本、不比较 action 和外部副作用的 snapshot test。

### 当前结论

| 消费者 | 可以声明 | 不能声明 |
| --- | --- | --- |
| Cube Stage | Director 已使用当前 POM authoring surface 编译，且库测试通过。 | 当前不支持 production Cube Stage；尚未迁移 mounted provider/ToolServer loop、durable publication 或 golden trace。 |
| Forgotten City agents | compatibility AgentLoop 的本地编译和行为测试通过；SelectIntent 有 consumer-shaped mounted component/Live/runtime/provider proof；Chess 的 SQLite-backed AgentLoop/external session facade、幂等 domain、durable Stockfish、User-only resync 和 reopen structural-delta 均有 consumer integration 证明。canonical playable CLI/skill 属于 AgentView。 | 尚不支持 production mounted Player/NPC/GM；没有真实远端 provider、常驻 worker supervisor、server 安装、显式 transport handoff、durable id-keyed cancel recovery/control 或完整 golden trace。 |
| Semantic graph | compatibility runtime 的 database-free local path 可用，typed View/delta 和 bounded tool loop 已验证。 | PostgreSQL 测试未完成；mounted provider-turn recovery 和 cross-runtime golden trace 未完成。 |

当并发开发仍在修改 public mounted facade 时，任何测试结果都必须绑定到稳定的代码快照。命中
半写文件或前后 mtime 变化的运行应作废并重跑，不能用于提升 Feature 状态。

## 10. 事实来源

事实优先级从高到低：当前实现与 executable test、当前 public API 文档、reviewed roadmap、
旧设计文档。实现与文档冲突时，以代码为准并修正文档。

- [README](../README.md)：`Application for LLM` 产品定义和当前 runtime 概览；
- [`src/lib.rs`](../src/lib.rs)：推荐 public surface；
- [`src/agent.rs`](../src/agent.rs)：compatibility model-backed loop 与 commit 边界；
- [`src/view_app.rs`](../src/view_app.rs)：external `observe/hook/act` loop；
- [`src/component/mounted.rs`](../src/component/mounted.rs)：opaque mounted Application facade；
- [`src/component/local_mounted.rs`](../src/component/local_mounted.rs)：process-local host 的真实范围；
- [`src/component/provider_wire.rs`](../src/component/provider_wire.rs)：stateful provider epoch 和 turn wire contract；
- [`src/component/publication.rs`](../src/component/publication.rs)：durable mutation/outbox publication contract；
- [POM Component Roadmap](../kanban.md)：目标 lifecycle、实现顺序和未完成项；
- [POM Component Authoring](pom-component-authoring-examples.md)：当前与目标 API 的详细区别；
- [Consumer Feature Matrix](consumer-feature-matrix.md)：消费者 build/test receipt 和具体缺口；
- [`tests/component_public_authoring.rs`](../tests/component_public_authoring.rs)：public local mounted black-box evidence；
- [Forgotten City Player runtime](../../forgotten-city/crates/engine/src/player/runtime.rs)：selector/phraser fork、batch fencing 和取消需求；
- [Forgotten City Player agent](../../forgotten-city/crates/engine/src/player/agent.rs)：SelectIntent/Phrase streaming side effect 的当前行为；
- [Forgotten City persistent graph loop](../../forgotten-city/crates/agent_runtime/src/persistent_loop.rs)：external observe/action 和 snapshot persistence；
- [Cube Stage Director](../../cube_stage/src/director/rig_agent.rs)：native tool loop、completion retry、SQL observer 和 timeline mutation。
