# POM Component 编写示例与评审

Last reviewed: 2026-08-01

## 1. 文档状态与阅读方式

这是一份 P3/P4 API 评审材料，不是当前 crate 的使用手册。文中的目标代码
统一使用 `rust,ignore`：它描述希望最终支持的调用方式，在对应 public API 落地前
不会加入 Cargo 的 `examples/` target。

2026-07-31 durable boundary update: durable authoring now has one complete
binding-owned ordered `DurableSystem<C, Props>`. Its POM-only nodes and
exact-one `DurableComponent` leaves may interleave; Create consumes the POM
projection once and reopen reads only the same tree's runtime projection.
Sections below that use `SystemMountContext -> SystemView` describe the explicit
one-shot compatibility path or older design alternatives, not the durable
owner contract. The canonical current proof is
`durable_catalog_reopens_streaming_and_grouped_native_tools_without_system_reauthoring`.

当前 AgentLoop 兼容路径仍然通过一次 `ComponentHarness::render` 同时构造 System、User 和
`HookPlan`，并在 context preparation 重试时重新编译 component tree。它尚未使用下面的
mounted runtime。隔离的 [`pom_mounted_lifecycle.rs`](../examples/pom_mounted_lifecycle.rs)
已经提供 `SystemView<C, TurnProps>`、`UserView`、一次 mount invocation 的
`MountedEpoch<C, TurnProps>`、`MountedTurn` 和 POM-only `UserTurnPlan`。它保证一次
`mount_system_epoch` 调用只执行一次 System render。公开的
`InMemoryMountedAgentFactory` 已通过 opaque `MountedAgent`/`MountedCall` 接到同一个
authoritative owner，并由外部测试证明 Create、replay、reload、drop/reopen、awaited Live 和
joined cancel；它是 process-local development proof，不是 production persistence host。
crate-private owner 仍负责 epoch replacement、fork 与完整 call pinning；legacy AgentLoop
尚未使用任何 mounted owner。

mounted XML path 已不只是 state-laziness proof：`.state_with(...)` 保存无参 reusable
initializer，`.try_state_with(...)` 接收 typed `TurnBindingCx` 并可失败；每次
`MountedTurn::prepare_user(&props, user_view)` 返回绑定同一份 props 的 `PreparedUserTurn`；只有
它的 `start_streaming_attempt(live_runtime)` 才创建 fresh state、shared parser 和 binding
instances。parser/reducer/init/finish failure 会产生带 binding identity 和 phase 的
terminal `BindingFault`；host Live failure 是独立的 `LiveEffectFault`。两者都保留显式 abort
路径。deferred `#[view(component)]`、`PomView`、
`ProvidedView<B>`、nominal `StreamingValueView<E, D>` 和 `StreamingChannelsView<C>` 的第一阶段
authoring boundary 也已经落地。

这仍不是完整 runtime，但 isolated host boundary 已经可执行：每个 attempt 必须提供 typed
`LiveEffectRuntime`，Live 在 parser callback 内顺序 await；abort 会先补偿同一 live scope，再执行
所有 pure local teardown。Commit 在 publication 前不会出现在 returned update 中。旧的
`publish_with -> PublishedStreamingAttempt -> Commit interpreter` 仍保留为 process-local
phase-ordering/retry proof，不是 production delivery path。

新的 durable path 已提供 `CommitStager<C>`、`PublicationRequestId`、稳定的
`OutboxItemId(request, index)`、versioned contract payload、`PreparedSessionMutation<M>`、
expected-revision CAS `PublicationStore`，以及 rejected/indeterminate/resolve state machine。
`FinishedStreamingAttempt` 与 native-only `FinishedProviderAttempt` 都能在 publication 前 stage
private Commit；durable success 分别返回 `DurablyPublishedStreamingAttempt` /
`DurablyPublishedProviderAttempt`，不再暴露 pending Commit。crate-private publication actor 证明
accepted store command 不会随 caller future 取消，并且 final handle 丢失后会 resolve
indeterminate write；只有 store 明确回答 NotCommitted 才允许 abort。真实 AgentLoop 尚未构造
完整 pure session mutation，也没有 concrete durable store、crash recovery 或 outbox worker。
`PomHarness` 和下文完整 production `MountedAgent` builder spelling 仍是目标草案；当前公开的
local facade 已接通 pure mutation、durable publication、receipt-gated session/revision writeback
与同 epoch `TurnFlow::Continue`，但只支持 `Commit = Never`、process-local reopen，且拒绝
reconfigure。一个额外的 private binding proof 已把 session/epoch identity、store、request-id
policy、attempt factory、typed `TurnRecord` 与 pure reducer 固定在 mount owner；private
snapshot/mutation 也已持久化完整 call ledger。一个 mount-owned `MountedSessionStore` 在每轮
User capture/provider 之前原子 claim `(session, epoch, call, input, turn, request)` 为 `Reserved`；只有
final Ready 且即将启动 provider/tool/Live I/O 时才把同一 lease 提升为 `Running`。预启动失败、取消或
`Reserved` lease 过期会恢复 prior continuation checkpoint（或删除新 call），只有已经 `Running` 的 lease
过期才持久化为 `RecoveryRequired`。最终 publication 必须用同一未过期 Running lease 原子写入
AwaitingContinuation/Settled、session 与 outbox；双 owner 排他、稳定边界 turn-1 resume、terminal
receipt replay 与 ledger retention 都已有 private proof。公开 local host 已把这些能力接到 owned
input、opaque call、replay/reload 与 cancel facade；它仍没有 concrete production store，因此仍不是
production AgentLoop contract。

provider-native path 也已从 mount-plan declaration 变为可执行的 isolated runtime。
`provider_tools[_with_context]` 将多个 schema 绑定为一个 per-attempt dispatcher group；
`PreparedUserTurn::start_provider_attempt` 用于 native-only harness，包含 XML binding 的 harness
则由 `start_streaming_attempt` 创建同一 identity、live scope 下的 parser 与 dispatcher groups。
`dispatch` 返回 model-visible `ProviderToolResult` 以及 typed channel update；Live 会在 result
返回前 await，result 会记录进后续 `TurnPublication`，而 Commit 保持 publication-gated。
`invocation_id` 是 replay identity，可选 `result_correlation_id` 独立保留给 provider transcript；
两者与 payload 完全一致时，同一 attempt 会 replay 缓存结果而不触发第二次 I/O/effect，复用
invocation id 但改变 correlation 或 payload 则是 terminal collision。unknown name、scalar/array
arguments、missing/empty invocation id 和 expected tool/domain error 都是
`ProviderToolResponse::Error`，且不会绕过 runtime replay/publication path。provider-call Live
failure、collision 和 publication failure 都保留可查询的 `ProviderCallIdentity`/results；只有
dispatcher/Live/finish/abort infrastructure failure 才终止 attempt。新的
`ProviderToolCatalog + MountedProviderExecutor + FallibleProviderWirePort` 已定义为独立于 legacy
`TurnSink` 的 contract，managed attempt 也能作为该 fallible port。owner-issued
`ProviderCancellationSource/Token` 与 joined `MountedProviderExit` 已补上：owner 发出 cancel 后必须
继续 await executor exit，不能把 drop future 当 acknowledgement。但真实 provider adapter/
AgentLoop 尚未使用它，跨 attempt 的 replay/idempotency policy 仍属于 durable host。

当前可运行的类型组合示例见
[`pom_component_composition.rs`](../examples/pom_component_composition.rs)。它只展示已经
实现的纯 POM/provided 组合。下面四个 isolated compatibility examples 使用旧的 raw
compiler/attempt IR，必须显式以 `--features raw-component-ir` 运行；它们保留用于迁移和
runtime proof，不能作为默认 component authoring API 的范例。四类 streaming channel 的可运行示例见
[`pom_streaming_channels.rs`](../examples/pom_streaming_channels.rs)；它只分类 emission，
仍使用当前 accumulator，不假装 live/commit interpreter 已经存在。异构 factory 与
provider-native tool 的并行 mount-plan spike 见
[`pom_mount_plan.rs`](../examples/pom_mount_plan.rs)；它不接入 AgentLoop。
可执行的 grouped native dispatch、awaited Live、publication-gated Commit、publication-recorded
result 与 abort
示例见 [`pom_provider_tools.rs`](../examples/pom_provider_tools.rs)；它同样是 isolated runtime，
不代表 `LLMExecutor` 已接入 capability plan。
一次性 System 和独立 User preparation 的 runnable lifecycle proof 见
[`pom_mounted_lifecycle.rs`](../examples/pom_mounted_lifecycle.rs)；它保存完整 System bundle，
但最终 epoch/attempt 次数仍需 AgentLoop owner 强制。reusable XML factory、typed turn props、
shared-parser attempt 与 terminal-failure proof 见
[`pom_mounted_streaming.rs`](../examples/pom_mounted_streaming.rs)：mount 和 User preparation
不创建 state，每个 provider attempt 创建 fresh state，按 wire order await Live，并通过示例
publisher 后才读取 pending Commit。它仍没有把 provider completion、session transaction、
cursor commit 或 abort 接到真实 `AgentTurn`。
durable staging/store 的可运行示例见
[`pom_durable_publication.rs`](../examples/pom_durable_publication.rs)：它在 publish 前把 typed
Commit 编码为稳定 outbox row，并用一个锁模拟 session mutation + outbox 的原子事务；这只是
contract proof，不是 production database implementation。

只使用稳定 `prelude` 的 durable authoring 示例见
[`pom_durable_authoring.rs`](../examples/pom_durable_authoring.rs)。它将 policy POM 和 streaming
component 组合进一个 `DurableSystem`，并明确展示当前 public lifecycle 只允许 isolated
one-shot mount；它不是 durable reopen owner 的替代品。

可运行的跨 System/User feature composition 示例见
[`pom_feature_composition.rs`](../examples/pom_feature_composition.rs)。每个 feature 返回自己的
`MountedFeature`，由 parent 以 `.project_props(...)` 将较大的 owned turn snapshot 投影为 child
props，再按作者顺序组合 durable System/runtime 与 per-turn User POM。feature-specific reopen
proof 见 [`component_public_authoring.rs`](../tests/component_public_authoring.rs)。该示例还通过
`MountedHostBindings::with_provider_dispatchers(...)` 绑定 host-owned dispatcher，并实际驱动两次
native-tool provider turn：它断言 System 只 attach 一次，而每次 turn 都重新 capture/render User
POM 并取得 tool result。这仍是 in-memory host proof，不是 production persistence/provider claim。

阅读时使用三个标记：

- **已确认**：已经由项目方向确定的生命周期或所有权约束。
- **目标草案**：为了讨论 ergonomics 给出的 API 写法，可以在实现时调整命名。
- **待定**：例子暴露出的真实设计选择，集中列在最后一节。

当前原型与目标 API 的关系是：

| 目标概念 | 当前状态 |
|---|---|
| deferred component call | 已实现隔离边界；function call 只保存 owned/`Arc` props，body 在 compiler traversal 时执行一次，尚未接入 mounted System/User split |
| mounted System / per-attempt User | 隔离 proof 已实现；[lifecycle](../src/component/lifecycle.rs) 每次 mount 调用保存 raw/resolved/rendered System 与 factory/capability plans，User 单独编译；[component adapter](../src/component/agent.rs) 尚未迁移，仍每次同时 build 两者 |
| reusable factory + `.try_state_with(...)` | 已实现：无参 `.state_with(|| ...)` 与 typed/fallible `.try_state_with(|cx| ...)` 都只在 final `PreparedUserTurn::start_streaming_attempt` 时运行；`S: Send + 'static` 禁止 state 借用 `cx.props()` |
| output/live/commit/diagnostic channels | `StreamUpdate + ChannelMap` 已支持同一 tag 多 lane；Live 被实时 await，Commit 可在 typed boundary stage 为 durable payload。durable published outcome 不暴露 Commit；真实 session mutation/store/worker 尚未接入 AgentLoop |
| fallible event + acknowledged abort | isolated path 已实现 `BindingFault`、独立 `LiveEffectFault`、async compensation 与 local abort report；另有独立 fallible wire contract 和 managed adapter，legacy [`TurnSink`](../src/llm_call.rs) 保持不变 |
| provider capability plan | 已实现：每个 capability 绑定一个或多个 static tool spec 和 reusable dispatcher factory；epoch 可导出 data-only `ProviderToolCatalog`，final-ready attempt 创建 fresh grouped dispatcher，真实 executor 尚未迁移 |
| `PomView` / `ProvidedView<B>` | 已实现第一阶段；纯 POM 拒绝 runtime binding，provided 仍使用临时 homogeneous `B` carrier |
| `MountProvidedView<C, TurnProps>` | 已实现隔离 runtime；异构 factory/capability 经完整 channel mapping 后拆成两个计划，XML binding 与 grouped provider dispatcher 都可执行 typed/fallible callbacks，尚未替换兼容 AgentLoop API |
| `DurableComponent<C, TurnProps>` | 已实现：一个 durable leaf 同时持有一个 `RuntimeContract`、一次 System POM projection 与一个 factory/capability declaration；crate-private catalog 从同一 leaf 派生首挂载计划和 POM-free rebind projection |
| POM composition、role placement、key/identity、实时 parser callback | 已有原型 |

例子按风险递增排列：静态 System、动态 User、显式 `DiffSlot`、commit-only
streaming、实时 streaming、多个 provided components、compaction/loop、显式
reconfigure，以及 Cube Stage 风格的 provider-native tool loop。评审重点不是语法
是否漂亮，而是每份数据由谁提供、何时创建、何时可以执行 I/O、失败后由谁清理。

项目方向见 [roadmap](../kanban.md)，运行时关系见
[POM Component Runtime](pom-component-runtime.html)。

建议评审者先看 3.6 的组合方式、4.1/4.4 的生命周期与取消、5.1-5.3 的生产迁移，
最后看 7.2 的 API freeze 问题。其余例子用于验证这些结论是否能落成 Rust API。

## 2. 最小目标 API

### 2.0 当前 carrier 过渡状态

2026-07-31 的 mounted authoring 路径已经引入了 `Component<C, Props>`：它是
**POM 与 mount-time runtime declaration 的单一作者侧 carrier**。`SystemView` 也保存这
个 carrier；`StreamingXml::into_component()` 是 streaming 的标准入口。纯 POM child
继续使用 `PomView`，因此它可以在 System 或 User root 下组合，而带 reducer/tool
declaration 的 `Component` 在类型上不能进入 `UserView`。

```rust,ignore
#[view(component)]
fn select_intent() -> Component<PlayerChannels, PlayerTurnProps> {
    StreamingXml::<TurnEmission<PlayerChannels>, PlayerDiagnostic>::new(contract())
        .state_with(SelectIntentState::default)
        .on_open(reduce_select_intent)
        .into_component()
}

#[view(component)]
fn player_capabilities() -> Component<PlayerChannels, PlayerTurnProps> {
    component((
        PlayerIntentRules::new(),
        select_intent(),
        provider_tool("inspect_world", inspect_spec(), InspectDispatcher::new),
    ))
}

fn player_system(
    _: SystemMountContext<'_, PlayerMountProps>,
) -> SystemView<PlayerChannels, PlayerTurnProps> {
    system_view(player_capabilities())
}
```

需要跨进程 reopen 的 provided child 则返回 `DurableComponent`，而不是先构造一个
aggregate `Component` 再补贴一个 contract。一个 durable leaf 只拥有一个 contract，
因此 manifest 和 POM-free rebind 不会分别从两份 authoring 代码生成：

```rust,ignore
#[view(component)]
fn select_intent() -> DurableComponent<PlayerChannels, PlayerTurnProps> {
    StreamingXml::<TurnEmission<PlayerChannels>, PlayerDiagnostic>::new(contract())
        .state_with(SelectIntentState::default)
        .on_open(reduce_select_intent)
        .into_durable_component(
            RuntimeContract::new("player.select-intent", "v1").unwrap(),
        )
}
```

这里不再重复填写 component key。`RuntimeContract` 的 declaration id 会生成稳定、
无碰撞的结构 key；只有父树确实需要另一份 placement identity 时，才使用
`.into_durable_component_with_key(...)`。显式 key 只改变 component tree identity，
不会改变跨进程 reopen 使用的 runtime declaration id/version。

一个 durable epoch 内，每个 streaming leaf 仍必须拥有不同的 runtime declaration id
和 XML route。可重复的 streaming component 因此应从父 component 的稳定配置取得这两个
值；`key` 只标识该 child 在 component tree 中的位置，不能把同一个 parser route 变成两个
独立 handler。这样 parser dispatch、durable manifest 和 reopen projection 始终是一一对应的。

`DurableComponent` 的 POM projection 在一个 process 内是线性的：创建新 epoch 时
可消费一次，existing epoch/recovery 只读取同一 leaf 的 POM-free declaration。普通
isolated mount 可通过 `.into_one_shot_component()` 显式消费它；真正 mounted owner 保留这些 leaf
在私有 catalog 中。当前公开的 `InMemoryMountedAgentFactory` 通过 opaque
`MountedAgent`/`MountedCall` 使用这个 owner，但 catalog、lease、actor 与 rebind registry
仍不对 author 暴露。这避免了同一 runtime declaration 在 System authoring 和 rebind
registry 里写两次。

`MountProvidedView`、`ProvidedView` 和 `StreamingProvidedView` 仍是 compatibility
spelling，尚未删除；新 mounted authoring 从 `Component` 或 exact-one
`DurableComponent` 进入，后者只在 isolated mount 时才显式
`.into_one_shot_component()`。
这些 carrier 是 API 收敛的实施起点，而不是 freeze 宣言。当前 public isolated lifecycle API 不能
自行 attach provider epoch；公开的 `InMemoryMountedAgentFactory` 则提供 narrow opaque
`MountedAgent` facade，但它仅是 `Commit = Never` 的 process-local lifecycle proof。crate-private
owner 已经持久化 epoch artifact，并证明 reopen 只做 POM-free runtime rebind 与 provider receipt
rehydration，不会再次调用 `SystemView`。这仍不是 public freeze contract：production
store/recovery adapter、stateful provider session、managed reconfiguration 的 public status contract
与真实 AgentLoop migration 尚未完成。crate-private durable reconfigure 已把 immutable owner policy
与 `MountedEpochDefinition` 分开，并证明一次性 System render、原子 session/System 切换和 POM-free
retry；abandoned `RenderStarted` 现在只能通过 exact-fence store transaction abort，同 id 会保留
tombstone，owner 从该事务返回的 epoch-A snapshot 原地恢复。

**已确认**：System 和 User 仍然使用同一种 POM `Document`，区别只来自生命周期
root。普通 child component 不需要知道自己最终属于哪一种 prompt role。

`PomView` 是纯 POM fragment，不携带 binding type；`ProvidedView<C>` 同时携带 POM 和
runtime declaration。多个 provided child 先通过 channel-specific mapping 归一化为
harness root channels，再由 compiler 在内部 erase 不同 factory concrete types。

当前兼容层把 `ProvidedView<B>` 作为既有 `View<B>` 的规范名称；`PomView` 是独立的
nominal type。`#[view(component)] fn ... -> PomView` 内仍可写统一的 `view(...)`，宏根据
声明返回类型完成无 binding 提升；普通 Rust helper 可显式使用 `pom_view(...)`。
当一个纯 component 的 `if`/`match` 分支同时返回 child `PomView` 和新建 POM fragment
时，新建 fragment 分支也应显式写 `pom_view(...)`，让所有分支具有同一 Rust 类型。
当前 `.map_binding(...)` 只服务兼容 carrier。单 lane component 返回 nominal
`StreamingValueView<E,D>`，可在 parent composition 点用
`.map_output/.map_live/.map_commit` 选择 lane，再用 `.map_diagnostic` 完成归一化；
single-lane 方法不实现在 raw `View<StreamingBinding<...>>` 上。一个 tag
需要多个 lane 时，component 返回 nominal `StreamingChannelsView<LocalChannels>`，因此
只能使用一次完整的 `TurnChannelMap<Local, Root>` 同时映射
output/live/commit/diagnostic，不能再调用 single-lane shortcuts。XML streaming 的 local
与 root event 都由类型约束固定为 `TextTurnEvent`。`StreamUpdate` 允许
同一 callback 同时产生 emissions 与非终止 diagnostics。isolated mounted path 已把
initializer、`open`、`stream`、`complete`、strict EOF 和 `finish` 的失败归为 terminal
`BindingFault`。mounted host 还会把 Live runtime failure 归为独立的 `LiveEffectFault`；把该
fallible contract 交给 `TurnSink`、executor 和 AgentLoop 仍待完成。

`MountProvidedView<C, TurnProps>` 是并行的异构声明 carrier：完整 channel mapping 会同时包住 binding
factory 与 provider dispatcher factory，`compile_mount_provided` 再按稳定 declaration ID
拆成 `BindingFactoryPlan<C, TurnProps>` 与 `ProviderCapabilityPlan<C>`。mounted XML binding 的
`open/stream/complete/finish` callback 都是同步、pure、可失败的 reducer；attempt 级唯一 parser
按 wire order 路由。factory 可经 `TurnBindingCx` 读取 typed turn props，失败会成为带
`BindingOrigin` 和 `BindingPhase` 的 terminal `BindingFault`。provider dispatcher 已有 async
`dispatch`、`finish` 与 explicit `abort` contract；一个 capability group 的多个 tool schema 可
共享 stateful per-attempt dispatcher。framework 按 provider order 串行 dispatch，保留
`ProviderInvocationKey`，并把 dispatcher update 的 Live/Commit 按 XML 相同的 lifecycle rules
解释。expected tool error 保持 model-visible result，terminal infrastructure failure 则停止 attempt。
isolated host 已能 await typed Live runtime、补偿 abort scope，并通过旧 `TurnPublisher` gate
Commit 和 provider results；crate-private driver 也已证明 process-local phase ordering 与
failure-retaining retry。production-intent 路径已经独立加入 typed Commit staging、durable request/
candidate-fingerprint/item identity、expected-revision CAS、atomic session + outbox contract，以及 caller
cancellation 后继续 publish/resolve 的 crate-private owner。fingerprint factory 在 typed Commit
staging 后看到最终 mutation/output/results/outbox，再生成 host 定义的 versioned canonical value；
这不是 framework 对 generic mutation/payload 的隐式序列化。crate-private in-memory store 已证明
pending-candidate identity 的 reopen reconciliation，但真实 AgentLoop、production durable store、
完整 candidate payload recovery 和 outbox worker 仍不存在，所以这还不是完整 production contract。

### 2.1 可复用 child 的 props projection

一个可复用 provided feature 不必要求整个 harness 使用同一份 `Props`。父树在组合点用
`.project_props(...)` 提供一个纯的 borrowed projection；它与 `.map_channels(...)` 分开，前者
只改变 binding/provider-dispatcher 所见的 turn props，后者只改变 typed channel lanes：

```rust,ignore
struct SelectIntentProps {
    intent_index: usize,
}

struct PlayerTurnProps {
    select_intent: SelectIntentProps,
    task: TaskView,
}

#[view(component)]
fn select_intent_feature() -> DurableSystem<PlayerChannels, SelectIntentProps> {
    durable_system((
        StreamingXml::new(select_intent_contract())
            .try_state_with(|cx| Ok(SelectIntentState::from(cx.props().intent_index)))
            .on_open(reduce_select_intent)
            .into_durable_component(select_intent_runtime_contract()),
        durable_provider_tool_with_context(
            world_lookup_runtime_contract(),
            (),
            world_lookup_schema(),
            |cx| Ok(WorldLookup::for_intent(cx.props().intent_index)),
        ),
    ))
}

#[view(component)]
fn player_system() -> DurableSystem<PlayerChannels, PlayerTurnProps> {
    durable_system((
        PlayerIntentRules::new(),
        select_intent_feature()
            .project_props(|player: &PlayerTurnProps| &player.select_intent),
    ))
}
```

projection 不会在 durable System POM render 或 rehydrate 时执行；它只在 final
prepared turn 创建 fresh binding/dispatcher state 时读取。child 如果需要变换后的值或异步
取得的值，应由 `MountedTurnCapture` 生成 owned snapshot；`project_props` 故意只做 borrowed
reference projection、不创建 child props，不是 async transform，也不是 state mutation hook。public authoring test
同时覆盖 ordinary 与 durable tree，证明投影后的 context 会抵达 binding factory 和 provider
dispatcher。

### 2.2 跨 System/User 的 reusable feature

`MountedFeature<C, Props>` 将一个 feature 的 retained `DurableSystem` 与其每轮 User POM
fragment 放在同一个 pure carrier 中。它不是第二个 owner，也不包含 provider、store、Live
runtime 或 async capture；这些仍是 host boundary。多个 feature 按 `.compose(...)` 的作者顺序
合并：System POM/runtime declarations 构成同一个 durable epoch tree，User fragment 则在每个
fresh snapshot 中按同一顺序渲染。

`MountedFeature::new` 用于 infallible User fragment。User POM builder 返回
`Result` 时使用 `MountedFeature::try_new`；错误会保留在 component tree 中，并在 turn
preparation 报告，不会为了传播 `PomError` 被迫再拆一个仅作包装的 child component，也不会在
失败后启动 provider I/O。

`try_new` 保留 renderer 的具体错误类型，因此直接返回 POM builder 的
`Result<_, PomError>` 不需要包装。若 closure 内部使用 `?`，且错误类型没有从其他表达式
唯一推断出来，则显式标注 `|cx| -> Result<_, ComponentError> { ... }`；`PomError` 会自动
转换为该 component authoring error。

```rust,ignore
#[view(component)]
fn select_intent_feature() -> MountedFeature<PlayerChannels, SelectIntentProps> {
    MountedFeature::new(
        durable_system((
            PlayerIntentRules::new(),
            StreamingXml::new(select_intent_contract())
                .try_state_with(|cx| Ok(SelectIntentState::from(cx.props().intent_index)))
                .on_open(reduce_select_intent)
                .into_durable_component(select_intent_runtime_contract()),
        )),
        |cx| pom_view(IntentPoolView::from(cx.props())),
    )
}

#[view(component)]
fn task_feature() -> MountedFeature<PlayerChannels, TaskProps> {
    MountedFeature::new(
        durable_system(TaskPolicy::new()),
        |cx| pom_view(TaskView::from(cx.props())),
    )
}

fn player_definition() -> CapturedMountedHarnessDefinition<PlayerChannels, PlayerCapture> {
    select_intent_feature()
        .project_props(|props: &PlayerTurnProps| &props.select_intent)
        .compose(task_feature().project_props(|props: &PlayerTurnProps| &props.task))
        .into_harness(EpochContractId::new("player/v3")?)
        .with_turn_loop_policy(TurnLoopPolicy::new(NonZeroUsize::new(3).unwrap()))
        .with_capture(PlayerCapture)
}
```

`into_harness` is the only conversion that creates the paired System/User root.
It consumes the feature tree, so a linear durable System POM projection cannot be mounted by
two owners. `with_capture` remains last and host-owned: capture can await and assemble the one
owned root snapshot, after which every feature is again synchronous and pure. The public local
owner test proves two projected features reach their own binding/dispatcher props, attach one
ordered System, and produce both User fragments on each call. Its feature-specific reopen case
also proves receipt rehydration does not execute either feature's System or User renderer. The
executable `examples/pom_feature_composition.rs` shows the same default public authoring path and
opens it with a host-owned dispatcher registry. It executes two native-tool turns, checking one
System attachment and fresh User POM/tool results per turn. This is a local in-memory host proof,
not a production persistence/provider claim.

`TurnFlow::Continue` 的上限属于 harness，不属于每次 `start` 调用。未配置时
`TurnLoopPolicy::ONE_TURN` 保守地只允许一轮；需要 continuation 的 harness 必须显式选择
policy。调用方若有自己的超时或配额，只能收紧而不能扩大该 policy：

```rust,ignore
let input = MountedCallInput::new(call_id, input_id, "inspect", props, source)?
    .with_turn_cap(NonZeroUsize::new(1).unwrap());
```

owner 使用 `min(harness_policy, turn_cap)`。因此 provider、reducer 或调用方都不能通过
一个 call 把产品定义的 `Continue` 行为从两轮提升到三轮。

上限也不是 process-local 的偶然配置。mounted owner 将它写入 durable epoch manifest，
artifact fingerprint 同样覆盖它；重开同一个 durable epoch 时若上限不同，公开结果是
`MountedOpenError::ContractMismatch`，不会重新执行或发送 System。`TurnLoopPolicy` 目前是
immutable owner policy，普通 System epoch reconfigure 也不能替换它；未来若产品确实需要改变
该行为，必须定义显式 owner-policy migration，不能把它伪装成普通 reopen。

当前已经可运行的 mounted XML spelling 是：

```rust
StreamingXml::<TurnEmission<LocalChannels>, LocalDiagnostic>::new(contract)
    .state_with(|| LocalState::default())
    .on_open(reduce_open)
    .on_stream(reduce_stream)
    .on_complete(reduce_complete)
    .on_finish(reduce_finish)
    .into_component()
    .map_channels(local_to_root)
```

`.state_with` 保存 initializer 而不是 state；它仍是无参、infallible convenience API。
需要本轮输入或可失败初始化时，当前 API 已提供：

```rust
StreamingXml::<TurnEmission<LocalChannels>, LocalDiagnostic>::new(contract)
    .try_state_with(
        |cx: &TurnBindingCx<'_, LocalTurnProps, LocalChannels>| {
            LocalState::new(cx.props()).map_err(LocalInitError::from)
        },
    )
    .on_open(reduce_open)
    .try_on_stream(reduce_stream)
    .on_complete(reduce_complete)
    .try_on_finish(reduce_finish)
    .into_component()
    .map_channels(local_to_root)
```

`into_component` 从同一个 contract 自动产生 System POM、stable component key 和
`RuntimeRoute::xml(tag)`；mount validation、`prepare_user` 都不会执行 initializer。只有
`PreparedUserTurn::start_streaming_attempt(live_runtime)` 才创建一个 shared parser、每条 route
的 fresh reducer state 和一个 attempt-local live scope。它复用 User render 时借用的 exact
turn props。`try_state_with` 与 `try_on_*` 的
错误会终止该 isolated attempt，而不是降级为 diagnostic。

当前 `TurnBindingCx<'_, Props, C>` 是只读 attempt-start context，公开
`props()`、`epoch_id()`、`turn_instance_id()`、`provider_attempt_id()`、`live_scope_id()`、
`call_label()`、`binding_id()` 和 `route()`。它不提供 service、executor、prompt history 或
mutable session；factory state 必须是 `Send + 'static`，因此不能保存对 `props()` 的借用。

authoritative mounted owner 仍是 crate-private contract；公开的 `MountedAgent`/`MountedCall`
只是不泄漏该 owner 的 opaque local facade。它覆盖一次性 System mount、borrowed logical-call
props、每个 preparation candidate 的 owned snapshot、User 重渲染、provider execution、cleanup、
fork/reconfigure、pure session mutation、durable publication、authoritative revision writeback，以及
同 epoch `TurnFlow::Continue`。private mount-owned reducer/persistence contract 与 typed
`TurnRecord` 已接入；logical-call checkpoint/resume、atomic admission lease、complete call ledger
与 expiry fencing 也已接入。当前 local facade 已有 owned input、`open`、`start`、`wait`、
`cancel` 与 `reload`，但 production call builder、idempotent terminal recovery、crash recovery
与 concrete persistence host 仍未接入。

当前 internal trait 的形状是：

```rust,ignore
trait TurnChannels: Send + Sync + 'static {
    type Output: Send + 'static;
    type Live: Send + 'static;
    type Commit: Send + 'static;
    type Diagnostic: Send + 'static;
}

#[async_trait]
trait MountedHarness<I = Turn>: Send + Sync + 'static {
    type MountProps: ?Sized + Send + Sync + 'static;
    type CallProps: ?Sized + Sync + 'static;
    type TurnProps: Send + Sync + 'static;
    type Source: ?Sized + Sync;
    type ContextState: Clone + Send + Sync + 'static;
    type Channels: TurnChannels;
    type CaptureError: Error + Send + Sync + 'static;

    fn epoch_contract_id(mount_props: &Self::MountProps) -> EpochContractId;

    fn system(
        cx: SystemMountContext<'_, Self::MountProps>,
    ) -> SystemView<Self::Channels, Self::TurnProps>;

    async fn capture_turn_props(
        &self,
        cx: TurnCaptureContext<
            '_,
            I,
            Self::ContextState,
            Self::CallProps,
            Self::Source,
        >,
    ) -> Result<Self::TurnProps, Self::CaptureError>;

    fn user(cx: UserTurnContext<'_, Self::TurnProps>) -> UserView;
}
```

`system` 和 `user` 都故意没有 `&self`：它们是同步、functional 的 render boundary。
`SystemMountContext` 只暴露 immutable mount props；它没有 history、task、call props、source、
service lookup 或 mutable Agent Context。只有 `capture_turn_props` 可以 await，并且只能通过
`TurnCaptureContext` 只读借用 draft context、source、整条 logical call 固定的 `CallProps` 和
call label。它必须返回 owned `TurnProps`；每次 history replacement 后会重新 capture，然后
`user` 只根据这份 snapshot 同步 render。

挂载成功后，Agent 持有一个不可拆分的 epoch bundle：

```rust,ignore
struct MountedEpoch<I, Props, C: TurnChannels> {
    id: HarnessEpoch,
    system_document: Document,
    rendered_system: Arc<str>,
    binding_factories: BindingFactoryPlan<I, Props, C>,
    provider_capabilities: ProviderCapabilityPlan<I, Props, C>,
}
```

`SystemDocument + rendered bytes + binding factories + provider capabilities` 必须
一起验证和替换。mounted owner 会把其中的 System bytes 和 native tool catalog **只在
epoch open 时**交给 provider adapter；普通 turn 不能取得或重发它们：

```rust,ignore
pub struct MountedProviderEpoch {
    // rendered System + provider tool catalog, from exactly one HarnessEpoch
}

pub struct MountedProviderRequest<I> {
    pub call_id: StorageString,
    pub history: Vec<I>,
    pub user: String,
    pub model: StorageString,
    pub max_tokens: u64,
    // deliberately no `system` and no tool catalog
}

#[async_trait]
trait MountedProviderExecutor<I> {
    type Epoch: Send + Sync + 'static;

    async fn prepare_context(
        &self,
        epoch: &Self::Epoch,
        request: &MountedProviderRequest<I>,
        budget: ContextPreparationBudget,
    ) -> Result<ContextPreparation<I>, Self::Error>;
}

trait MountedProviderEpochAttacher<I>: MountedProviderExecutor<I> {
    fn attach_epoch(
        &self,
        epoch: MountedProviderEpoch,
    ) -> Result<Self::Epoch, Self::Error>;
}
```

`MountedProviderEpochAttacher::attach_epoch` 是 ordinary one-shot provider runtime 的
binding/factory boundary，不是一个普通 turn。它在成功 mount 时执行一次；`ReplaceHistory`、retry
和 `TurnFlow::Continue` 都复用返回的 opaque `Epoch` binding。fork 共享这个
immutable/share-safe binding；显式 reconfigure 先完成 history rebase，成功后才 attach 一个新的
binding。这样 System 的 wire ownership 不是靠文档约定，而是由普通 request 类型中缺少该字段来保证。

需要 durable reopen 的 provider 还实现公开的 advanced contract
`DurableMountedProviderExecutor`。它的首次 `attach_durable_epoch` 收到 durable epoch id、
artifact fingerprint、System 和工具 schema，并返回 `AttachedProviderEpoch`（process-local
binding + serializable `ProviderEpochReceipt`）。随后 `rehydrate_durable_epoch` 只收到
epoch id、fingerprint 和原 receipt；其 request 在类型上没有 `system()` 或 `tools()`。
因此生产 adapter 可以恢复 provider-local state，却没有一条普通 reopen 路径能重新发送
System。durable adapter 不需要、也不能仅因实现 durable trait 而调用 ordinary
`attach_epoch`；只有显式实现 `MountedProviderEpochAttacher` 的 one-shot adapter 才具备该能力。
这个 seam 不公开 mounted owner、store、lease、revision 或 actor；外部 integration test 与
compile-fail test 已覆盖该边界，但 real AgentLoop adapter 仍未落地。

当前 crate-private 调用边界是 borrowed call façade：

```rust,ignore
let pinned = agent.call("player-turn", &call_props).pin().await?;
let active = pinned
    .prepare_managed(&source, start_attempt, max_replacements)
    .await?;
```

`CallProps` 在整个 pinned call 中保持借用；它不会被强迫 clone。每个 candidate 的
`TurnProps` 则在 async capture 返回后成为 owned snapshot，因此 reducer/factory 不会借用
短生命周期 host state。下面仍是未来 public builder 的 ergonomics 草案：

```rust,ignore
let agent = MountedAgent::builder(PlayerHarness)
    .mount_props(PlayerMountProps { policy, contracts })
    .provider(provider)
    .live_factory(player_live_factory)
    .persistence(PlayerPersistence::bind(session_id, store, policy))
    .open()
    .await?;

let call_props = Arc::new(PlayerCallProps { context, artifacts, task });
let source = Arc::new(source);
let call = agent
    .call(DurableCallId::new("request-42")?, "player-turn")
    .input(
        DurableCallInputId::new("request-42/input-v1")?,
        call_props,
        source,
    )
    .start()
    .await?;
let outcome = call.wait().await?;
```

这个目标 production façade 隐藏 `pin()`、epoch、Finished、revision、publication plan 与 actor，
并把 `call_props`/`source` 的 `Arc` 所有权交给 loop owner。当前 local facade 的
`MountedCallInput`、`start()`、`wait()` 与 call-scoped `cancel()` 已遵守同一 ownership 方向：
`start()` 只在 durable admission 成功后返回可等待 handle；dropping a wait future 不能取消已
接受的 call。`Arc` 只解决进程内 continuation ownership。private proof 已把
`DurableCallInputId`、`next_turn_index` 与 pending Continue 写入 durable call state，确保
host-driven resume 不从 turn 0 重跑；API freeze 前仍需提供 production persistence、idempotent
terminal recovery 和 indeterminate recovery intent，而不能把 local proof 当作其替代。

`BindingFactory` 是 reusable declaration，不拥有已经开始使用的 reducer state。local
factory 在 child 映射到 root channels 后进入 heterogeneous plan。下面把将来的
`ContextPreparation::Ready` owner 写成 target；当前 isolated API 在显式
final `PreparedUserTurn::start_streaming_attempt(live_runtime)` 时实例化：

```rust,ignore
trait TurnBindingFactory<I, Props, C: TurnChannels> {
    type Binding;

    fn instantiate(
        &self,
        cx: TurnBindingCx<'_, Props, C>,
    ) -> Result<Self::Binding, BindingFailure>;
}
```

`TurnBindingCx` 使用内部 `HarnessEpochId`、`TurnInstanceId`、`ProviderAttemptId` 和
`LiveScopeId`，而不是把用户传入的 call label 当唯一身份。它还携带 `BindingId` 与
`RuntimeRoute`，但不提供 service 或 mutable session；factory 需要的 immutable
catalog/snapshot 必须由应用放入 typed call props。

`UserTurnContext<TurnProps>` 只有 `props()` 是刻意的 contract，而不是暂时漏字段。application
定义完整的 turn props，并决定 `context`、`artifacts`、`task`、call metadata 或 captured view 中
哪些进入 User POM、以什么顺序进入；framework 不暗中注入或重排这些 section。可运行的
[`pom_mounted_lifecycle.rs`](../examples/pom_mounted_lifecycle.rs) 直接展示
`PlayerTurn { context, artifacts, task } -> user((context, artifacts, task))`。

**目标草案**：P3 初版只允许 System subtree 注册 reusable binding factories；User
subtree 只生成本轮 POM。这样 model 看见的 contract 与 runtime capability 不会分叉。
如果真实迁移证明需要 turn-local binding declaration，再增加独立概念，而不是让
当前 `binding(...)` 同时承担两个生命周期。

## 3. 编写示例

除非标题明确写为“当前”，本节的 `PomHarness`、`MountedAgent::builder` 和 production host
runtime 调用均为目标草案。当前 public API 同时有 compatibility 的
`SystemMountContext`、`UserTurnContext`、`mount_system_epoch`/`MountedTurn`，以及 local mounted
facade 的 `DurableEpochDefinition`、`MountedHarnessDefinition`、`MountedCallInput` 和
`InMemoryMountedAgentFactory::open`，随后是 `MountedAgent::{start,reload}` 与
`MountedCall::{wait,cancel}`。实际 mounted streaming spelling 见第 2 节与第 4.1 节；后者
不是 production persistence contract。

### 3.1 目标：最小挂载与普通 turn

System 只接收 owned 或 `Arc` mount config；问题、当前视图和本轮 task 只进入
User View。

```rust,ignore
#[derive(Clone)]
struct SupportMountProps {
    policy_version: Arc<str>,
}

struct SupportTurnProps {
    current_view: SupportView,
    question: String,
}

struct SupportCallProps {
    question: String,
}

struct PromptOnlyChannels;

impl TurnChannels for PromptOnlyChannels {
    type Output = Never;
    type Live = Never;
    type Commit = Never;
    type Diagnostic = Never;
}

#[view(component)]
fn support_policy(props: SupportMountProps) -> PomView {
    view((
        SupportIdentity::new("support-agent"),
        SupportPolicy::new(Arc::clone(&props.policy_version)),
        PlainAnswerContract::default(),
    ))
}

impl MountedHarness for SupportHarness {
    type MountProps = SupportMountProps;
    type CallProps = SupportCallProps;
    type TurnProps = SupportTurnProps;
    type Channels = PromptOnlyChannels;
    type Source = SupportSource;
    type ContextState = SupportContextState;
    type CaptureError = Infallible;

    fn epoch_contract_id(props: &Self::MountProps) -> EpochContractId {
        EpochContractId::new(format!(
            "support/{}/plain-answer/v1",
            props.policy_version,
        ))
        .expect("mount props contain a validated policy version")
    }

    fn system(cx: SystemMountContext<'_, Self::MountProps>) -> SystemView<PromptOnlyChannels> {
        system(support_policy(cx.props().clone()))
    }

    async fn capture_turn_props(
        &self,
        cx: TurnCaptureContext<
            '_,
            Turn,
            Self::ContextState,
            Self::CallProps,
            Self::Source,
        >,
    ) -> Result<Self::TurnProps, Infallible> {
        Ok(SupportTurnProps {
            current_view: cx.context().context_state().current_view.clone(),
            question: cx.call_props().question.clone(),
        })
    }

    fn user(cx: UserTurnContext<'_, Self::TurnProps>) -> UserView {
        user((
            cx.props().current_view.build_root()?,
            Task::new(&cx.props().question).build_root()?,
        ))
    }
}

let agent = MountedAgent::builder(SupportHarness)
    .mount_props(Arc::new(SupportMountProps { policy_version: "v3".into() }))
    .provider(Arc::new(provider))
    .persistence(persistence)
    .open()
    .await?;

let call = agent
    .call(DurableCallId::new("ticket-42")?, "answer-ticket")
    .input(
        DurableCallInputId::new("ticket-42/input-v1")?,
        Arc::new(SupportCallProps { question }),
        Arc::new(source),
    )
    .start()
    .await?;
let outcome = call.wait().await?;
```

两次 `call` 会执行两次 User View，但 `support_policy` 只在 `mount` 中执行一次。
改变 `policy_version` 不是普通 turn input，而是创建一个新的 mount candidate。

### 3.2 目标：Role-agnostic child 与条件性 User POM

普通 child component 不调用 `system(...)` 或 `user(...)`。父 lifecycle root 决定
placement，因此同一个纯 POM component 可以复用。

```rust,ignore
#[view(component)]
fn evidence_format(style: EvidenceStyle) -> PomView {
    view((
        EvidenceRules::new(style),
        CitationShape::default(),
    ))
}

fn system(cx: SystemMountContext<'_, AnalystConfig>) -> SystemView<PromptOnlyChannels> {
    system((
        AnalystIdentity::new(),
        evidence_format(cx.props().evidence_style.clone()),
    ))
}

fn user(cx: UserTurnContext<'_, AnalystTurnProps>) -> UserView {
    let repair = cx.props().artifacts.last().map(RepairNote::from);
    let focus = cx.props().focus.as_ref().map(FocusTarget::from);

    user((
        cx.props().current_view.build_root()?,
        repair,
        focus,
        Task::new(&cx.props().question).build_root()?,
    ))
}
```

`Option` 和 collection 仍按 source order 组合。User 中的条件可以改变本轮内容，
但不能条件性增删 System 已声明的 tool/streaming capability。

### 3.3 目标：显式 DiffSlot context

应用自己决定 User POM 的 section 和顺序。只有 `#[view(diff)]` 对应的 XML edge
参与 full/delta/omitted；artifacts 与 task 每轮完整发送。

```rust,ignore
#[derive(AgentView)]
#[agent_view(document)]
struct PlayerUserDocument {
    #[view(heading = 2)]
    title: &'static str,

    #[view(name = "agent_context", diff)]
    context: Option<PlayerContextView>,

    #[view(block)]
    artifacts: Vec<TurnArtifact>,

    #[view(block)]
    task: TaskParagraphView,
}

fn user(cx: UserTurnContext<'_, PlayerTurnProps>) -> UserView {
    let document = PlayerUserDocument {
        title: "Player turn",
        context: Some(cx.props().current_view.clone()),
        artifacts: cx.props().artifacts.clone(),
        task: TaskParagraphView::new(&cx.props().task),
    };

    user(document.build_root()?)
}
```

第一次成功提交发送完整 `<agent_context>`。下一轮 unchanged slot 被省略，changed
slot 发送 delta，显式 absent slot 按 diff strategy 表达删除。preparation 被丢弃或
provider 失败时，candidate cursor 不提交。

### 3.4 目标：Commit-only StreamingXml

这个例子刻意使用一个只有成功 publication 后才允许应用的 action proposal。它不是
Forgotten City `SelectIntent`；后者的 production behavior 是 live，并在 5.1 单独迁移。
XML contract 在 System mount 中固定，本轮 allowed-action snapshot 只进入 factory
props/state，不进入 System POM。

```rust,ignore
#[derive(Debug)]
enum NpcCommit {
    ApplyAction(NpcAction),
}

#[derive(Debug)]
enum NpcDiagnostic {
    Action(ActionDiagnostic),
}

#[view(component)]
fn action_proposal(contract: Arc<ActionContract>) -> ProvidedView<NpcChannels> {
    StreamingXml::new(contract.build_root()?)
        .try_state_with(|turn: &TurnBindingCx<'_, NpcTurnProps, NpcChannels>| {
            Ok(ActionState::new(
                turn.live_scope_id(),
                Arc::clone(&turn.props().allowed_actions),
            ))
        })
        .on_complete(|state, element| {
            let action = parse_action(element)?;
            state.allowed_actions().validate(&action)?;
            Ok(StreamUpdate::commit(ActionCommit::Apply(action)))
        })
        .map_commit(NpcCommit::from)
        .map_diagnostic(NpcDiagnostic::Action)
        .into_view()
}

fn system(cx: SystemMountContext<'_, NpcMountProps>) -> SystemView<NpcChannels> {
    system((
        NpcActionRules::new(),
        action_proposal(Arc::clone(&cx.props().action_contract)),
    ))
}
```

这段是目标 authoring sketch；`StreamUpdate::commit(...)`、`map_commit(...)` 以及
`SystemMountContext`/`ProvidedView` 的完整 mounted spelling 尚未一起作为 AgentLoop API 交付。
当前 `.try_state_with(...)` 已真实保存 reusable initializer，但它在
`PreparedUserTurn::start_streaming_attempt` 时执行，而不是由现有系统中的
`ContextPreparation::Ready` 自动驱动。
它产生的 `TurnEmission::Commit` 会留在 attempt-private buffer。旧 isolated proof 只有在 awaited
`TurnPublisher` 成功后才允许 `PublishedStreamingAttempt::pending_commits()`，并由 process-local
interpreter retry。production-intent 路径则在 root channel 尚未 erase 时调用
`stage_durable_publication`：每条 Commit 经 `CommitStager<C>` 一对一编码为稳定 outbox item，
store transaction 同时写完整 session mutation 与 outbox。host 还必须提供带版本的
`PublicationCandidateFingerprint`，它覆盖 expected revision、完整 mutation、raw output、全部 provider
results，以及 ordered outbox contract/payload；它绝不能包含 process-local attempt identity。durable
outcome 不再暴露 pending Commit；请求 future 也不负责外部 delivery。真实 AgentLoop 尚未构造
mutation，concrete store 与 recovery-scanned worker 仍待实现。

### 3.5 目标：实时 Phrase streaming

实时 reducer 与 commit-only reducer 使用同一个 parser 时序，但产生不同 channel。
`PhraseLive` 应在当前 callback 后交给 host runtime 并 await，之后才能处理下一个 chunk；
这条 isolated runtime contract 已由 `LiveEffectRuntime` 实现。`PreparedUserTurn` 启动 attempt 时
要求 root host 明确选择 runtime；每个 Live emission 在 callback 内顺序 await，成功后不会再次出现在
returned update。runtime failure 是 terminal `LiveEffectFault`，后续 tag 不会继续执行。

```rust,ignore
#[derive(Debug)]
enum PhraseLive {
    Open { slot: PhraseSlot },
    Append { slot: PhraseSlot, text: String },
    ParsedClose { slot: PhraseSlot, full_text: String },
}

#[view(component)]
fn phrase_stream(contract: Arc<PhraseContract>) -> ProvidedView<NpcChannels> {
    StreamingXml::new(contract.build_root()?)
        .try_state_with(|turn: &TurnBindingCx<'_, NpcTurnProps, NpcChannels>| {
            Ok(PhraseState::new(
                turn.live_scope_id(),
                turn.props().phrase_slot,
            ))
        })
        .on_open(|state, _| {
            Ok(StreamUpdate::live(PhraseLive::Open {
                slot: state.slot(),
            }))
        })
        .on_stream(|state, element| {
            match state.take_delta(&element.content) {
                Some(text) => Ok(StreamUpdate::live(PhraseLive::Append {
                    slot: state.slot(),
                    text,
                })),
                None => Ok(StreamUpdate::none()),
            }
        })
        .on_complete(|state, element| {
            Ok(StreamUpdate::live(PhraseLive::ParsedClose {
                slot: state.slot(),
                full_text: element.content.clone(),
            }))
        })
        .into_view()
}
```

```text
TextDelta -> parser -> pure reducer -> StreamUpdate -> host LiveRuntime -> next TextDelta
XML close -> ParsedClose (tentative lease remains open)
finish -> typed Commit staging + complete pure session mutation
publish -> Resolving -> CAS session revision + insert outbox -> Published
outbox worker -> at-least-once external delivery keyed by OutboxItemId
known rejection -> Ready -> retry same request or acknowledged abort
indeterminate/cancelled waiter -> resolve same request id; never replay provider first
active failure -> LiveEffectRuntime::abort -> compensation -> pure local teardown
```

Reducer 不捕获 `WorldApi`、channel、`TextWriter` 或 service handle；host runtime 拥有这些能力。
实现会先产生 owned `PhraseLive` value 并释放 reducer-state lock，再 await runtime。abort 先
await 外部 compensation，再执行所有 pure `on_abort` reducer；两边结果都保留在
`StreamingAbortReport`。raw task Drop 仍不能替代显式 async abort。

### 3.6 目标：多个 provided components 的组合

父 component 将 child 的 output/live/commit channel 提升到 application enum。只产生
一个 lane 的 child 可以使用 `map_live`、`map_output`、`map_commit` 快捷方式；同一个
streaming tag 产生多个 lane 时必须使用完整 `TurnChannelMap<Local, Root>`。后者一次映射
全部 emission 与 diagnostic，不能留下半归一化的 component。

```rust,ignore
enum NpcOutput {
    ReplyParsed(ParsedReply),
    SafetyChecked(SafetyDecision),
}

enum NpcLive {
    Speech(PhraseLive),
}

enum NpcCommit {
    Action(NpcAction),
    BehaviorLogged(BehaviorEntry),
}

enum NpcDiagnostic {
    Speech(PhraseDiagnostic),
    Reply(ReplyDiagnostic),
    Action(ActionDiagnostic),
    Behavior(BehaviorDiagnostic),
}

#[view(component)]
fn npc_reply_contract(config: Arc<ReplyContractConfig>) -> ProvidedView<NpcChannels> {
    view((
        phrase_stream(Arc::clone(&config.phrase))
            .map_live(NpcLive::Speech)
            .map_diagnostic(NpcDiagnostic::Speech),
        parse_reply(Arc::clone(&config.reply))
            .map_output(NpcOutput::ReplyParsed)
            .map_diagnostic(NpcDiagnostic::Reply),
        select_action(Arc::clone(&config.action))
            .map_commit(NpcCommit::Action)
            .map_diagnostic(NpcDiagnostic::Action),
        log_behavior(Arc::clone(&config.behavior))
            .map_commit(NpcCommit::BehaviorLogged)
            .map_diagnostic(NpcDiagnostic::Behavior),
    ))
}

fn system(cx: SystemMountContext<'_, NpcMountProps>) -> SystemView<NpcChannels> {
    system((
        NpcIdentity::new(cx.props().identity.clone()),
        NpcPolicy::new(cx.props().prohibitions.clone()),
        npc_reply_contract(Arc::clone(&cx.props().reply_contract)),
    ))
}
```

这里假定每个 local provided component 未使用的 channel 是 `Never`，所以
channel-specific shortcut 足以完成 mapping；若一个 child 同时产出多种 channel，则用
一次 `.map_channels(...)` 显式映射完整 bundle。

同一 parent 下重复挂载或条件性插入 component 时，任何需要稳定 identity 的 sibling
必须使用 `.key("...")`。当前 positional identity 会在前置 `Option::None` 时重新编号；
测试同时覆盖了 unkeyed shift 和 keyed stability。static output tag 和 `BindingId` 在
mount 时验证；目标 mounted System 在同一 epoch 内不允许 User props 动态增删这些
capability child。

## 4. 生命周期与测试示例

本节先记录当前可运行的 isolated lifecycle，再保留 AgentLoop/Live runtime 的目标测试。
后者使用 `rust,ignore`，不应被理解为当前 host 已提供的时序保证。

### 4.1 当前 isolated attempt 的完成与回收

当前 mounted API 的成功路径是：`mount_system_epoch` 产出 immutable epoch，`begin_turn`
分配 logical turn identity，`prepare_user` 把 rendered User candidate 与 exact typed props 绑定，
其 `start_streaming_attempt` 再用 host-selected Live runtime 建立 fresh parser/reducer state
以及 provider dispatcher groups；`on_event` 和 `call_tool` 都会先 await 本次 update 的全部
Live effect，再返回 Output/diagnostic 或 model-visible tool result。native-only harness 可改用
`start_provider_attempt`，但同样有 fresh dispatcher、attempt identity、replay/collision、finish、
publication 与 abort typestate。Commit 保留在 attempt 内，不能在 publication 前读取。

```rust
let epoch = mount_system_epoch(&mount_props, system)?;
let turn = epoch.begin_turn("player-turn");
let prepared = turn.prepare_user(&turn_props, user)?;

let mut attempt = prepared.start_streaming_attempt(live_runtime)?;
let update = match attempt
    .on_event(TextTurnEvent::TextDelta("<selection index=\"2\" />".to_owned()))
    .await
{
    Ok(update) => update,
    Err(error) => {
        let report = attempt.abort(BindingAbortReason::ParserFailure).await;
        record_local_abort(report);
        return Err(error.into());
    }
};

// `finish_stream(self)` takes ownership of the live attempt. It produces a
// finished attempt only after strict EOF and every binding's `on_finish` succeed.
let finished = match attempt.finish_stream().await {
    Ok(finished) => Some(finished),
    Err(failure) => {
        let report = failure.abort(BindingAbortReason::ParserFailure).await;
        record_local_abort(report);
        None
    }
};

// `publish_with` awaits the host's real session publication boundary. Only its
// success can create the unforgeable receipt carried by the published typestate.
if let Some(mut finished) = finished {
    let final_update = finished.take_update();
    match finished.publish_with(&mut publisher).await {
        Ok(mut published) => {
            deliver_outputs(final_update);
            deliver_commits(published.take_pending_commits()).await?;
        }
        Err(failure) => {
            let report = failure.abort(BindingAbortReason::PublishFailure).await;
            record_abort(report);
        }
    }
}
```

`finish_stream` performs strict parser finalization before `on_finish`. An incomplete registered
tag becomes `BindingPhase::Finalize`; a reducer failure is attributed to `Open`/`Stream`/
`Complete`; an `on_finish` failure is `BindingPhase::Finish`. A callback failure makes the attempt
terminal, stops later callbacks in the same chunk, and later `on_event` calls return
`StreamingAttemptError::Terminal`. `StreamingFinishFailure` retains the attempt specifically so
the host can call `abort`. `StreamingAbortReport` 同时包含 awaited Live compensation 结果和每条
binding 的 local cleanup acknowledgement；即使 compensation 失败，所有 local teardown 仍执行。

`PublishedTurnReceipt`、`publish_with` 和 `PublishedStreamingAttempt` 只属于旧 isolated proof：
framework-issued receipt 证明 phase ordering，crate-private interpreter 证明同一进程内的 whole-batch
retry，但它既不绑定 AgentSession transaction，也不是 durable identity。

durable proof 使用另一条 contract：host 在 store await 前分配 `PublicationRequestId`，并提供
`PublicationCandidateFingerprint`。它是 host-owned、versioned canonical encoding 的结果；framework
不会为 generic mutation/payload 猜测序列化规则。该值必须覆盖 expected revision、完整
`PreparedSessionMutation`、raw output、provider results，以及每个有序 outbox contract/payload，且排除
`ProviderAttemptIdentity`。Commit staging 生成稳定 `(request id, index)` item，`PublicationStore` 对
expected revision 做 CAS，并在一笔事务中写完整 mutation 与所有 outbox row。store 必须持久化并
byte-for-byte 比较 `(request id, fingerprint)`：完全相同才可返回旧 receipt；同 request id、不同
fingerprint 必须 definite `Rejected`，不得覆盖已有 mutation/outbox。store response 丢失时状态保持
Resolving，必须按相同 request id 和 fingerprint 查询；collision 必须返回 store error，不能伪装成
`Published` 或 `NotCommitted`。authoritative `NotCommitted` 与 definite fingerprint collision 都证明
当前 candidate 未提交并回到 Ready；后者以 terminal `PublishFailure` abort，而不是网络重试。
`DurablyPublished*Attempt` 只携带 durable receipt、typed Output/Diagnostic 和 provider transcript，
不提供 Commit delivery API。

### 4.2 目标：System 一次、User 按 preparation 次数执行

下面的 AgentLoop trace 是目标行为，不是当前 executable flow：

一个 logical call 先遇到一次 `ReplaceHistory`，第一次 Ready turn 提交后返回
`Continue`，第二次 Ready turn 返回 `Wait`。因此 System render 为 1，User render 为
3，binding instance 为 2。

```rust,ignore
#[derive(Debug, Clone, PartialEq, Eq)]
enum LifecycleEvent {
    MountSystem { epoch: HarnessEpoch },
    RenderUser { attempt: usize, history_len: usize },
    InstantiateBinding { instance: u64 },
    PublishSession { iteration: usize },
}

#[tokio::test]
async fn mounted_system_survives_replacement_and_continue() {
    let trace = LifecycleTrace::default();
    let agent = traced_agent(trace.clone())?; // records MountSystem once
    let executor = ReplaceHistoryOnceThenContinueOnce::new();

    agent
        .call("two-step")
        .with_props(TestTurnProps { value: 7 })
        .with_max_loops(2)
        .execute_loop(&source, &executor)
        .await?;

    assert_eq!(trace.count(LifecycleEventKind::MountSystem), 1);
    assert_eq!(trace.count(LifecycleEventKind::RenderUser), 3);
    assert_eq!(trace.count(LifecycleEventKind::InstantiateBinding), 2);
    assert_eq!(trace.count(LifecycleEventKind::PublishSession), 2);

    assert_eq!(
        trace.user_history_lengths(),
        &[old_history_len, compacted_history_len, committed_history_len],
    );
    assert!(trace.system_events_after_mount().is_empty());
}
```

这三个 User render 分别对应 discarded preparation、第一轮 Ready、Continue 后的
第二轮 Ready。provider adapter 在同一 request 上做 transport retry 时复用已渲染的
User bytes，不产生第四次 render。

### 4.3 目标：discarded plan 不实例化 reducer state

factory declaration 的 identity 在 epoch 内稳定，但每个真正执行的 provider attempt
得到不同 instance。`ReplaceHistory` 只丢弃 User candidate；它发生在 binding
instantiate 之前。

```rust,ignore
#[tokio::test]
async fn only_ready_turns_receive_fresh_binding_state() {
    let ids = Arc::new(AtomicU64::new(0));
    let bindings = Arc::new(Mutex::new(Vec::new()));
    let agent = binding_id_agent(Arc::clone(&ids), Arc::clone(&bindings))?;

    agent.call("replace-once").execute(&source, &replace_once).await?;
    agent.call("provider-fails").execute(&source, &failing).await.unwrap_err();
    agent.call("after-failure").execute(&source, &ready).await?;

    assert_eq!(*bindings.lock().await, vec![1, 2, 3]);
    assert_eq!(ids.load(Ordering::SeqCst), 3);
    assert_eq!(replace_once.prepare_count(), 2);
    assert_ne!(bindings.lock().await[1], bindings.lock().await[2]);
}
```

这里 failure 会消耗自己的 instance，但不能把已完成或失败的 parser/state 交给下一轮。
内部 `TurnInstanceId`/`ProviderAttemptId` 也必须不同；对 abort 应有同样覆盖。

### 4.4 已实现 isolated proof：live effect 的顺序与背压

下面的 executor 在每次 `TextDelta` 返回之后记录进度。测试 runtime 在处理 `Open`
时停在显式 gate；gate 打开前，executor 不可能观察到第一个 callback 已返回，也不能
投递后续 `Append`。

```rust,ignore
#[tokio::test]
async fn live_effect_is_awaited_before_the_next_parser_event() {
    let (live_tx, mut live_rx) = mpsc::channel(8);
    let open_gate = Arc::new(Barrier::new(2));
    let runtime = GatedLiveRuntime::new(live_tx, Arc::clone(&open_gate));
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let executor = ScriptedStream::new(
        ["<phrase>", "hello", "</phrase>"],
        progress_tx,
    );

    let turn = tokio::spawn(agent.call("phrase").execute_with_live_runtime(
        &source,
        &executor,
        runtime,
    ));

    assert_eq!(live_rx.recv().await, Some(PhraseLive::Open { slot }));
    assert!(progress_rx.try_recv().is_err());

    open_gate.wait().await;
    assert_eq!(progress_rx.recv().await, Some(ProviderProgress::ChunkHandled(0)));
    assert_eq!(live_rx.recv().await, Some(PhraseLive::Append {
        slot,
        text: "hello".to_owned(),
    }));

    turn.await??;
}
```

默认语义是同一 binding 内严格 callback order 和 backpressure。若应用选择 buffered
或 fire-and-forget delivery，必须挂载另一个明确命名的 runtime policy，不能悄悄
改变默认行为。上面的 AgentLoop spelling 仍是目标；当前 regression test 直接 gate
`LiveEffectRuntime::apply`，并证明同一 chunk 的下一个 XML tag reducer 不会提前执行。

### 4.5 已实现 isolated proof：abort、provider failure 与 live cleanup

`ParsedClose` 表示 parser 已看见闭合标签，不代表整个 provider turn 已提交。若后续
provider framing、sink 或 session commit 失败，host 仍必须显式 cancel/compensate
已经可见的 live output。

```rust,ignore
#[tokio::test]
async fn abort_cancels_open_live_scope_exactly_once() {
    let runtime = RecordingLiveRuntime::default();
    let executor = PauseAfterPhraseOpen::new();
    let turn = agent
        .call("phrase")
        .start_with_live_runtime(&source, &executor, runtime.clone())
        .await?;

    executor.open_callback_returned().await;
    let report = turn.cancel(AbortReason::Caller).await?;

    assert_eq!(runtime.events(), vec![
        LiveRuntimeEvent::Apply(PhraseLive::Open { slot }),
        LiveRuntimeEvent::Cancel { scope, slot, reason: AbortReason::Caller },
    ]);
    assert!(report.acknowledged(scope));
    assert_eq!(runtime.cancel_count(slot), 1);
}

#[tokio::test]
async fn failure_after_xml_close_still_compensates_live_output() {
    let runtime = RecordingLiveRuntime::default();
    let executor = ClosePhraseThenFailProvider::new();

    agent
        .call("phrase")
        .execute_with_live_runtime(&source, &executor, runtime.clone())
        .await
        .unwrap_err();

    assert_eq!(runtime.events_for(slot), [
        Apply(PhraseLive::Open { slot }),
        Apply(PhraseLive::Append { slot, text: "hello".into() }),
        Apply(PhraseLive::ParsedClose { slot, full_text: "hello".into() }),
        Cancel { scope, slot, reason: AbortReason::ProviderFailed },
    ]);
}
```

cleanup 不能只藏在 reducer state 的 `Drop` 中。Runtime 必须收到结构化 abort reason；
`Drop` 最多作为进程内的兜底，不能执行需要 await 或保证送达的补偿。isolated attempt 现已
先 await runtime compensation，再执行所有 local abort reducer；compensation failure 也不会
跳过 local teardown。managed AgentLoop cancellation 仍未实现。

### 4.6 已实现 isolated gate/interpreter proof：commit effect publication

commit effect 可以改变 durable application state，所以必须拿到已经发布的 turn token。
provider success 本身不够；User cursor、history 和 context state 作为一个 session draft
成功发布后，interpreter 才能运行。

```rust,ignore
#[tokio::test]
async fn commit_effect_runs_only_after_session_publication() {
    let runtime = RecordingCommitRuntime::default();

    agent
        .call("apply-action")
        .execute_with_commit_runtime(&source, &ready, runtime.clone())
        .await?;

    assert_eq!(runtime.effects(), [NpcCommit::ApplyAction(action.clone())]);
    assert!(runtime.observed_tokens().iter().all(PublishedTurn::is_visible));

    agent
        .call("apply-action-fails")
        .execute_with_commit_runtime(&source, &failing, runtime.clone())
        .await
        .unwrap_err();

    assert_eq!(runtime.effects(), [NpcCommit::ApplyAction(action)]);
}
```

外部 commit effect 若要求崩溃恢复，需要 durable outbox 和 receiver-side idempotency；AgentView
不虚构跨进程 exactly-once。当前 durable contract 已固定 atomic enqueue：session mutation 与
ordered outbox rows 必须同事务提交，item identity 为稳定 `(PublicationRequestId, index)`，caller
cancellation 不会取消 actor 已接受的 store command。`Published` 只表示 durably queued；worker
之后的 external delivery 仍是 at-least-once。上面的 public `MountedAgent` API、concrete store、
restart recovery、lease/backoff/dead-letter worker 和 delivery telemetry 仍是目标。旧
`publish_with` path 仅保留为 process-local gate proof。

### 4.7 目标：authoring boundary 的 compile-fail fixtures

下面两种写法在目标 API 中必须被类型或 macro validation 拒绝：deferred child 捕获普通
借用，以及 User subtree 声明 runtime capability。

```rust,ignore
// compile_fail: deferred component props must be owned or Arc.
#[view(component)]
fn borrowed_policy(props: &MountProps) -> PomView {
    view(Policy::new(&props.name))
}

// compile_fail: UserView accepts POM only, not ProvidedView.
fn user(cx: UserTurnContext<'_, CompileFailTurnProps>) -> UserView {
    user((
        cx.props().current_view.build_root()?,
        phrase_stream(Arc::clone(&cx.props().contract)),
    ))
}
```

相反，POM-only child 与多个已经 map 到 root channels 的 provided child 必须正常
compile。duplicate tag、binding id 或 provider capability identity 则在 mount 时失败，
且不能启动任何 runtime：

```rust,ignore
let definition = Arc::new(durable_system(duplicate_phrase_system));
let error = validate_durable_runtime_projection(&definition).unwrap_err();

assert!(matches!(error, MountError::DuplicateCapability { .. }));
assert_eq!(trace.count(LifecycleEventKind::InstantiateBinding), 0);
assert_eq!(trace.count(LifecycleEventKind::StartProviderRuntime), 0);
```

### 4.8 目标：call label、fork 与 retry identity

用户可以反复使用同一个观测 label。资源 identity 必须来自 runtime，而不是字符串：

```rust,ignore
#[tokio::test]
async fn repeated_labels_and_forks_get_distinct_runtime_scopes() {
    let first = agent.call("Phrase").execute(&source, &ready).await?;
    let second = agent.call("Phrase").execute(&source, &ready).await?;
    let fork = agent.forked().await;
    let third = fork.call("Phrase").execute(&source, &ready).await?;

    assert_eq!(first.epoch(), second.epoch());
    assert_eq!(second.epoch(), third.epoch());
    assert_all_distinct([
        first.turn_instance_id(),
        second.turn_instance_id(),
        third.turn_instance_id(),
    ]);
    assert_all_distinct([
        first.live_scope_id(),
        second.live_scope_id(),
        third.live_scope_id(),
    ]);
}
```

stream 尚未产生 event 的 transport retry 可以复用 rendered request。stream 已经产生
live event 后若仍允许 retry，必须先收到旧 scope 的 abort acknowledgement，然后用新
`ProviderAttemptId` 和 fresh binding：

```rust,ignore
assert_eq!(trace.sequence(), [
    Open(old_scope),
    Cancel(old_scope, AbortReason::ProviderFailed),
    AbortAcknowledged(old_scope),
    Instantiate(new_attempt),
    Open(new_scope),
]);
assert_ne!(old_scope, new_scope);
assert_eq!(trace.count(LifecycleEventKind::MountSystem), 1);
```

### 4.9 目标：reconfigure 的 epoch 原子性

in-flight turn 固定持有开始时的 epoch；candidate validation 和切换不能产生 mixed
bundle：

```rust,ignore
#[tokio::test]
async fn reconfigure_never_mixes_system_factories_or_capabilities() {
    let request_gate = RequestGate::new();
    let turn_a = agent
        .call("held")
        .start(&source, &request_gate.executor())
        .await?;
    request_gate.wait_until_request_started().await;

    // Pure definition construction: no System POM body is executed here.
    let candidate_b = PlayerEpochDefinition::new(
        EpochContractId::new("player/policy-b/v1")?,
        Arc::new(player_durable_system(config_b)),
    )?;
    let reconfigure = agent.submit_reconfiguration(
        EpochReconfigurationId::new("player/policy-b/change-42")?,
        candidate_b,
        HistoryRebase::CompactAndRebuild,
    );

    request_gate.release().await;
    let outcome_a = turn_a.await?;
    reconfigure.await??;
    let outcome_b = agent.call("next").execute(&source, &ready).await?;

    assert_eq!(outcome_a.bundle_digest(), digest(system_a, factories_a, capabilities_a));
    assert_eq!(outcome_b.bundle_digest(), digest(system_b, factories_b, capabilities_b));
}
```

另一个 fixture 必须让 candidate B 在 duplicate capability validation 中失败，并断言
active epoch、history 和 User cursor 完全未变。

## 5. 生产迁移示例

这一节不是重新发明三个 demo，而是把目标边界对照到现有 production behavior。通用
`LiveEffectRuntime`、grouped provider dispatcher、compensation report 与 isolated publication
gate 已存在；production `PlayerLiveRuntime`、Cube Stage ToolServer wiring 和 AgentLoop
integration 仍是迁移目标。

| 现有路径 | 必须保留的行为 | 目标 component |
|---|---|---|
| Forgotten City [`SelectIntentTool::on_open`](../../forgotten-city/crates/engine/src/player/agent.rs) | 每个合法 XML open 立即提交一个候选，并触发后续并行 phraser | `StreamingXml` + pure reducer + `PlayerLiveRuntime` |
| Forgotten City [`PhraseTool`](../../forgotten-city/crates/engine/src/player/agent.rs) | open 创建 streaming text，stream 追加 delta，complete 结束，abort 撤销 option | `StreamingXml` + stable phrase slot + compensating live scope |
| Cube Stage [`DirectorTurnSink`](../../cube_stage/src/director/rig_agent.rs) | 执行 provider-native tool，生成 tool result，提交 transcript 后继续下一步 | `ProviderTools` + provider capability plan；不是 `StreamingXml` |

### 5.1 Forgotten City SelectIntent

当前 `SelectIntentTool::on_open` 同时读 `WorldApi`、解析 XML、验证 handle，并向
`PlayerRuntime` channel 发送结果。迁移后 reducer 只做同步、确定性的 parse/validation；
异步的 authoritative validation 和 channel delivery 属于 host live runtime。

```rust,ignore
#[derive(Clone)]
struct SelectIntentTurnProps {
    batch: PlayerTurnBatch,
    allowed: Arc<IntentValidationSnapshot>,
    artifacts: Vec<TurnArtifact>,
    task: PlayerIntentTask,
}

#[derive(Debug)]
enum SelectIntentLive {
    StartPhrasing {
        scope: SelectionScope,
        batch: PlayerTurnBatch,
        sequence: u64,
        intent: SelectedIntent,
    },
}

enum PlayerLive {
    Intent(SelectIntentLive),
}

struct SelectIntentState {
    scope: SelectionScope,
    allowed: Arc<IntentValidationSnapshot>,
    next_sequence: u64,
    accepted: usize,
}

#[view(component)]
fn select_intent(contract: Arc<SelectIntentContract>) -> ProvidedView<PlayerChannels> {
    StreamingXml::new(contract.build_root()?)
        .try_state_with(|turn: &TurnBindingCx<'_, SelectIntentTurnProps, PlayerChannels>| {
            Ok(SelectIntentState {
                scope: SelectionScope::new(
                    turn.live_scope_id(),
                    turn.props().batch,
                ),
                allowed: Arc::clone(&turn.props().allowed),
                next_sequence: 0,
                accepted: 0,
            })
        })
        .on_open(reduce_select_intent_open)
        .finish(|state| Ok(StreamUpdate::output(SelectIntentOutput {
            accepted: state.accepted,
        })))
        .into_view()
        .map_channels(
            TurnChannelMap::<SelectIntentChannels, PlayerChannels>::builder()
                .output(PlayerOutput::from)
                .live(PlayerLive::from)
                .commit(Never::absurd)
                .diagnostic(PlayerDiagnostic::SelectIntent)
                .build(),
        )
}

fn reduce_select_intent_open(
    state: &mut SelectIntentState,
    element: &XmlElement,
) -> Result<StreamUpdate<SelectIntentChannels>, SelectIntentDiagnostic> {
    let intent = parse_selected_intent(element)?;
    state.allowed.validate(&intent)?;

    let sequence = state.next_sequence;
    state.next_sequence += 1;
    state.accepted += 1;
    let scope = state.scope.clone();
    let batch = scope.batch();

    Ok(StreamUpdate::live(SelectIntentLive::StartPhrasing {
        scope,
        batch,
        sequence,
        intent,
    }))
}
```

`PlayerLiveRuntime` 收到 `StartPhrasing` 后，仍要对当前 `WorldApi` snapshot 做
authoritative revalidation，再发送现有的
`PlayerRuntimeTaskOutput::SubmitSelectedIntent`。这样
`TurnBindingCx` 不需要 service handle，stale handle 也不会因为 prompt-time snapshot
而被错误接受。

这不是 commit-only effect。当前 production behavior 会立即启动 phraser；P6 必须为
`SelectionScope` 增加 `CancelSelectionAttempt`，取消该 attempt 启动的 phraser 并移除
tentative option。现有 batch generation 只拒绝 stale batch，不能撤销同一 active batch
里 selector failure 前已启动的工作。没有完成这条 gate 之前，不能声称 SelectIntent
已等价迁移。

User POM 保持应用定义的顺序，retry feedback 也是本轮 User 内容：

```rust,ignore
fn user(cx: UserTurnContext<'_, PlayerIntentTurnProps>) -> UserView {
    user(PlayerIntentUserDocument {
        context: DiffSlot::recursive("agent_context", cx.props().current_view.clone()),
        artifacts: cx.props().artifacts.clone(),
        feedback: cx.props().feedback.as_ref().map(ComponentPromptText::from),
        task: cx.props().task.to_pom(),
    })
}
```

### 5.2 Forgotten City PhraseTool

当前 `PhraseTool` 把 `WorldApi`、`TextWriter`、channel sender 和 parser state 放在同一
context，并用 `Drop` 发送 cancel。目标实现用 `(live scope, batch, sequence)` 组成稳定
且唯一的 `PhraseSlot`；reducer 只发 slot-based commands，runtime 独占 `TextWriter`。

```rust,ignore
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PhraseSlot {
    scope: LiveScope,
    batch: PlayerTurnBatch,
    sequence: u64,
}

#[derive(Debug)]
enum PhraseLive {
    Open { slot: PhraseSlot, selected: SelectedIntent },
    Append { slot: PhraseSlot, text: String },
    ParsedClose { slot: PhraseSlot, full_text: String },
}

#[view(component)]
fn phrase_tool(contract: Arc<PhraseContract>) -> ProvidedView<PhraserChannels> {
    StreamingXml::new(contract.build_root()?)
        .try_state_with(|turn: &TurnBindingCx<'_, PhraserTurnProps, PhraserChannels>| {
            Ok(PhraseState::new(
                PhraseSlot {
                    scope: turn.live_scope_id(),
                    batch: turn.props().batch,
                    sequence: turn.props().sequence,
                },
                turn.props().intent.clone(),
            ))
        })
        .on_open(reduce_phrase_open)
        .on_stream(reduce_phrase_delta)
        .on_complete(reduce_phrase_parsed_close)
        .map_live(PhraserLive::Phrase)
        .into_view()
}
```

Runtime 侧的资源表是 effect interpreter state，不是 component/reducer state：

```rust,ignore
struct PhraseLiveRuntime {
    world: WorldApi,
    task_output: mpsc::UnboundedSender<PlayerRuntimeTaskOutput>,
    open: HashMap<PhraseSlot, OpenPhrase>,
}

struct OpenPhrase {
    writer: TextWriter,
    text_id: TextId,
    parsed_close: bool,
}

impl LiveRuntime<PhraseLive> for PhraseLiveRuntime {
    async fn apply(&mut self, effect: PhraseLive) -> Result<(), LiveError> {
        match effect {
            PhraseLive::Open { slot, selected } => self.open_phrase(slot, selected).await,
            PhraseLive::Append { slot, text } => self.append(slot, text),
            PhraseLive::ParsedClose { slot, full_text } => {
                self.record_parsed_close(slot, full_text)
            }
        }
    }

    async fn cancel_scope(
        &mut self,
        scope: LiveScope,
        reason: AbortReason,
    ) -> Result<(), LiveError> {
        self.cancel_all_open_phrases(scope, reason).await
    }

    async fn commit_scope(&mut self, scope: LiveScope) -> Result<(), LiveError> {
        self.complete_phrases_and_release(scope).await
    }
}
```

`record_parsed_close` 可以保存完整文本，但不能发送
`PlayerRuntimeTaskOutput::CompletePhrasedOption`。只有整个 agent turn 成功 publication
后的 `commit_scope` 才把 option 标为 ready 并释放 compensation record。这样
`<phrase>` 已闭合后 provider 又失败时仍能发送 `CancelPhrasedOption`。这比当前依赖
`PhraserParseContext::drop` 更明确，也支持 async cleanup 和错误诊断。

这是一个有意的 failure-semantics tightening：文本 delta 的可见时序保持不变，但 option
从 XML close 到可选择 ready 的时点会推迟到 turn publication。P7 的 golden trace 必须
把这项行为变化单独验收，不能称为完全无行为变化。

### 5.3 Cube Stage provider-native tools

Cube Stage 的 tool call 是 provider response 中的 typed message，不是模型文本里的 XML。
当前 `DirectorTurnSink` 顺序执行 `ToolServer::call_tool`，把成功或失败都编码为
`Message::tool_result_with_call_id`，然后 `commit_turn` 把 transcript 提交并通过
`TurnFlow::Continue` 开始下一步。目标 component 必须保留这条 round trip：

```text
Provider ToolCall
  -> bound DirectorToolRuntime (external I/O)
  -> ToolResult transcript
  -> publish turn
  -> TurnFlow::Continue
  -> rerender User diff
  -> next provider request with the same mounted System epoch
```

目标 authoring 可以是同一个 component 同时声明 tool-use POM 和 provider capability：

```rust,ignore
#[view(component)]
fn director_tools(config: Arc<DirectorToolConfig>) -> ProvidedView<DirectorChannels> {
    view((
        DirectorToolRules::new(),
        ProviderTools::new(DirectorToolDefinitions::all())
            .runtime_factory(DirectorToolRuntimeFactory::new(
                Arc::clone(&config.services),
            ))
            .tool_choice(ProviderToolChoice::Required)
            .errors_as_results()
            .map_output(DirectorOutput::Transcript),
    ))
}

fn system(cx: SystemMountContext<'_, DirectorMountProps>) -> SystemView<DirectorChannels> {
    system((
        DirectorPolicy::new(cx.props().policy.clone()),
        DirectorPromptBoardContract::default(),
        director_tools(Arc::clone(&cx.props().tools)),
    ))
}

fn user(cx: UserTurnContext<'_, DirectorTurnProps>) -> UserView {
    user((
        DiffSlot::recursive("prompt_board", cx.props().current_view.clone()),
        DirectorStepTask::new(cx.props().step, &cx.props().task),
    ))
}
```

`ProviderTools` 是目标 spelling；当前 public API 是 `provider_tools[_with_context]`，同样是
provided component，不是 `StreamingXml` 的 mode。它的 schema 不会被 POM renderer 序列化，
而是作为 mounted provider capability 与 System epoch 一起验证。一个 declaration 可包含多个
tool schema，并在每次 attempt 创建一个共享 state 的 dispatcher group；普通 POM child 和
provider declaration 仍通过同一个 component tree 组合。

当前 isolated runtime 已有 `ProviderCapabilityPlan + per-attempt runtime factory`：
`PreparedUserTurn::start_provider_attempt` 可执行 native-only group，带 XML binding 的 harness
由 combined streaming attempt 执行同一 dispatcher runtime。call 按 provider order 串行，tool
error 作为模型可见 result，dispatcher infrastructure failure 为 terminal；每次 call 的 Live
必须确认后才返回 result。result 会累积进 `TurnPublication` 供 session transcript 记录，只有
Commit 在 publication 后释放。`AgentTurnRequest` 仍没有 capability 字段，`LLMExecutor` 也尚未
接收 bound dispatcher，因此 Cube Stage 不能标成 component migration complete。

当前 attempt 已用 `(epoch, turn instance, capability id, invocation_id)` 形成
`ProviderInvocationKey`；provider result 所需的 optional correlation id 在 call/result/publication
中独立保留。相同 invocation id、correlation 与完整 payload 会 replay 内存 result，不再执行
I/O 或 effects；同 invocation id 不同 correlation/payload 是 terminal collision。对外部
mutation 的 durable idempotency、
跨 attempt replay 和 event-store/outbox 仍需应用/AgentLoop 定义，禁止在已经开始 tool I/O 后
盲目 transport retry。

```rust,ignore
#[tokio::test]
async fn provider_tools_preserve_order_results_and_idempotency() {
    let dispatcher = RecordingDirectorDispatcher::new();
    let provider = ScriptedNativeTools::respond_with([
        tool_call("call-1", "append_planned_dialogue", append_args),
        tool_call("call-2", "edit_planned_dialogue", edit_args),
    ]);

    let first = agent
        .call("director")
        .execute_with_provider_tools(&source, &provider, dispatcher.clone())
        .await?;

    assert_eq!(dispatcher.invocations(), ["call-1", "call-2"]);
    assert_eq!(first.tool_result_ids(), ["call-1", "call-2"]);

    let replay_key = ProviderInvocationKey::new(
        first.epoch(),
        first.turn_instance_id(),
        "call-2",
    );
    let mutation_count = dispatcher.external_mutation_count(replay_key);
    let replay = dispatcher.invoke(replay_key, edit_call).await?;

    assert_eq!(replay, first.tool_result("call-2"));
    assert_eq!(dispatcher.external_mutation_count(replay_key), mutation_count);
}
```

当前 isolated tests 已覆盖 expected tool error 和 unknown tool 均返回 result、dispatcher
infrastructure failure 终止 attempt、exact replay 不重复 I/O/Live，以及 collision 终止 attempt。
上面的 fixture 仍是 durable extension 的目标：publication/recovery 若重放 `replay_key`，必须
命中同一 durable receipt；这不是当前 in-memory replay 的保证。

### 5.4 显式 reconfigure

普通 component、model、reducer 和 compactor 都不能改变 System。host 先离线构造
candidate definition 并验证它的 POM-free runtime projection，再在 turn boundary 原子切换
epoch。下面名称是目标 vocabulary，不是已经冻结的 Rust API：

```rust,ignore
let candidate = PlayerEpochDefinition::new(
    EpochContractId::new("player/policy-next/v1")?,
    Arc::new(player_durable_system(PlayerSystemProps {
        policy: new_policy,
        contracts: new_contracts,
    })),
)?;

let submission = agent
    .submit_reconfiguration(
        EpochReconfigurationId::new("player/policy-next/change-42")?,
        candidate,
        HistoryRebase::CompactAndRebuild,
    )
    .await?;
assert!(matches!(submission, ReconfigurationSubmission::ActorAccepted { .. }));
```

`PlayerEpochDefinition::new` 只能构造 retained definition 和检查 POM-free declarations；
它不能执行或渲染 candidate System POM。host binding 而非 component author 保留
`HostConfigurationFingerprint` 与 `RuntimeImplementationFingerprint`，并从 provider、persistence、
reducer 与 Live-runtime 配置派生它们。只有 store 返回 winner `Create` 后，detached owner
才执行 System tree 一次。失败 candidate 不影响 active epoch。已经开始的 turn 持有旧 epoch
的 `Arc` 并以旧 bundle 完成；切换后的下一轮只能看见完整的
`{System B, factories B, provider capabilities B}`。不允许出现 `System A + factory B`，
也不会往 history 追加第二条 System message。

## 6. Subagent 评审反馈

本轮使用四个相互独立的视角评审方向与示例。下面保留的是会改变 API 或迁移顺序的
意见，不记录措辞类建议。

### 6.1 Component authoring ergonomics

第一位 reviewer 检查了 component 作者实际会写什么，结论如下：

- reusable streaming state 必须使用 `.state_with(...)` / `.try_state_with(...)`，不能把
  单轮 state 放在 mounted component 中。
- output、live、commit、diagnostic 必须分 channel mapping；保留一个模糊的
  `.map_effect(...)` 会把执行阶段重新泄漏到 application code。
- streaming factory 只能从 System mount subtree 声明；User 中 conditional mount
  capability 必须报错。
- reducer 要先产出 owned value、释放 state borrow，再 await host runtime；isolated mounted
  path 已按此实现，reducer 自身仍不执行 I/O。
- live callback 默认保持顺序和 backpressure；abort/cancellation 不是可选扩展。isolated host
  runtime 已接管，AgentLoop 的 managed cancellation 仍待迁移。

这些意见已作为目标 contract 接受。原型中的 `.init_state(value)`、单一 effect accumulator
和 User-side binding 都不能直接进入稳定 API。

随后对 executable channel spike 的复审又发现两个具体缺口：单 lane helper 无法表达
SelectIntent 的 `on_open -> Live` 与 `finish -> Output`，而 parser closing chunk 不会再触发
`on_stream`。当前实现已增加完整 `TurnChannelMap` 与 `StreamUpdate`；runnable phrase example
在 `on_complete` 补齐最后 append，再产生 `ParsedClose`。`Result<Vec<E>, D>` 只作为
non-terminal diagnostic 兼容写法保留；binding identity、callback phase 和 terminal failure
已在 isolated mounted path 中由 `BindingOrigin`、`BindingPhase`、`BindingFault` 和 shared
`HermesParser` 建模；把它们传播到 `TurnSink`/AgentLoop 仍是 P4 工作。二次复审发现的 event 类型泄漏、外部 mapper 改写 lane，以及
multi-lane view 误用 single-lane shortcut，也已经分别由 text-event bound、框架内部 lane
match 和 nominal `StreamingChannelsView<C>` 封住。最终复审发现通用 `view(...)` 曾能将
multi-lane wrapper 解包回 raw view；现在 single-lane API 只属于
`StreamingValueView<E,D>`，binding remap 也不再公开，因此 generic composition 不能恢复
这些方法。两条绕路均有 compile-fail fixtures。

### 6.2 Rust type 与 agentloop feasibility

第二位 reviewer 对照当前 `ComponentNode<B>`、`HookPlan<B>`、`TurnSink` 和 agentloop，
指出四个 P0：

1. deferred `ComponentCall` 不能安全持有普通借用。child component props 必须 owned、
   `Arc`，或让整个 IR 显式带 render lifetime；首版已选择 owned/`Arc` 并加入 compile-fail
   coverage。
2. 当前 homogeneous `HookPlan<B>` 无法自然组合 POM-only child、多个不同 streaming
   factory 和 provider tools。目标 compiler 需要在 root channels 上归一化后 type-erase
   各 child factory。
3. 当前 `TurnSink::on_event` 返回 `()`，executor failure 也没有 sink abort callback，
   无法表达 live runtime failure 和 acknowledged cleanup。
4. `MountedEpoch.rendered_system` 与 mutable `PromptContext.system` 不能同时作为权威来源。
   POM path 只以 epoch bundle 为准；session 最多持久化 epoch id/digest 做 resume 校验。

建议的最小 channel 边界是：

```rust,ignore
trait TurnChannels {
    type Output: Send + 'static;
    type Live: Send + 'static;
    type Commit: Send + 'static;
    type Diagnostic: Send + 'static;
}

struct BindingFactoryPlan<I, Props, C: TurnChannels> {
    factories: Vec<MountedFactory<I, Props, C>>,
}
```

Provider ingress is not a component channel. The mounted text host normalizes
provider deltas to its internal text event stream before dispatching XML
callbacks; component authors declare only typed Output, Live, Commit, and
Diagnostic lanes.

`PomView` 不携带 runtime type，可以放进任意 role tree。单 lane child 可以通过
`.map_output/.map_live/.map_commit/.map_diagnostic` 归一化；multi-lane child 必须用完整
`ChannelMap<Local, Root>`。具体 factory/instance type 可以在仍保留其 channel `C` 的 typed
carrier 内 erase。当前并行
`MountProvidedView<C, TurnProps> -> MountPlan<C, TurnProps>` spike 已证明两个不同 factory 和一个 provider tool
可以共处一个 component、共享一次 channel mapping，再按 source order 拆成两个 plan；
duplicate route/tool name 与 User/unplaced declaration 都在零实例化时失败。它尚未替换
AgentLoop 使用的 `HookPlan<B>`。

root channel generic 不需要继续泄漏进 AgentLoop 的内部存储。这里的“一次 erasure”特指
root channel contract；具体 reducer/factory implementation type 可以在仍保留 `C` 的 typed
carrier 内提前 erase。epoch storage 与 per-attempt driver 是两个不同的边界，确定顺序是：

```text
typed local component channels
  -> ChannelMap<Local, Root>
  -> MountPlan<Root>
  -> one-time mounted-epoch storage erasure

typed attempt + typed interpreter
  -> one-time attempt-owner erasure
  -> non-generic AgentLoop storage
```

#### 6.2.1 决策记录：root contract 的擦除时机

**结论：`PlayerChannels` 可以做 type erasure，但只能在 local component 已经通过
`TurnChannelMap<Local, Root>` 归一化为 root contract 之后做一次。** 这解决的是
AgentLoop 内部不必为每个 harness 持有不同 generic 参数的问题；它不改变 component
authoring，也不能把类型检查延后到运行时。

| 阶段 | 可以安全擦除 | 仍必须保留或尚不能做 |
|---|---|---|
| 当前 typed carrier | reducer state、具体 factory、具体 binding instance、具体 live runtime；现有 `Box<dyn BindingInstance<C>>` 和 `ErasedLiveEffectRuntime<L>` 已采用此模式 | `C` 与每条 root lane 仍静态；`TurnEmission<C>` 不能变成无标签 payload |
| executable provider dispatcher（已实现） | concrete dispatcher 已由 `ErasedProviderDispatcher<C>` 隐藏；每个 group 的 state 只属于一个 attempt | `C` 与五条 lane 仍静态；public API 不暴露 `Any`，native result/replay/collision/terminal state 仍必须保留语义 |
| mounted epoch erasure（已实现） | `MountedEpoch<C, Props>` 已包住同一种 non-generic private storage；`ChannelTypeInfo` 保存 root、五条 lane 与 props descriptor，所有 `Any` downcast 只在 framework 内 | active attempt 仍是 typed handle；lane、terminal state、publication/abort identity 不能合并成一个 effect bag |
| object-safe attempt driver proof（已实现，crate-private） | 一个 non-generic Active/Finished/legacy-Published owner 驱动完整 `MountedStreamingAttempt<C>`；Text/XML 与 native tool 共用 parser、dispatcher、identity、Live scope 和 Commit buffer | typed update/Commit interpreter 在 erase 前安装；lane payload 不经过 `Any`；这里的 Published/Commit retry 仅为 process-local proof |
| managed pre-publication owner（已实现，crate-private） | actor 独占 Active/Finished state；terminal failure 先 await abort 再回复；durable staging adapter 在 root erasure 前安装，成功启动 publication actor 后旧 owner 才进入 `Transferred` | 尚未进入 AgentLoop；in-flight Live callback 必须先返回才能继续 cleanup；staging failure 会恢复同一个 Finished owner 和 durable plan |
| authoritative `MountedAgent`（private owner + public local facade 已实现） | 拥有 epoch/session/durable revision/generation；opaque public owner 在 durable admission 后返回 call，observer drop 不取消 work，call-scoped cancel 会等待 cleanup join；每个 replacement/Continue 前 async capture owned turn props 并只重渲染 User；final Ready 才初始化 attempt；detached owner 保留 cleanup/turn lock，receipt 后原子写回 session + revision；immutable binding 固定 store/reducer/request-id/publication/attempt policy，只有 `MountedEpochDefinition` 可被 durable reconfigure 替换；typed `TurnRecord` 线性交给 pure reducer；同一个 mount-owned session store 在 User/provider 前 claim `Reserved`，final Ready 前原子提升为 `Running`，并在 publication 事务校验同一未过期 Running lease；Reserved 失败/过期恢复稳定 checkpoint，只有 Running expiry 进入 RecoveryRequired；完整 ledger 保留 terminal calls；settled duplicate 返回 public `MountedCallOutcome::Executed | Replayed`，replay 只保留 durable result；pending identity 可在 reopen 时 reconcile；expired `RenderStarted` 通过 exact fence/tombstone transaction 返回 epoch-A snapshot 并在 owner lock 内安装 | `InMemoryMountedAgentFactory` 只提供 process-local、`Commit = Never`、无 reconfigure 的 lifecycle proof；production recovery/store、public builder ergonomics、post-activation B reconstruction 与真实 AgentLoop 迁移仍未完成 |
| durable publication contract（已实现，public isolated API） | root `C` 尚未 erase 时一对一 stage Commit；pure host fingerprint factory 随后看到完整 mutation/output/results/ordered outbox；same-id collision 由 fingerprint definite reject，receipt/resolve 都回显并校验它。通用 contract 不序列化 generic values；AgentView-owned `DurableMountedAgentFactory` 另为其 private session mutation 和 provider JSON 提供 recursive-key canonical fingerprint schema v2，并在 durable state schema 2 中拒绝旧 fingerprint state | concrete store、restart recovery 与 worker 尚未实现；host 仍负责 typed outbox payload 的 versioned canonical encoding；无序集合必须先投影为稳定排序序列，framework 不会把 JSON array 猜成 set |
| managed durable publication（已实现，crate-private） | root-channel-erased handoff handle 隐藏 `C`、mutation、payload 与 concrete store，同时保留 host 的 revision/error 类型；mailbox 接受 publish 后由 actor 持有 store future；caller cancellation 不会丢 candidate；indeterminate write 用同一 request id resolve；transient resolve failure 使用 capped exponential backoff；definite fingerprint collision 对当前 candidate 是 terminal non-commit；durable outcome 不暴露 Commit | 已与 managed attempt 做 actor-to-actor isolated handoff，但尚未进入真实 AgentLoop；runtime shutdown/restart recovery handoff 与 production telemetry 仍需 owner 定义 |

`TurnProps` 也不能被无检查地擦除。`TurnBindingCx<'_, TurnProps, C>` 借用 exact props，且
`PreparedUserTurn` 保证 User POM 与 factory 初始化使用同一份值。因此 erased adapter 若接收
`&dyn Any`，必须在 adapter 内通过一个为 `TurnProps` 单态化的 trampoline 检查并立即借用；返回的
attempt state 不得保留该借用。对外更好的 API 是 `MountedAgent<Root, TurnProps>` 继续保持 typed
props，只有 framework 内部把它转交给 erased epoch operations。

`PlayerChannels` 一类 root marker 本身没有运行时数据；它在编译期绑定 `Event`、`Output`、
`Live`、`Commit` 和 `Diagnostic`。安装 epoch 时可以为 root contract 和五个 associated type
保存 `TypeId + type_name` descriptor，并把具体 factory/instance 包装成 object-safe erased
adapter。descriptor 只能检查类型和生成诊断，不能替代 reducer/interpreter 行为；adapter 或
vtable 必须保留 monomorphized typed implementation，并独占所有 `Any` downcast。

当前 mounted-epoch storage 已采用下面的 descriptor 与 typed-handle 形状；
crate-private `ErasedAttemptOperations` proof 也已落地，但还没有安装进 AgentLoop：

```rust,ignore
struct TypeSlot {
    type_id: TypeId,
    type_name: &'static str,
}

struct ChannelTypeInfo {
    root_channels: TypeSlot,
    event: TypeSlot,
    output: TypeSlot,
    live: TypeSlot,
    commit: TypeSlot,
    diagnostic: TypeSlot,
    turn_props: TypeSlot,
}

struct ErasedMountedEpoch {
    channels: ChannelTypeInfo,
    state: Box<dyn Any + Send + Sync>,
}

struct ErasedAttemptRuntime {
    operations: Box<dyn ErasedAttemptOperations>,
}

struct MountedEpoch<C: TurnChannels, Props: ?Sized + 'static> {
    inner: Arc<ErasedMountedEpoch>,
    marker: PhantomData<fn(&Props) -> C>,
}
```

当前 private epoch adapter 通过 descriptor 检查并由 typed `MountedEpoch<C, Props>` 单态化地
恢复 epoch state。`ErasedAttemptOperations` 按 wire order 接收封闭的 provider text/tool input、
完成 stream、交给 publication boundary，或显式 abort。单态化 shim 在 erase 前已经装入
`AttemptUpdateInterpreter<C>`；每个 `StreamUpdate<TurnEmission<C>, C::Diagnostic>` 直接交给
这个 typed interpreter，object-safe owner 只看到 provider-neutral acknowledgement/tool result。
因此 attempt driver 没有 `Any` payload、lane downcast 或无标签 effect bag。

descriptor 需要记录 `Root`、五个 channel associated type 与 `TurnProps` 的 `TypeId` 和
`type_name`。它们只用于 compatibility check 与诊断，绝不能用来“解释”一个 payload。当前
attempt proof 在 reducer/dispatcher 实例化之前执行完整 descriptor preflight，然后只调用已经
单态化的 typed interpreter。若 contract 要跨进程、动态库、持久化或跨 build 识别，还必须显式
定义 stable contract key，不能把 `TypeId` 或 `type_name` 当 durable identity。

lane tag 必须保留在 opaque payload 外部，所以 erasure 后仍不能把 `Live` 当成 `Commit`。
component 作者和 application interpreter 不接触 `Any` 或手写 downcast。

mounted epoch 只保存 reusable erased factories 和静态 provider capability declarations。
每次 provider attempt 从 factory 创建新的 `ErasedAttemptRuntime`，由它独占 binding instance、
dispatcher、事件串行化和 abort/finish 状态；attempt 结束后不能把这些状态放回 epoch 或给下一次
attempt 复用。

provider dispatcher 的 prerequisite 已完成。`ProviderDispatcher<C>` 现在有 async `dispatch`、
`finish` 和 explicit `abort`，concrete dispatcher 由 `ErasedProviderDispatcher<C>` 在仍保留 root
`C` 的 carrier 内隐藏。`ProviderDispatchRuntime` 每个 attempt 实例化 dispatcher groups、路由
tool name、串行执行 call、在相同 payload replay result、拒绝 collision，并将 expected tool error
与 terminal infrastructure failure 分开。它与 XML parser 共用 `ProviderAttemptIdentity`、
`LiveEffectRuntime`、private Commit buffer 和 publication typestate；isolated tests 已覆盖这些
行为以及 abort report。

`ChannelTypeInfo`、root descriptor、一次性 mounted-epoch storage erasure 与 object-safe attempt
driver proof 均已实现。两个不同 root harness 可同时进入同一种 non-generic active/finished/
published storage；测试覆盖 Text event、native call、typed Output/Diagnostic、awaited Live、
publication-retained Commit、finish/publish/abort 以及 mismatch-before-instantiation。下一步是让
AgentLoop 成为这个 driver 的唯一 owner，而不是再引入第二次 erasure 或让 component 作者处理
`Any`。

sink/abort 的最低能力应接近：

```rust,ignore
trait TurnSink<E>: Send {
    type Output: Send;
    type Finished: Send;
    type FinishFailure: Send;

    async fn on_event(&mut self, event: E) -> Result<(), TurnRuntimeError>;
    async fn finish_stream(self) -> Result<Self::Finished, Self::FinishFailure>;
}

trait FinishedTurn: Send {
    type Published: Send;
    type PublishFailure: Send;

    async fn publish_with(self, publisher: &mut dyn TurnPublisher)
        -> Result<Self::Published, Self::PublishFailure>;
    async fn abort(self, reason: AbortReason) -> AbortReport;
}
```

这是 legacy phase-ordering 的 typed contract 草图，不是 production publication API，也不是可以
直接做成 `dyn TurnSink` 的最终 object-safe spelling。当前 isolated API 已采用不同但更严格的
owned transition：
`MountedStreamingAttempt::finish_stream(self) -> Result<FinishedStreamingAttempt<_>,
StreamingFinishFailure<_>>`，随后只能 `publish_with(self, &mut publisher).await` 或
`abort(self, reason)`。publication 成功返回带不可公开构造 receipt 的
`PublishedStreamingAttempt`；失败保留 finished attempt 供显式 abort。durable path 不扩展这个
trait，而是在 finished typed boundary 先 stage Commit 与 complete mutation，再把 store transition
交给 managed publication owner。
erased adapter 需要使用 `async_trait` 展开或显式返回 boxed `Future`，并要求 vtable 持有的
future 与 wire input 满足 `Send + 'static`。typed event/output payload 在 shim 内解释，不穿过
`Any` 或 object-safe wire 边界。这里不是要求立刻冻结 trait
spelling，而是要求 P4 不能继续依赖 infallible `TurnSink` callback 和 implicit `Drop` cleanup。

### 6.3 Forgotten City 与 Cube Stage migration

第三位 reviewer 逐行对照两个生产仓库，给出三个必须保留的事实：

- `SelectIntentTool::on_open` 当前会立即发送 selection 并启动 parallel phraser，所以
  它是 live effect，不是 commit effect。等价迁移需要
  `(epoch, turn instance, selector attempt, sequence)` lease 和
  `CancelSelectionAttempt`。
- Phrase 的 XML close 只表示 `ParsedClose`。只有 provider 与 session publication 都
  成功后才能把 tentative option 变成 final `Complete`；所有失败路径都走 `Cancel`。
- Cube Stage tool schema 属于 provider request metadata，tool execution 是外部 I/O，
  result 是 typed tool transcript。它需要 `ProviderToolPlan + per-attempt dispatcher`，
  不能用 System POM 或 `StreamingXml` 冒充。

Cube 的 mutation tool 可用当前 `(epoch, turn instance, invocation_id)` 做 attempt-local replay
correlation，但 durable idempotency key 必须来自持久化的 session/revision 或 outbox identity，
再结合 stable invocation id。provider transcript correlation id 是另一字段，不能替代 mutation
identity。tool error 保持当前语义，转换为模型可见 result；同一 response 的多个 call 保持
provider order。

### 6.4 Dioxus 边界对照

第四个视角检查了 Dioxus component core。可以借鉴的是 typed owned props、deferred
component identity、pure render 与 commit phase 分离；不应复制的是 VDOM mutation、
signals、position-based hook state、Suspense、implicit context lookup 和通用
`use_effect`。

AgentView 已有 POM resolver、canonical renderer、DiffSlot 和 session transaction。
缺的是 lifecycle-controlled component execution，不是第二套 renderer 或 scheduler。

### 6.5 Mounted lifecycle 与 factory 复审

冷启动 API 使用者和独立 code reviewer 对新 slice 给出三条会改变实现的意见：

- `mount_system_epoch` 当前只保证每次调用执行 System function 一次；logical epoch 只调用
  一次要等最终 AgentLoop owner 强制。function pointer 能禁止 captured closure，但不能禁止
  named function 读取 global/thread-local/I/O，也不能判断 mount props 是否混入 turn data。
- 一个 provider attempt 必须只有一个 text parser/router。若每个 binding 各自解析完整 chunk，
  `<b/><a/>` 会被 factory order 重排，还会重复 parse。当前 mounted attempt 已改为 shared parser，
  并有 cross-route wire-order regression test。
- `finish_stream` 必须形成线性状态转换：当前 `finish_stream(self)` 消费 live attempt，成功时
  返回仍拥有 reducer state 的 `FinishedStreamingAttempt`，失败时返回仍拥有 attempt 的
  `StreamingFinishFailure`。两者都保留 `abort(self, reason)`。旧 proof 可通过 awaited
  `TurnPublisher` 进入 `PublishedStreamingAttempt`；durable proof 则可先进入
  `StagedStreamingPublication`，并在 authoritative durable receipt 后变成不暴露 Commit 的
  `DurablyPublishedStreamingAttempt`。Live scope compensation 和 local teardown 的组合 report
  已实现；AgentLoop ownership 仍待接入。

## 7. 已确认决策与待定问题

### 7.1 当前边界与目标 owner contract

**当前已实现的 isolated 边界：**

- System 与 User 使用同一种 POM `Document`。`SystemView<C, TurnProps>` / `UserView` 是
  lifecycle 与 capability seal，不是两套 POM；`mount_system_epoch` 保存 raw/resolved/rendered
  System 和绑定/capability plan。
- `mount_system_epoch` 每次调用只执行一次 System render；`prepare_user` 不遍历 System；
  `MountedTurn` 为 logical turn 分配 identity，并可多次创建 fresh provider attempt。
- runtime factory 只能由 System subtree 声明。每个
  `prepare_user(&props, ...)` 返回绑定 exact props 的 `PreparedUserTurn`；只有它能用
  `start_streaming_attempt(live_runtime)` 创建 fresh parser/reducer/binding/dispatcher state，或在
  native-only harness 中用 `start_provider_attempt(live_runtime)` 创建 fresh dispatcher groups。
  User root 不能声明 runtime declaration，也不能用另一份同类型 props 初始化 reducer/dispatcher。
  即使使用 low-level `binding_factory[_with_context]`，也必须同时传入 prompt-facing `Document`；
  POM contract 与 runtime declaration 返回为同一个 view，不能单独注册隐藏 route。
- `TurnBindingCx` 只给 typed props、opaque identity、binding id 和 route。它不暴露 context、
  service、executor 或 mutable session，且 state 不能借用 props。
- `ProviderDispatcherCx` 同样只给 exact typed props、attempt identity、capability id 与 mounted
  specs。`dispatch` 是显式 host I/O boundary：expected tool/domain failure 返回 model-visible
  `ProviderToolResponse::Error`，而 dispatcher/Live/finish/abort infrastructure failure 终止
  attempt。多个 specs 可共用一个 grouped dispatcher；call 按 provider order 串行，exact replay
  不重复 I/O/effect，collision 终止 attempt。
- reducer 在 parser event 到达时实时、串行、pure 地执行，返回 owned
  output/diagnostic/live/commit emission。Live 在 callback 内交给 host runtime 并 await；returned
  update 只有 Output/diagnostic，Commit 留在 private buffer。`BindingFault` 为 pure callback
  failure 归因，`LiveEffectFault` 为 host I/O failure 归因。
- native dispatcher update 使用相同的 awaited Live runtime、private Commit buffer、attempt
  identity 和 publication typestate；call 会即时返回 tool result，同时累计该 result 并在
  `TurnPublication` 中交给 host publisher。旧 proof publication 成功后才允许读取 Commit；durable
  path 会在成功前把 Commit stage 成 outbox payload，并在成功 handle 中只保留已记录的 results。
- `MountedEpoch<C, TurnProps>` 已是 typed public handle over non-generic private storage；
  `ChannelTypeInfo` 保存 root、Event/Output/Live/Commit/Diagnostic 与 TurnProps 的 process-local
  descriptor。不同 root harness 可进入同一种内部 storage，`Any` 与 downcast 不进入 public API。
- native finished/published handle 可用 `take_update().into_parts()` move 出非 `Clone` 的
  Output/Diagnostic；provider result 独立保存 replay `invocation_id` 与 optional transcript
  correlation id。missing/empty invocation id 不会进入 dispatcher I/O，而是返回 model-visible
  `invalid_invocation_id`；provider Live/collision/publication failure 仍保留关联 metadata。
- `finish_stream(self)`、`FinishedStreamingAttempt` 与 `StreamingFinishFailure` 保证成功和
  失败的 reducer state 都有显式 abort 路径。`publish_with` await host publisher 后才返回
  legacy `PublishedStreamingAttempt`；durable path 在 publish 前 stage Commit，返回的
  `DurablyPublishedStreamingAttempt` 没有 `pending_commits()`。
- crate-private object-safe driver 已把完整 combined attempt 包进一个串行 vtable，并保留
  Active -> Finished -> Published/Aborted typestate。finish/publication failure 持有可 abort 的
  旧 state；tool result 只在 typed Output/Diagnostic interpreter 成功接收对应 update 后返回。
  两个不同 root harness 已共用同一种 non-generic driver storage，lane payload 不经过 `Any`。
- crate-private managed attempt actor 已接管 driver 的 Active/Finished state。一次 drive/finish
  command 进入 mailbox 后，即使调用它的 future 被取消，actor 仍把 state 推进到稳定状态；最终
  client 消失时会 await `abort(Cancelled)`。durable finalizer 在 root channel 尚未擦除时安装；
  `BeginDurablePublication` 在旧 actor 内同步构造 mutation、stage Commit、生成 fingerprint 并启动
  新 actor，随后才把旧 state 标记为 `Transferred`。staging failure 恢复同一个 Finished state 和
  unchanged durable plan；成功返回的 handle 不含 root `C`，但保留 host revision/error 类型，
  publication 后的责任只属于新 actor。
- crate-private `MountedAgent` 已拥有 authoritative epoch/session/durable revision snapshot、turn
  lock 与 generation。mount 会把 epoch 的唯一 System snapshot 绑定到 session；durable mount 同时
  接收已加载 session 的 CAS revision。borrowed `CallProps` 在
  `pin()` 后覆盖整个 logical call，`capture_turn_props` 则在每个 candidate 前从 read-only draft、
  source、call props 与 label 生成 owned `TurnProps`。每次 `ReplaceHistory` 都先更新 draft、清空旧
  User cursor，再重新 capture 并只重渲染 User；普通 request 只携带 history/User，System bytes 和 tool
  catalog 在 epoch open 时已经通过 `attach_epoch` 固定到同一个 opaque provider binding。
  只有 final Ready candidate 会创建 binding/dispatcher state。provider 成功后返回
  publication-pending Finished owner；显式取消、handle drop，以及 pending `complete` / `cancel` /
  Finished `abort` future 被 drop 时，detached transition owner 都会保留 turn lock 直到 cleanup ack。
  keeper thread 驱动 current-thread 或 multi-thread Tokio runtime。`prepare_context` deadline 依赖
  pure/read-only/drop-safe contract；provider 超过 cancellation grace 时返回独立
  `ProviderJoinTimedOut`、abort 本地 attempt，并永久 poison 后续 call/reconfigure。fork 与显式
  reconfigure 已存在，generation 不允许回绕。成功 completion 先生成完整 pure session mutation，
  再把 owned publication plan 交给 durable actor；只有 validated receipt 才会在 detached owner 中
  原子写回 session、User cursor 与 next revision。`TurnFlow::Continue` 保留同一 turn lock/epoch，
  使用 receipt revision 重新 capture 并只渲染下一份 User POM。非连续 revision、caller 取消、
  indeterminate resolve、NotCommitted、rejection/collision 均有 owner-level 测试。
  abort report 只有在 Live compensation 与每个 native dispatcher 都明确 acknowledged 时才算
  cleanup ack；host compensation failure 会 poison owner，不能仅凭本地 reducer teardown 解锁复用。
  durable reconfigure 只接收 `MountedEpochDefinition`，并显式 rebase history；它不能携带另一份
  store、reducer、request-id、publication 或 attempt policy。same-owner call 在切换前完整持有 A，
  cross-owner running call 在 System render 前 fence candidate；Activated retry 只 rebind/rehydrate，
  `RenderStarted` crash 不重新执行 POM。其 lease 过期后，store 只允许 exact-fence abort，推进 revision、
  tombstone 同 id、丢弃 staged rebase，并在同一事务返回 epoch A snapshot；`Rendered` 只能 resume attach。
  activation 后 waiter 被取消仍会 poison A owner，并可由新 owner POM-free reopen B；当前 owner 还不能只靠
  A definition 重建 B。需要替换 harness、source adapter、capture/User
  或其他 host 行为时必须 mount 新 agent，不能把它伪装成一次 System reconfigure。
- crate-private Published shim 已在 session publication 成功后调用 typed Commit interpreter。
  delivery failure 保留相同 batch/interpreter，retry 不重跑 model/provider 或 publication；空 batch
  可直接完成，未配置 interpreter 时仍能恢复 Published state 并显式 discard。这个 proof 仍把
  state 移进 consuming future，future 取消会丢 state，因此不是最终 owner。
- public durable publication contract 已定义 `CommitStager<C>`、稳定 request/item/contract identity、
  host-owned `PublicationCandidateFingerprint`、`PreparedSessionMutation`、expected-revision
  `PublicationStore`、Rejected/Indeterminate 与 `resolve(request_id, fingerprint)`。store 对同 request
  id 的 fingerprint byte-for-byte 比较；receipt mismatch 也会被 attempt 拒绝。streaming 和 native-only
  attempt 使用同一 contract；durable success 不暴露 pending Commit。
- crate-private managed publication actor 在 mailbox 接受 command 后独占 store future。caller
  cancellation 不能把 Resolving 误判为可 abort；最后 handle 消失时，Published 会完成内存
  lifecycle，Indeterminate 会持续按相同 request id resolve；NotCommitted 会 abort，definite
  fingerprint collision 证明当前 candidate 未提交并以 `PublishFailure` abort，临时 resolve failure
  则使用 capped exponential backoff 保留 recovery obligation。attempt-to-publication handoff 的
  reply 被取消时，新 handle 的丢失会触发这个 owner 的 recovery；旧 actor 已是 `Transferred`，
  不会对同一个 finished attempt 二次 abort。

**仍未接入的 production owner 边界：**

- reducer、Commit staging policy、durable session/epoch identity、store/revision conflict policy 与
  fingerprint policy 现在已经由 crate-private binding 在 mount 时固定，Continue 不能替换它们。public
  配置层仍需把这些依赖组织成窄的 persistence/live/provider façade，不能要求使用者构造 internal
  attempt typestate。harness definition 持有 `TurnLoopPolicy`；public call builder 只接收 call id/label、
  owned inputs 与可选的 lower-only turn cap，不暴露
  pinned call、Finished state、publication actor、raw revision 或 staging plan。
- 外部 CAS conflict 表示 local session/revision 已过期，owner 必须进入 `ReloadRequired` 并拒绝新 call，
  直到 host 原子 reload session + revision。System snapshot 还必须持久化覆盖 POM、binding factories 与
  provider capabilities 的 stable epoch-contract identity；仅比较 rendered text 不能识别 runtime contract
  已变化。crate-private owner 已区分 conflict、阻止 call/reconfigure，并通过同一 binding reload session、
  revision 与 stable contract identity；reload 的 load/validate/install 也在同一 turn lane 内串行。private
  durable System reconfigure transaction 与 abandoned `RenderStarted` exact-fence recovery 已存在；后者
  返回 authoritative A snapshot 并在 owner configuration/turn lock 内安装。旧 process owner 仍不能在 B
  activation 后仅靠 A definition 执行 local reload。公开层必须选择 managed non-cancellable
  reconfigure actor，并为 post-activation failure 定义 B reconstruction 或明确的 close-and-reopen contract。
- typed Output/Diagnostic 已由 per-attempt `TurnRecord<C>` 随 completion 线性交给 pure reducer，raw output
  与 provider results 也属于同一 record。公开 `CallReport` 仍需提供 consuming access；若应用要求跨 lane
  的全局 wire order，还需增加带 sequence 的 observation journal。
- caller 可能在 receipt 已提交 `TurnFlow::Continue` 后停止等待。当前 crate-private path 会先完成
  session/revision writeback 再释放 lane，但不会自动执行下一轮。private checkpoint 已把 call/input id、
  next turn、last request 与 AwaitingContinuation/Settled status 随 session mutation 持久化；新 owner 能从
  turn 1 resume，wrong input、different pending call 和 settled retry 都在 User render/provider 前拒绝。
  private `MountedSessionStore` 已在每轮原子 claim `Reserved`，并只在 provider 启动前提升为 `Running`；
  双 owner 只有一个能进入 provider，publish 也会 fence Running expiry 并写 RecoveryRequired。完整 ledger
  会跨后续 call 保留 settled entry；public
  runtime 仍需把 settled duplicate 提升为 idempotent completion success。进程内 loop owner 必须持有
  owned/`Arc` props/source，使 dropping wait future 只停止等待。
- provider-complete candidate 会在第一次 publish 前持久化
  `ClaimedCallPublication(request id, fingerprint, call/input/lease scope)`；process-local actor 负责当前
  publish/resolve，reopen/reload 则先通过 store 的原子 reconciliation 处理 Published、NotCommitted、
  InFlight 与 collision，不能直接 replay provider。当前没有持久化完整 mutation/outbox payload，
  所以 restart 后权威 NotCommitted 会进入 `RecoveryRequired`，而不是重新 publish 或重跑 model。
- logical System 仍只能有一个。mounted `MountedProviderRequest` 与 session reducer 的 request 都不再
  包含 System 或 tool schemas；唯一的 provider-facing System transport 是
  `attach_epoch(MountedProviderEpoch)`。在一个 in-process harness epoch 内，context replacement、retry
  和 `TurnFlow::Continue` 都复用已 attach 的 binding；reconfigure 只有在 history rebase 成功后才 attach
  新 epoch。System policy 变化仍通过显式新 epoch + history rebase，而非 Continue。
  **public local facade 已经有这条 reopen 保证：** private `BoundMountedAgent::open` 在 store 返回
  `Existing` artifact 时，只重建 POM-free runtime registry、验证持久化 manifest，然后以
  durable id + artifact fingerprint + provider receipt 调用 rehydrate。它不调用 `SystemView`，也不传递
  System/tool payload；owner 测试验证 create/drop/reopen 后 System render 仍为 1，第二个 provider
  只发生 receipt-only rehydration。mount-owned store 还会在第一次 open 时持久化 session seed，并在 owner
  构造前以同一个 store snapshot 严格校验 active artifact；不同 reopen seed 与 missing/changed artifact 都有
  fault-injection coverage。external consumer 现已通过 `InMemoryMountedAgentFactory` 证明相同路径，
  包括两个 call、replay、reload、drop/reopen、provider cursor、awaited Live 与 joined cancel。这仍只是
  local proof；production store/recovery worker、durable reconfigure 和 Forgotten City adapter 完成后，
  才可以将这个行为视为 production freeze contract。
- pre-Ready capture 与 `prepare_context` 没有 cooperative cancellation token。当前 timeout 通过 drop
  future 实现，因此 trait 已要求它们 pure、read-only、drop-safe；若真实 provider preparation 需要
  外部 side effect，contract 必须先升级为 cancellable/joinable operation，不能依赖当前 deadline。
- AgentLoop 必须采用现有 isolated Live/compensation/publication contract，并让 fallible executor
  sink 能中止 active provider turn；当前 compatibility adapter 尚未这样做。
- AgentLoop 必须把当前 side-effecting `commit_turn` 拆成完整 pure session mutation，并把它与
  staged outbox 交给 managed publication owner。只有 staging failure、known rejection 或
  authoritative NotCommitted 可以对 retained Finished state 调用 abort。
- concrete store 必须实现 `(request id, fingerprint)` 幂等、same-id/different-fingerprint collision、
  revision CAS 和 atomic session + outbox；crate-private in-memory proof 已覆盖 pending identity 的
  reopen reconciliation，production recovery supervisor、完整 candidate payload policy 与
  recovery-scanned worker 仍没有实现。
  external delivery 是按 `OutboxItemId` 去重的 at-least-once，不是 publication request future 的职责。
- production AgentLoop 仍未使用 `MountedAgent + MountedProviderExecutor`；legacy
  `LLMExecutor + TurnSink` 仍在 compatibility path。不能从 internal owner、fork/reconfigure 或
  native-tool round-trip tests 推断迁移已经完成。当前 private actors 已覆盖 Active/Finished、
  actor-to-actor durable handoff、process-local publish/resolve cancellation 与 reopen reconciliation；
  production recovery supervisor、concrete session store 和 outbox delivery 仍不能由 raw task abort 或
  Drop 代替。
- 用户 call label 只用于观测；当前 attempt identity 已使用 opaque epoch/turn/provider-attempt/
  live-scope token，`ProviderInvocationKey` 还包含 capability/invocation id。当前 exact replay 只在一个
  attempt 的内存中有效；外部 resource scope、durable tool idempotency 和 retry policy 仍待 host
  定义。

### 7.2 API freeze 前仍需回答

1. **View 与 channel 的最终类型形状。** 第一阶段已公开 `PomView` 和兼容型
   `ProvidedView<B>`，并证明 tuple、`Option`、`Vec`、nested component 与 local
   `.map_binding(...)`；streaming spike 已证明 parent expected type 下的单 lane inference，
   以及一个 tag 经 nominal `StreamingChannelsView<C>` 和完整 `ChannelMap` 同时产生多 lane。
   `StreamingValueView<E,D>` 与 `StreamingChannelsView<C>` 分别封住 single/multi-lane
   authoring capability，multi-lane、lane preservation 与 text-event 边界已有 compile-fail
   证明；独立表达式仍可能需要 root type annotation。
   下一步仍需冻结
   `ProvidedView<LocalChannels>`、`SystemView<RootChannels>` 和无 binding 的 `UserView`；
   compiler 内部使用 normalized erased factories，并把相同 channel contract 扩展到
   provider-native capabilities。
2. **root channel 的 AgentLoop driver。** local channel contract 已先映射到 typed root，随后
   在 mounted epoch storage erase 一次；`ChannelTypeInfo`、mismatch diagnostic、typed public
   handle、crate-private object-safe attempt proof 和单次 mounted preparation/provider owner 已实现。
   private call ledger 已持久化 `DurableCallId + DurableCallInputId + next_turn_index + pending Continue`，
   能在新 owner open 后 resume，并用 combined store contract 完成 atomic claim、lease-fenced publish、
   expiry recovery 与 terminal retention。private owner 已能返回 idempotent terminal receipt 而不重跑
   provider；`MountedCallOutcome::Executed | Replayed` 已由 public mounted owner 返回，external
   black-box test 覆盖 owned input、两个 call、replay、reload、drop/reopen 与 cancel。仍需冻结的是
   builder/capture ergonomics、stable reconfiguration vocabulary，以及如何让 fork/reconfigure 成为
   production persistence transaction。完整 session mutation、typed staging、in-process
   indeterminate resolve、reopen reconciliation 和 cancellation retention 已接通；production concrete
   store、recovery supervisor 与真实 AgentLoop adapter 尚未实现。
3. **`TurnBindingCx` 的长期最小字段。** 当前 API 已给 epoch、turn instance、provider attempt、
   live scope、binding id、route、call label 和 typed props。需确认其中哪些应成为稳定 public
   contract；current view/catalog 若 factory 需要，由应用放入 immutable call props，context、
   service、channel 和 mutable session 不直接暴露。
4. **stream 开始后的 retry。** 当前 native runtime 会在同一 attempt 对 exact invocation/correlation/payload
   replay 内存 result，并在 collision 后终止；未收到任何 runtime event 的 transport retry 可以复用
   request bytes。一旦产生 live event，旧 attempt 必须 acknowledged abort，并用新的 provider
   attempt id 和 fresh binding/dispatcher state 重试。是否默认禁止这类自动 retry、如何持久化
   replay，仍需冻结。
5. **Provider capability 的 executor integration。** isolated runtime 已有 mounted plan、bound per-attempt
   dispatcher、typed result，以及独立的 `ProviderToolCatalog + MountedProviderExecutor +
   FallibleProviderWirePort` contract。owner cancellation source/token 与 joined terminal exit 也已
   定义；不能扩展 legacy `TurnSink` 来伪装 native tool round-trip。剩余工作是实现真实 provider
   adapter 与 AgentLoop owner，并覆盖 hung transport、timeout 和 wire-fault cleanup。
6. **outbox delivery failure。** session + outbox 已发布后不能假装 rollback。durable enqueue 已选定；
   仍需定义 worker lease/backoff/dead-letter、receiver dedupe、delivery status 与 observer event。
7. **reconfigure 与 fork/resume。** 需要确定 fork 固定继承创建时 epoch，还是跟随 parent
   后续切换；恢复时 epoch digest 不匹配是拒绝、显式迁移，还是 compact-and-rebase。
8. **live compensation failure。** cancel timeout、host unavailable、partial compensation
   应如何进入 `AbortReport`、diagnostic 和 application recovery queue，不能只记录 log。
9. **publication store 的恢复与冲突 policy。** contract 已区分 preallocated request id、host-owned
   candidate fingerprint、store-issued publication id、session revision、stable outbox item id 和结果未知
   状态；same-id/different-fingerprint 已是 definite error。默认 durable mounted adapter 已递归排序
   private mutation/provider JSON object key，使用自描述 `sha256:v2:*` fingerprint，并把 durable state
   envelope 升到 schema 2；v1 在 candidate recomputation 前被明确拒绝，必须离线迁移。adapter 也拒绝
   backend 返回未推进 generation 的伪成功 CAS；新 publication 与 replay 已显式区分，只有前者写 outbox。仍需冻结
   typed host outbox payload 与 concrete store 的 canonicalization
   version migration、revision conflict error、resolve retention/backoff 与 restart scan；不能把可能已写入
   的事务当普通失败后直接补偿。当前 stage API 已在 typed Commit staging 后调用 pure host fingerprint
   factory；factory 看到最终 mutation/output/results/outbox，失败时 retained Finished state 与原 staging
   plan 可用于重试或 acknowledged abort；通用 factory 的 payload encoding 仍由 host 定义。
   managed owner 已把 known collision 与 transient resolve failure 分开：前者把当前 candidate 恢复到
   Ready 并 terminal abort，后者保留 Resolving obligation 并退避重试。runtime shutdown/restart 时如何
   把这份 obligation 移交给 durable recovery supervisor 仍待定义。

### 7.3 实现前的 executable spikes

在改 AgentLoop 前，先把以下六个 target example 做成隔离的可编译 spike：

1. `PomView + two ProvidedView` composition、`StreamingXml` multi-lane mapping，以及通用
   factory/provider capability normalization 已在三个 runnable example 中完成。当前
   `MountPlan<C>` 仍是并行 spike，不是 AgentLoop 的 mounted lifecycle。
2. deferred component 只接受 owned/`Arc` props，borrowed prop compile-fail 与延迟执行测试
   已完成。
3. nominal `SystemView<C, TurnProps>`/`UserView`、不可捕获的 System function 和一次性
   `MountedEpoch<C, TurnProps>` 已由 `pom_mounted_lifecycle` 证明；User preparation 不遍历 System，
   User root 不能注册 runtime declaration。更精确地说，API 保证每个 mount 调用执行一次；
   crate-private `MountedAgent` 已强制 in-memory logical epoch、pin、fork 与 reconfigure。它尚未进入
   production AgentLoop。
4. mounted `StreamingXml::state_with` / `.try_state_with`、contract-derived route、typed props、
   fresh state、shared parser、cross-route wire order、closing chunk、terminal `BindingFault`、
   awaited `LiveEffectRuntime`、terminal `LiveEffectFault`、compensation、private pending Commit、
   `finish_stream -> publish_with/abort` 和 framework-issued receipt 已由 `pom_mounted_streaming`
   与 component tests 证明。`pom_provider_tools` 与 provider-dispatch tests 还证明了 grouped
   dispatcher、shared XML/native identity、expected-result/terminal-error policy、replay/collision、
   awaited Live、publication-recorded tool results 与 publication-gated Commit。crate-private
   object-safe driver 进一步证明 non-generic owner 可串行驱动同一个 combined attempt，并在
   typed update delivery 后才放行 tool result。crate-private managed actor 还证明 caller 在
   `drive`/`finish` 中取消时不会直接 Drop pre-publication state；crate-private legacy Published shim
   已证明 process-local Commit retry。durable staging/store state machine 与 managed publication actor
   进一步证明 caller cancellation 后继续 publish/resolve，且 durable success 不泄漏 Commit。
   crate-private `MountedAgent` tests 还证明 borrowed call props、per-candidate async capture、
   replacement 只重渲染 User、System/epoch 不变、final Ready 才初始化 state，prepare error/limit
   不创建 attempt，native tool 完整 round trip，以及 complete/cancel/Finished-abort future drop 都由
   detached owner 执行 acknowledged cleanup。current-thread runtime、hung preparation deadline、
   hung provider grace timeout/poison、generation exhaustion、fork/reconfigure 也有覆盖。complete pure
   session mutation、owned plan handoff、receipt-gated session/revision writeback、非连续 CAS revision、
   cancellation-independent publication/resolve 和同 epoch `TurnFlow::Continue` 也已覆盖。private
   mount-owned reducer/persistence binding、typed Output/Diagnostic/tool-result `TurnRecord`、identity
   mismatch、reload serialization、turn-limit pre-publication abort、stable-boundary turn-index resume、
   two-owner admission exclusion、publish-time expiry fencing、RecoveryRequired 与 ledger retention 也已覆盖。
   terminal completion replay 与 pending identity reconciliation 已覆盖，并通过 public local facade
   执行；production adapter、concrete store、完整 candidate restart policy 与 outbox worker 尚未实现。
   private owned-call actor 另已覆盖 admission-before-return、wait observer drop、reserved preparation cancel
   和 call-scoped joined cleanup。durable reconfigure 已覆盖 immutable owner policy/replaceable epoch split、same-owner pinning、
   cross-owner fencing、atomic session/System activation、attachment resume、Activated POM-free retry、
   `RenderStarted` no-rerender、exact-fence abort/tombstone、owner-A snapshot reload，以及 activation 后 waiter
	 cancellation 的 poison-and-reopen。crate-private managed reconfiguration actor 现在会在返回 handle
	 前接管 definition/rebase/runtime lease，但 start 只有在 store 对同一 id 返回 `Create` 或
	 `ResumeAttachment` durable admission 后才返回 handle。projection/manifest preflight failure 在
	 rebase/store/System render 前作为 start error 返回；history rebase failure 先持久化 `Rejected`
	 再返回 start error；already-activated retry 返回 typed terminal start disposition，不伪装成新的
	 `ActorAccepted`。丢弃尚未完成的 start observer 不会取消 detached actor。测试在 provider attachment
   阶段丢弃正在执行的 `wait()` 以及最后一个 handle，仍证明 B 在本地与 store 中完成 activation，且
   System B 只 render/install 一次、不需要 retry。public start/error/result vocabulary 仍待冻结，
   post-activation B reconstruction 或明确 close-and-reopen policy 也仍是 freeze blocker。
   Independent public-freeze review 明确拒绝直接公开这个 one-shot handle：公开层应返回
   `ActorAccepted { id }`，再按 reconfiguration id query/watch 可重放的 durable terminal state；
   terminal vocabulary 至少区分 `Activated`、`Rejected`、`RecoveryRequired` 与
   `ReopenRequired`。公开层不应暴露一个实际无权停止 durable transition 的 `cancel()`。
	当前 crate-private query proof 已按 id 原子读取 operation record 与 current active epoch，
	并投影 `NotDurablyAdmitted`、`InFlight`、`RecoveryRequired`、`Activated`、
	`Superseded`、`Rejected`、`Aborted`，同时报告本地 owner 是 `Current`、
	`TransitionInProgress` 还是 `ReopenRequired`。A -> B -> C 后查询 B 仍得到
	`Superseded`，重复使用 B 的 id 会在任何新 System render 前被拒绝。history rebase
	失败现在会在 candidate System render 前写入 session-scoped `Rejected` tombstone；
	相同 rejection 写入幂等，改变 base/manifest 或碰撞 in-flight/activated/aborted id
	会冲突。新 tombstone 还要求 expected base 仍是 active epoch，且 session 的
	reconfiguration lane 没有被另一个操作占用，因此 stale owner 不能穿过并发 replacement
	写入终态。丢弃 observer/final handle 以后 actor 仍会写入，reopen owner 仍能查询同一
	terminal record，并且不会重渲染 System。它还不能公开：`NotDurablyAdmitted` 仍只说明
	store 中尚无记录，可能是 actor 正在 preflight，也可能是第一次 store transaction 前
	发生进程丢失；future facade 仍需定义 query/watch 和这个窗口的 stable result。
	production adapter 可以在 retention 到期后压缩 superseded/aborted/rejected 的完整
	artifact payload，但必须保留 session-scoped `(id, terminal status, target identity)`
   tombstone 直到 session 被原子删除；GC 不能重新授权旧 id，也不能导致第二次 System render。
5. root-channel erased adapter：两个不同 `MountPlan<Root>` 可进入同一种内部 epoch storage，
   两个不同 root 的 active/finished/published attempt 也可进入同一种 driver storage；typed
   Output/Diagnostic、awaited Live、retained Commit 保持 phase，mismatch 在 reducer/dispatcher
   initialization 前失败，public API 和 attempt wire path 都不出现 `Any`。
6. [待完成] real AgentLoop owner 的 `open -> append -> parsed close -> publish/complete` 与
   `open -> abort/cancel` 两条 deterministic trace。

这些 spike 通过后再改 mounted AgentLoop；否则 authoring、type erasure 和 cancellation
会同时压到 transaction code 上，难以判断失败来自哪一层。
