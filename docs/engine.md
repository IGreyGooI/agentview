# AgentView Engine 设计

本文是 AgentView Engine 的权威设计文档。它规定稳定边界、状态所有权、调度语义和必须保持的
不变量，不记录实现进度、测试数量、提交版本或临时迁移状态。

若其他设计说明与本文冲突，以本文为准。实施顺序和迁移锚点见
[`frame-driven-runtime-plan.md`](frame-driven-runtime-plan.md)。

## 1. Engine 的目标

AgentView 把 LLM 应用组织成 retained Component tree：

- Component 持有业务状态，并声明外部 target 当前需要看到的完整界面；
- Component Runtime reconcile 出完整 `RenderedProjection`；
- private `FrameSession` 将 canonical history、完整 projection 和 staged ToolOutput 编译为一个 target-ready
  `Frame`；
- `ReactionPort` 把 Frame handoff 给 Provider、Skill 或 Plugin，并返回有序 `ProviderFactStream`；
- Runtime 先把 fact 接纳进 canonical history，再向 Component dispatch provider-neutral Event；
- `Application::run()` 是默认 fixed driver；需要手动调度的 external owner 可以显式调用
  `Application::react()` 开始一次 reaction。

### Component 输出契约

Component 输出的是当前组件必须告诉 LLM 的完整状态声明：规则、环境、业务状态、记录或应用显式
提供的消息。这里“完整”只约束当前组件的交付要求；它不是 LLM 已知内容、canonical history
或最终 provider 请求上下文的全集。Component 不需要判断模型历史里已经有什么，也不需要自行输出
“本轮新增的消息”或 patch。`RenderedProjection` 中的完整 items 表达这份声明；成功提交时，Runtime
和交付层必须通过有效历史与本轮输入共同满足它，不要求每次请求都重复序列化全部内容。

有效历史可能含有旧状态、较早对话、provider assistant output 及已接纳的 ToolCall/ToolOutput
等事实。projection 中的内容消失不自动让 LLM 忘记这些历史；普通 user/developer XML 的
`<remove>` 只声明对应状态已经语义失效，不物理删除既有 history。历史保留与实际 wire context
仍由 FrameSession、port 以及既有 System snapshot、compaction 和 context reset 规则决定，
Component 不把它当作永久保留或可精确观测的集合。

从作者视角，Runtime 与 provider port 根据各自可用的有效历史 best effort 地生成 diff 或补齐缺失内容。
best effort 指尽量复用历史、减少重复的交付优化；无法可靠复用时使用完整内容或明确报告无法提交。
普通 POM 的状态变化仍只与上次成功提交的完整快照比较；更早的同值历史不能吞掉 `A -> B -> A`。

当前 Frame 架构将这项职责分成两部分：private FrameSession 编译 canonical replay、projection diff
及缺失输入；`ReactionPort` 根据自己的 wire history、continuation 和 provider 能力编码最终请求。
因此 port 不另行推断组件语义 diff，也不持有第二份组件 baseline。旧 `ProviderPort` 是兼容接口名称。

通常的 assistant output 由 provider/runtime 的历史路径保留和管理，Component 无需重新声明模型
之前说过的话。FrameSession 保留已接纳的 canonical output，port 保留 provider-specific history
和私有协议状态。若 Component 自己显式提供 assistant message，它属于组件 authored projection，
按当前完整声明处理；assistant role 本身不意味着它是 provider 产生的 output。
这类 authored assistant message 的新增和变化仍参与普通快照比较，其消失按既定规则不生成删除 patch。
组件通过 `view! { #[assistant] ... }` 声明它，沿用 `#[user]` / `#[developer]` 的角色继承和连续合并规则。
捕获结果为 `Message(Assistant, POM)`，provider 自动输出仍为独立的 `AssistantText`；即使最终文字相同，
也不把这两类 canonical 内容按文本合并。普通 authored assistant 变化时发送完整当前 POM，
显式组合 `#[diff]` 时仍使用已有字段策略。

需要每轮重发的 policy 或上下文使用 `#[developer(repeat)]` / `#[user(repeat)]`。这类声明在每次
提交的 Frame 中发送完整当前 POM，即使内容没有变化。`repeat` 优先于 `#[diff]` 的输出选择，
但 diff baseline 仍随成功 handoff 正常维护；仅 render 或 prepare 不算提交。

placement 作用于 `view!` 中紧接着的声明节点及其子树。标在组件调用前时，覆盖该组件及其子组件
输出的普通 POM；不影响后续兄弟声明，也不表示作用于整个 `RenderedProjectionNode` 归属分组。
外层 placement 优先于内层 placement，role 和 repeat 设置一起继承。捕获时只有同组件、同 role、
同 repeat 设置的连续内容才可合并；普通内容与 repeat 内容即使同 role 也生成不同 item。
repeat 是内部交付元数据，不渲染为 POM 标签，不要求声明 item 或 slot。repeat 内容消失仍按普通
user/developer XML 删除规则处理；assistant 和 System placement 不支持 repeat 参数。

### 完整提示词字符串

`view!` 根级动态表达式中的 `String`、`&String`、`&str` 转成一个 `RawTextNode`，承载已有的完整
提示词。它是 POM document 内的原文 block，不是 paragraph 或 inline text。document 级渲染保留
其中的换行、缩进、Markdown 和 XML 样例，不解析内部格式、不添加 XML 包装、不做 Markdown/XML
转义。多个 block 之间仍使用既有 document 分隔符；不承诺跨 block 拼接后与字符串直接连接相同。

`#[system_once] { system }` 将这个节点放入 System snapshot；角色属性不决定文本解析方式。
普通 user/developer 原文按完整值进行相邻比较，相同省略、变化完整输出，消失不产生删除 patch。
原文中的 `<tag>` 不构成 POM XML，不能被 XML diff 或 slot 扫描解释。显式 `#[diff]` 不会将原文
拆成字符或行级 patch；`repeat` 仍完整发送当前值。System 保持独立的 replace/clear 路径。

quoted 文本模板和显式 Markdown/XML 内的文本仍按既有 inline / XML 上下文校验与转义。
根级字符串适配不改变 `String` 的字段编码，也不放宽 `TextNode` 的 block 转换约束。底层 API
通过 `Document::from_raw_text(...)` 构造同样的原文内容。

```text
application owner
      |
      | Application<P>::run() (default)
      | or one explicit react()
      v
Component Runtime -> complete RenderedProjection
      |                         |
      | private bindings        | private FrameSession
      |                         v
      |                  Full | DeltaFrom Frame
      |                         |
      |                         v
      +------------------ ReactionPort
                                |
                         ProviderFactStream
                                |
                  canonical commit -> Component dispatch
```

Engine 保证 structured handoff、因果顺序、history/diff 一致性、取消和资源回收。它不替业务决定棋步
是否合法，也不把 Provider continuation 或 Component projection 当成业务数据库。

## 2. 所有权边界

### Component Runtime

Component Runtime 是业务权威，拥有：

- mounted Component identity 和 mount generation；
- `use_signal` state；
- 每个lexical `use_preparation` hook的identity与本次render的声明；
- Component tree 表达的完整业务 POM；
- latest committed complete `RenderedProjection`；
- retained provider-event handler slots；
- 每次 render/reaction generation 的 private dispatch bindings。

Signal 写入立即成为业务事实。Engine 不 fork 或回滚 Component state。业务需要事务性时，应在业务
Component 内先校验，再一次性写入自洽的新状态。

`RenderedProjection` 始终是完整值，不是 patch。Runtime 可以跳过 clean subtree 的重复执行并复用
retained fragment，但这只能是内部优化，不能让 projection 变成 partial。
preparation completion仅属于当前显式operation；Runtime不保留key、readiness或其他跨operation cache。

### Application<P>

`Application<P>` 是公开的应用编排 owner，长期固定持有：

- 一个 Component Runtime；
- 一个 crate-private `FrameSession`；
- 一个不可替换的 `P: ReactionPort`；
- single-flight reaction gate。

一个 Application 只代表一个 logical target session。它不公开 `port_mut()`、FrameSession mutation、
history commit 或替换 port 的 API。`&mut Application` 使第一版同一 Application 最多运行一个 reaction。
每次active `prepare()`或`react()` operation还临时拥有本轮各`(mount identity, lexical slot)`的完成记录；
该记录随operation结束而丢弃，不是Application或Component上的跨operation缓存。

公共编排入口保持很小；root没有运行时props，但可以由mount closure捕获只读启动配置：

```rust
impl<P: ReactionPort> Application<P> {
    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
        port: P,
    ) -> Result<Self, ApplicationFault>;

    pub fn current_projection(&self) -> ProjectionSnapshot<'_>;
    pub fn exit_handle(&self) -> ApplicationExitHandle;
    pub async fn prepare(&mut self) -> Result<ControlFlow<ExitReason>, ApplicationFault>;
    pub async fn wait_for_reaction_request(&mut self) -> Result<(), ApplicationFault>;
    pub fn take_reaction_request(&self) -> Result<bool, ApplicationFault>;
    pub async fn run(&mut self) -> Result<ExitReason, ApplicationFault>;
    pub async fn react(&mut self) -> Result<ControlFlow<ExitReason>, ApplicationFault>;
    pub async fn shutdown(self) -> Result<(), ApplicationFault>;
}
```

`mount()`保持同步。它发布一版完整bootstrap projection；若render含有preparation declaration，该projection
可以是provisional，`ProjectionSnapshot::is_prepared()`明确区分它。`prepare()`只驱动Component preparation
和reconcile，不读取`declare()`、不构造或修改`FrameSession` candidate，也不调用`submit()`。
每次显式`prepare()`和`react()`各自创建一个新的preparation operation，因此成功的`prepare()`之后的
`react()`仍须再次运行全部active preparation。普通preparation失败或调用方drop pending operation后，
同一个Application仍可由下一次显式`prepare()`或`react()`重试；该新operation也从全部active hooks重新开始。

`run()`顺序循环调用`react()`，在第一个`Break(reason)`返回该`ExitReason`，或直接返回第一个
`ApplicationFault`。它不spawn background driver、不替owner停止业务资源，也不调用`shutdown()`。
`react()`是手动单步入口，返回`Continue(())`表示本轮已结算；`prepare()`的`Continue(())`只表示准备完成。
`use_application_exit()`提供mount-fenced退出句柄，宿主可以通过`exit_handle()`取得独立句柄。
`request(ExitReason::Completed | ExitReason::Requested)`记录第一个退出原因并唤醒pending preparation；
退出后两个operation返回相同的`Break(reason)`，不再运行新准备或提交Frame。

退出与提交许可经过同一个同步状态检查：退出先记录则不调用`port.submit`；当前提交先取得许可时，
允许当前reaction完成，再返回Break。该许可不改变Provider handoff或history commit边界；continuity retry
也必须再次检查许可。Exit不越过streaming recovery fence，不吞掉fault或panic。Break不证明业务资源已清理；
owner仍需完成业务stop/ack并调用consuming shutdown，shutdown及Application drop会使退出句柄失效。

`ReactionRequest::request()`只记录至少一次后续reaction的sticky request；重复request在driver消费前合并。
`wait_for_reaction_request()`阻塞直到消费一个请求，`take_reaction_request()`不阻塞地消费当前请求并在没有请求时
返回`Ok(false)`。二者都不render Component、不调用`declare()`或`submit()`，并在消费前后复用与`react()`相同的
outer-boundary仲裁：mount fence、supervisor `Closed`和已经终结的Application均fail closed，fresh Component task
panic保留原payload并优先unwind。

### private FrameSession

FrameSession 原子持有两类不同状态：

```text
CanonicalHistoryState
  handed-off canonical input
  admitted public output partial / seal / interruption
  ToolCall and ToolOutput causal facts

TargetDeliveryState
  FrameSession namespace and current FrameRevision
  retained complete projection checkpoint
  selected replay-view identity
  #[diff] complete baseline
  staged ToolOutput receipt
  disposable PreparedFrame candidate
```

Canonical history 是 shared causal truth；target delivery state 是同一 history 对一个 fixed target 的
delivery cursor。第一版二者由一个 FrameSession 持有，但不能把 revision、projection baseline 或 staged
receipt误称为 canonical history。未来多 target 可以拆开这两类 owner，不改变 Component API。

FrameSession 负责 `HistoryPolicy`、canonical replay、`#[diff]` lowering、Full/Delta 选择、hard budget、
prepare/commit transaction 和 ToolOutput staging。它不编码 Provider HTTP body，不持有 response id，
也不解析 SSE。

### ReactionPort

`ReactionPort` 是 Agent、Skill 和 Plugin 共用的 integration boundary：

```rust
#[async_trait]
pub trait ReactionPort: Send {
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault>;

    async fn submit<'a>(
        &'a mut self,
        frame: Frame,
    ) -> Result<ProviderFactStream<'a>, SubmitFault>;
}

#[non_exhaustive]
pub enum ReactionPortFaultKind { Retryable, Terminal }

#[non_exhaustive]
pub enum ReactionPortFaultCode { Unavailable, Rejected, Protocol, Limit, Internal }

#[non_exhaustive]
pub enum ReactionPortFaultReason {
    Declaration,
    RequestPreparation,
    Transport,
    Authentication,
    Authorization,
    RateLimited,
    UpstreamRejected,
    ResponseProtocol,
    StreamTransport,
    StreamTimeout,
    OutputLimit,
    Other,
}

pub struct ReactionPortFault {
    kind: ReactionPortFaultKind,
    code: ReactionPortFaultCode,
    reason: ReactionPortFaultReason,
}

#[non_exhaustive]
pub enum ApplicationFaultKind { Retryable, Terminal }

#[non_exhaustive]
pub enum ApplicationFaultCode {
    Unavailable,
    Rejected,
    Protocol,
    InvalidConfiguration,
    Limit,
    Exhausted,
    Component,
    Internal,
}

#[non_exhaustive]
pub enum ApplicationFaultStage {
    Reaction,
    Declaration,
    Bootstrap,
    Reconcile,
    Preparation,
    FramePrepare,
    Submit,
    FactStream,
    Admission,
    Binding,
    ToolOutput,
    PostReconcile,
}

#[non_exhaustive]
pub enum ApplicationFaultReason {
    Port(ReactionPortFaultReason),
    InvalidDeclaration,
    InvalidFrameProfile,
    NamespaceExhausted,
    TargetIdentityChanged,
    FrameProfileChanged,
    EpochRegressed,
    AcceptedRevisionInNewEpoch,
    ContinuityResetWithoutEpochAdvance,
    ReplayReplacementUnsupported,
    AmbiguousProjectionProvenance,
    RevisionExhausted,
    PendingToolCall,
    CanonicalInvariant,
    FrameBudget,
    FrameInvariant,
    UnstableContinuity,
    ProfileChangedBeforeHandoff,
    Preparation,
    PreparationGraphUnstable,
    ComponentRuntime,
    ComponentContract,
    ComponentInvariant,
    BindingLifecycle,
    EventHandler,
    ToolBinding,
    ToolLane,
    Admission(ReactionAdmissionReason),
    ToolOutput(ToolOutputStagingReason),
    FactProjectionInvariant,
}

pub struct ApplicationFault {
    stage: ApplicationFaultStage,
    kind: ApplicationFaultKind,
    code: ApplicationFaultCode,
    reason: ApplicationFaultReason,
}

pub enum SubmitFault {
    ContinuityChanged,
    ProfileChanged,
    Rejected(ReactionPortFault),
}
```

`ReactionPortFault`只保留closed、payload-free classification，不持有provider text、wire body、credential、
arbitrary source或自由字符串。`ApplicationFault`在编排边界同样只暴露payload-free
`stage/kind/code/reason`；tool/call identity和authored/model text必须在分类后丢弃。Component、handler、tool、
parser或observer panic不进入`ApplicationFault`，而是保留原payload沿caller stack unwind。
`ReactionAdmissionReason`和`ToolOutputStagingReason`同样是closed、payload-free的子分类；其具体variant在
Phase 9 curated public export前保持crate-private，不允许携带任意字符串或底层source。
详细诊断属于integration-private observability。`ContinuityChanged`与`ProfileChanged`都保证pre-handoff；
前者允许Application重新declare/reprepare一次，后者在当前mount内terminal fail closed。
post-handoff `Retryable` fault或未完成stream的Drop可按port策略丢失continuity并推进epoch；后续
`declare()`只能如实给出仍可使用exact continuation的兼容`Accepted`、更高epoch的`FullRequired`或terminal
declaration fault，不能在required private state已经丢失后继续声明stale `Accepted`。post-handoff
`Terminal` fault必须成为logical target的sticky terminal state，不能降级成新epoch `FullRequired`。

port 只拥有 integration-specific protocol state：

- canonical item 到 wire item 的 deterministic lowering；
- request body、headers、transport 和 wire/token limit；
- Provider output ledger、SSE framing和remote acceptance state；
- response id、remote cursor和prompt-cache hint；
- encrypted reasoning、remote compaction和其他 protocol-required private causal artifact。

port 不拥有 shared canonical history、Component checkpoint、semantic `#[diff]` baseline或 ToolOutput
staging。它也不能在 `declare()` 中读取这些 private FrameSession 内容。

`declare()` 中的 `FullRequired` 只证明 port 具有不依赖具体 canonical payload 的 recovery strategy，且
当前仍保留该 strategy 所需的 required private causal state。它不声称已经验证某个 exact Full request；
因为 declaration 看不到 canonical history，这个证明只能在 `submit(frame)` 拿到 exact Frame 后完成。
port 必须在 crossing poll 前把 Full、required private artifacts、真实 wire bytes和token limit一起验证；
失败返回 structured、确定 pre-handoff 的 `SubmitFault::Rejected`。若 required state 已丢失且没有
provider-defined stateless rebuild，port 必须在 declaration 阶段 fail closed，不能声明 `FullRequired`。

旧`ProviderPort::execute(RenderedProjection)`不是这个contract的specialization；它只在新API发布后通过
default-enabled、deprecated `legacy-provider-port` feature保留一个minor release，随后删除。built-in
ports和新correctness tests不能通过legacy adapter实现。
同一个built-in provider实例不能交替驱动legacy与Frame-native state；mode只能在各自真实handoff crossing
poll claim，确定的pre-handoff local failure不claim，claim后另一入口typed fail closed。

### ToolCall Component

ToolCall Component 声明 target 可调用的能力。它接收完整 ToolCall，执行、拒绝或报告失败，然后产生
同一 `call_id` 的 typed ToolOutput。它不编码 Provider wire item，也不直接写 canonical history。

`#[tool]` 从同步或异步 Rust 函数生成工具定义值、参数结构和 JSON Schema；
工具名固定取 Rust 函数名，禁止 `name` 覆盖；`description` 仍可单独配置。
Unicode 函数名在调用记录和请求 JSON 中保留原值，不单独限制工具名的字节长度；Frame 和请求总大小预算仍然生效。
`NativeToolCall::new(add)` 将该值挂载为有状态 Component。每个工具实例保留最近两轮工具交互：
一轮指包含该工具调用的一次 provider response，同一响应中的多个 call/result 整组保留；尚未完成
的轮次不能淘汰。普通 render、prepare 和未调用该工具的响应不推进窗口。接纳调用时先追加
ToolCall，handler 完成后记录 ToolResult。render 输出保留窗口内的完整记录：每轮先输出所有调用，
再按调用顺序输出结果，不执行 handler，也不重复追加。reset 后重放同样保持该顺序。
工具定义和原生记录共同构成该 Component 的 projection。
Frame compiler 将这些记录与已接纳调用、staged results 对应起来，只保留一份 canonical fact；
reset 从完整 projection 恢复的工具记录也保留已提交归属，后续窗口滚动不会再次追加旧记录。
正常续接时，多个工具的线缆顺序由全局 admission 和 call ordinal 决定。
显式 reset 会从当前完整 projection 重建历史，跨工具记录按 Component tree 分组；
依赖原始跨工具时序的业务事实必须由业务 Component 明确投影。
旧轮次退出 projection 不生成删除 patch；provider 历史和 compact 由 port 管理。卸载后不再暴露
该工具，但不能删除已接纳的全局历史。取消产生的 unknown-outcome result 同样追加
到原工具实例的记录。普通业务 POM 不因此获得任意写入 native history 的能力。

`call_id` 只能标识这一次模型调用，不能保证业务重试仍使用相同 ID。有外部副作用的工具默认只能
承诺 at-least-once；需要 exactly-once 时，业务 Component 必须使用稳定业务幂等键或持久 effect
journal。Engine 的取消不能撤销已经发生的外部动作。

## 3. Rendering、Frame 与 #[diff]

### Complete projection

`Application::mount` 先读取一次 side-effect-free declaration，再执行 bootstrap reconcile。bootstrap
不创建 Frame、不调用 `submit()`，但保证 bare latest 已经有一版 committed complete projection，也让
Component mount lifecycle 可以建立 retained state。

Signal dirty 表示 Component state 比 latest committed projection 更新。dirty 不会自动 reconcile或
react；`current_projection()` 仍返回上一版完整 projection，并同时暴露 projection revision、dirty bit和
preparation checkpoint bit。一个已经prepared的snapshot发生后续Signal写入时仍保持prepared，同时标记dirty；
发布含preparation declaration的新projection在当前operation完成前为provisional。这个bit只描述已发布
projection的checkpoint，不是hook readiness，不能使未来显式`prepare()`或`react()`跳过preparation。
正常 reaction返回前执行一次 post-reconcile，使 handler和ToolCall lane产生的状态出现在下一 Frame。

完整projection中的全部`Instruction(System, pom)`不是普通append history。Frame compiler按node/item
render顺序拼接每个POM的top-level children，形成零或一个normalized System snapshot；结果没有child时
等价于`None`，表示clear。这里是“完整snapshot整体替换”，不是最后一个System item获胜，也不做value
dedup。System snapshot不进入canonical transcript、ordinary occurrence reconciliation或`#[diff]`。

private checkpoint保存最近一次成功handoff的snapshot。首次非空、内容变化、`Some -> None` clear和
`None -> Some`都强制Full；snapshot相等才允许继续检查Delta eligibility。Full的Component section以零或
一个normalized System item开头，随后才是ordinary reconciled items；Delta永远不携带System，表示保留
accepted baseline。prepare、Pending、pre-handoff Err都不推进snapshot；successful handoff与Frame revision
及其他commit candidate同步原子提交，post-handoff stream fault不回滚。port只应用这个Full/Delta结果，
不得从private history推断System replacement。

### Component preparation

`use_preparation(factory)`是Component-owned的pre-handoff hook，不是root登记的coroutine，也不是
Component-scoped background task：

```rust
pub fn use_preparation<Factory, PreparationFuture, Error>(factory: Factory)
where
    Factory: FnOnce() -> PreparationFuture + Send + 'static,
    PreparationFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: Display + Send + 'static;
```

render只同步声明factory，绝不执行I/O。每个显式`prepare()`或`react()`创建独立的preparation operation：
每个active `(mount generation, lexical hook slot)`在其中运行一次；rerender跳过该operation内已经完成的slot，
新mount的slot则在当前operation运行。下一次显式operation无论前一次成功、失败还是取消，都重新运行全部
active slot；所以`prepare()`之后的`react()`也会运行它们。operation完成记录不会成为跨operation
readiness cache或key。

dispatch时使用该slot当前render declaration所捕获的factory snapshot。Runtime按Component结构和hook词法顺序
识别并同步调用本wave所有未完成且已授权的factory；factory必须短小，返回future才放入`FuturesUnordered`并发poll。
该顺序不构成future完成顺序或hook间依赖。每个`Ok(())`都须通过mount verification才记录完成；只有本wave全部成功后才reconcile
Signal写入或进入Frame prepare。第一个被观察到的`Err`会drop该wave其余future并返回普通retryable preparation
fault；exit或drop operation也以drop未完成future表示取消，不增加hook的第三种返回值。另一个hook随后写Signal并改变已经完成slot的inputs，不会使该slot在同一operation重跑；
需要顺序依赖的I/O必须放进同一个factory，或由其Signal写入挂载nested Component来表达。

pre-handoff阶段最多运行16个execution waves：每一wave先render/reconcile并发现slot，按声明顺序同步调用本轮未完成
slot的factory，再以`FuturesUnordered`并发poll其future；只有全部成功后才丢弃已完成slot的新declaration并检查projection。
若先观察到error则drop该wave其余future并返回；若projection clean则立即success，否则进入下一wave
reconcile其Signal写入。只有第16个execution wave留下dirty projection时，才进行一次不得开始第17个factory
或future的final reconcile。final reconcile后必须没有unfinished hook，且丢弃unused declarations后projection
仍然clean才能success；若仍有unfinished declaration或dirty状态，返回terminal
`ApplicationFault { stage: ApplicationFaultStage::Preparation, reason:
ApplicationFaultReason::PreparationGraphUnstable, .. }`；普通preparation error返回retryable同stage的
`ApplicationFaultReason::Preparation`。两者均发生在`FrameSession::prepare()`和provider handoff之前。

factory和future都在mount fence外执行。Runtime在dispatch前authorize当前mount generation，在future完成时
verify该generation仍mounted。retired generation不能record completion或publish late Signal write；
retirement前已publish的Signal写入始终是authoritative且nontransactional，失败、取消或graph fault均不rollback
它们或外部effects。
Runtime只等待返回的future，对factory自行detached的work不提供readiness、completion或cancellation guarantee。
preparation不提供exactly-once保证；factory及其返回future必须tolerate repeated/partial execution和cancellation，
外部effects需要业务方自己的idempotency和durable deduplication。

factory或future panic仍是当前`prepare()`/`react()`调用栈上的direct user-code panic，原样unwind且不转为
`ApplicationFault`；supervised mount-task panic保持既有sticky terminal仲裁。ordinary failure和
pre-handoff cancellation均不隐式开始reaction；下一次显式operation重新运行全部active hooks。一个
`ContinuityChanged`所要求的同一`react()`内retry复用当前operation的completed slots，不重跑preparation。

legacy `ApplicationHost` / `ProviderPort`不运行Component preparation；render声明preparation时，必须在
provider execution前fail closed为`ApplicationHostFault::ComponentPreparationsUnsupported`。

### Frame boundary

public `Frame` 是一次 exact target-visible submission：

```rust
pub struct Frame {
    revision: FrameRevision,
    target: TargetIdentity,
    epoch: TargetEpoch,
    prepared_against: TargetContinuity,
    prepared_profile: FrameProfile,
    basis: FrameBasis, // Full | DeltaFrom(FrameRevision)
    submission: FrameSubmission,
}
```

Frame只能由crate-private validated constructor创建，并同时满足以下一致性不变量：

- `revision`内嵌的TargetIdentity和TargetEpoch必须分别等于`target`和`epoch`；
- `prepared_against.epoch()`必须等于`epoch`；`Accepted`中的revision也必须属于同一target和epoch；
- `prepared_profile`是prepare时使用的完整mount-stable profile；
- `DeltaFrom(base)`必须精确对应`prepared_against == Accepted { epoch, revision: base }`；
- base与新revision必须属于同一FrameSession namespace，且新revision sequence严格递增；
- Full可以从`FullRequired`或`Accepted`prepare。Accepted revision仍须属于同一target/epoch并作为exact
  handoff precondition，但它不必与新revision属于同一FrameSession namespace，也不约束新revision
  sequence；successful Full handoff会让port rebase到新revision。

因此port接到的Frame不会在这些重复control字段之间自相矛盾；port只需把完整precondition同自己的当前
snapshot比较，而不需要猜测哪个字段优先。

`FrameSubmission` 包含本次 Full或Delta的 ordered canonical items和ToolCatalog。`replay`与
`staged_inputs`永不包含System；Full的`projection.items`至多有一个位于首位的normalized System snapshot，
缺失即clear，Delta的`projection.items`不包含System。它不包含：

- complete `RenderedProjection` 或 complete private checkpoint；
- Component Signal、closures或reaction bindings；
- diff candidate、commit receipt或mutable history capability；
- Provider response id、wire history或private compaction artifact。

private、non-cloneable `PreparedFrame` 同时保留 public Frame、generation-exact bindings、complete checkpoint
和commit candidate。prepare不推进任何 state；successful handoff后的同步 commit一次性推进 canonical
outbound segment、revision、complete diff baseline和exact staged ToolOutput receipt。

### #[diff]

完整 projection 中的 diff marker只是 `#[diff]` 生成的 address/template metadata，不是已经算好的 delta。
Frame compiler 使用以下稳定地址：

```text
ComponentId + mount/execution scope + node-local structural path + diff slot
```

compiler 对照 private complete checkpoint，决定一个 slot是 full、semantic delta还是omit。首次出现、
baseline缺失、mount改变或target要求Full时发送完整值；v1 non-append replay view按下一节fail closed，
不能绕过provenance要求直接生成Full。mount/execution scope改变时清空普通快照比较基线和当前scope的
provider claims，并重置diff baseline。消失的diff地址也随成功handoff离开baseline，再次出现时发送完整值。
`#[diff]` lowering产生的append-forced item始终直接提交，不参与provider occurrence claim。
这个过程不对最终渲染字符串做任意文本 diff，也不泄漏完整 checkpoint给Delta port。

普通projection item默认对比同scope内上一次成功handoff的完整`RenderedProjection`，不查询累计authored
历史。相同前缀省略；长度相同且只有一个同role item变化时比较其POM，其后的相等item继续保留；
其他变动后缀先撤下旧XML，再按当前顺序发送
完整item。重复值按出现次数保留。node序列变化同样保留相同前缀并刷新受影响的后缀；node不渲染成wrapper。
XML可以输出局部patch或完整当前root；旧root消失时以旧role输出`<remove>旧XML</remove>`。
删除仅处理普通user/developer POM；ToolCall、ToolResult、assistant、provider extension及System
item的消失不生成删除patch。System仍走独立snapshot流程。
混合document可比较，无法稳定对齐root时撤下旧XML roots并输出完整当前document。text/Markdown变化
发送完整值，消失不输出撤回。删除POM只进入本轮submission，新checkpoint仅保留完整当前投影。
同scope的transport Full仍使用本地完整快照比较普通内容，显式`#[diff]`的target patch基线则重置。

Provider/tool来源认领独立保留：先claim同node已经认领过的provider occurrence，再claim本scope尚未
认领的provider occurrence；与上次普通快照相同的authored item不消耗新的provider occurrence。
普通authored回归满足`A -> B -> A`每轮发送，已认领的provider fact省略后重现仍不重复提交。
v1 projection没有逐item origin，因此scope改变时不能把旧provider value继续当成provenance：旧scope
累计provider outputs、checkpoint之后尚未reconcile的replay tail和staged inputs全部转入持久的
ambiguous multiset。普通item若在完成本scopeclaim后仍匹配该multiset，Frame prepare返回typed terminal
`AmbiguousProjectionProvenance`；不能省略或重复提交它。ambiguous occurrence不会被失败或无关的成功Frame
消费，后续Frame仍受同一fence约束。未来显式projection provenance可以替换这个保守fault。

### HistoryPolicy 与 canonical replay

面向编写 Component 代码的 agent 的用法见 [HELP.md](../HELP.md#business-history)，
相邻投影比较的详细规则见 [设计说明](adjacent-projection-diff-design.md)。

Component每次render完整的业务POM状态；变化字段使用`#[view(diff)]`，增长的业务记录使用
`#[view(diff(append))]`，外层通过`#[diff(slot = "state")]`声明稳定边界。Frame compiler根据baseline
选择完整值、支持的delta或omit。当前structured delta要求root有多个children且含diff slot；只有一个
history字段时变化会full-fallback。业务层不重建旧assistant回复或ToolCall/ToolResult，也不通过新的
`view!`语法声明canonical message边界。

Native tool由Component声明并处理当前call，返回绑定该call的output。private FrameSession维护
canonical conversation/tool history与待提交结果，port负责provider编码和private session state。
下一次显式`react()`提交待处理结果；tool完成本身不会自动发起下一轮模型请求。

固定同一execution scope和Component identity时，上一轮node vector为`[[A], [O]]`，
当前为`[[A, B], [O, P]]`，本轮新增`[B, P]`。之后变成`[[A, B, C], [O]]`，本轮发送C，
并对P中的XML发送删除POM；若P为纯text则没有删除输出。P再次出现时发送完整P。
完整projection的node-order展开与跨reaction累计的canonical顺序是两个不同的值；历史只追加，
POM更新不物理撤回或重排旧消息，也不通过相同内容定位某条历史消息。需要精确业务身份和集合顺序时，
由XML结构及已有diff策略表达。`#[diff]`产物沿用上一节的独立提交规则。

`HistoryPolicy` 是 crate-private pure function：它从只读 `CanonicalTranscript` 和mount-stable
`FrameProfile` 选择 replay view。它不拥有history、不提交Event、不推进revision，也不参与ToolOutput
receipt。

v1唯一合法view是`CompleteTranscript`：按原顺序包含全部committed append-only canonical items，不删除
前缀、不生成summary、不替换closed turn，也不注入provider artifact。Component-owned System snapshot不在
transcript中，它按上一节的独立、可证明规则随Full原子replace/clear；这不放宽ordinary history replacement
或occurrence provenance。FrameSession验证pending ToolCall由本次staged ToolOutput闭合。若完整replay无法
满足Full reserve，session返回typed`CanonicalReplayTooLarge`并停止后续handoff；不能静默truncate或调用
未定义的semantic compaction。

未来canonical checkpoint/summary必须先定义显式canonical fact、覆盖区间、digest、ToolCall closure和
每个selected occurrence的origin/provenance grammar。该扩展存在后，view replacement强制下一Frame为
Full；在扩展落地前，任何non-append selected view都必须typed fail closed，不能仅凭item value猜来源后
生成Full。matching head始终只是Delta必要条件。

Provider-private remote compaction是另一类状态。它只有在对应private output完整验证并seal后才能安装，
不进入public Frame或canonical transcript，也不因后续stream fault回滚。只要port仍能合法承认head，
private compaction本身不强制Full。

### Budget closure

`FrameProfile` 在一个Application mount期间稳定：

```rust
pub struct FrameConstraints {
    pub max_frame_bytes: usize,
    pub max_component_bytes: usize,
    pub context_window_tokens: Option<u64>,
    pub reserved_output_tokens: Option<u64>,
}
```

`max_component_bytes` 是complete Component projection加Component-declared ToolCatalog的authoring
envelope；`max_frame_bytes` 是整个stable canonical `FrameSubmission` payload的hard limit。二者使用同一
versioned meter：RFC 8785 JSON Canonicalization Scheme（JCS）的UTF-8 byte length。

```text
ProjectionSubmissionV1 {
  items: [CanonicalInputItemV1],
}

ToolCatalogEntryV1 {
  name: non-empty UTF-8 string,
}

ComponentEnvelopeV1 {
  version: 1,
  projection: ProjectionSubmissionV1,
  tools: [ToolCatalogEntryV1],
}

FrameSubmissionV1 {
  version: 1,
  replay: ordered canonical replay items,
  staged_inputs: ordered mandatory inputs,
  component: ComponentEnvelopeV1,
}
```

`ProjectionSubmissionV1.items`按Component render顺序扁平化ordinary items；Full在它们之前放零或一个
normalized System snapshot。Full携带shared reconciliation后的self-contained Component segment：
`replay + staged_inputs + component`整体不依赖旧delivery checkpoint即可解释，但replay/staged已经表示的
provider-output occurrence不会在Component section重复。raw complete projection始终保存在private
checkpoint；`max_component_bytes`计量normalized System snapshot加完整ordinary projection和ToolCatalog。
Delta携带compiler已经lower完成的full/delta ordinary items且不携带System，omit项不出现；node identity和
diff address保持private。port不得再次对这些section做semantic dedup或System replacement inference。
`CanonicalInputItemV1`使用canonical transcript的versioned tagged encoding。sealed `AssistantText`沿用
既有编码并省略默认status；interrupted text显式编码`status: "interrupted"`，不能伪装成sealed output。
ToolCatalog按name的JCS string comparator升序排列，重复name非法；数组顺序是meter的一部分，port不得
重排后再解释`canonical_bytes()`。

Component meter包括version、projection/tool字段名、容器和ToolCatalog；Frame meter包括version、三个
section字段名和全部容器。identity、epoch、revision、prepared-against和basis是fixed-size structured
control metadata，不进入semantic payload meter。JCS object key排序，causal/tool arrays保持规定顺序。

Full reserve按exact公式验证。将使用actual CompleteTranscript replay和actual staged inputs的候选Full中
`component`替换为JSON `null`：

```text
full_non_component_bytes = len(JCS(candidate_with_component_null)) - 4
required_full_bytes = full_non_component_bytes + max_component_bytes
required_full_bytes <= max_frame_bytes
```

mount先对空transcript/staging验证；每次TextDelta、ToolCall和ToolOutput admission用候选完整state再次验证，
Delta也必须证明hypothetical next-Full成立。prepare最后验证actual JCS bytes。checked arithmetic失败、零值、
Component limit大于Frame limit或初始reserve失败都是typed invalid-profile fault。

port随后独立校验private artifact、真实wire bytes和tokenizer。任何一层超限都是typed pre-handoff fault，
任何一层都不能静默truncate。

## 4. Target declaration 与 handoff

`TargetDeclaration` 是幂等状态快照：

```rust
pub struct TargetIdentity(NonZeroU128);

impl TargetIdentity {
    pub const fn new(value: NonZeroU128) -> Self;
    pub const fn get(self) -> NonZeroU128;
}

pub struct TargetDeclaration {
    identity: TargetIdentity,
    continuity: TargetContinuity,
    profile: FrameProfile,
}

pub enum TargetContinuity {
    FullRequired { epoch: TargetEpoch },
    Accepted { epoch: TargetEpoch, revision: FrameRevision },
}
```

Application在mount时固定opaque `TargetIdentity`和`FrameProfile`。相同snapshot的重复`declare()`完全
幂等，不推进state，也不会仅因调用次数使PreparedFrame失效。identity变化或profile变化fail closed；
同一identity内epoch只能单调推进且不得复用。切换Provider account、Skill session或Plugin parent必须
创建新的Application，不能继承旧canonical history。

每次成功校验declaration都会同步推进private `highest_observed_epoch`，即使后续prepare或submit在handoff前
失败也不回滚。后续较低epoch必须在render和submit前fail closed；successful handoff另行推进committed
delivery epoch，这两个水位不能合并。

Frame携带target identity、epoch、exact `prepared_against` continuity、完整`prepared_profile`和basis。
port在real delivery前重新校验当前snapshot。Full也不能绕过precondition：从
`Accepted(epoch, R1)` prepare的canonical-rebase Full若遇到当前`Accepted(epoch, R2)`，必须确定未交付地
返回`SubmitFault::ContinuityChanged`；profile变化返回`SubmitFault::ProfileChanged`。
Runtime最多重新declare/reprepare一次；continuity再次变化则返回typed unstable-continuity fault。

### Poll-level cancellation contract

`Ok(stream)` 是唯一public handoff proof，不增加public commit、AcceptedFrame或declaration nonce：

```text
submit poll -> Pending          Frame确定尚未handoff
submit poll -> Ready(Err)       Frame确定尚未handoff
submit poll -> Ready(Ok(stream)) Frame已经handoff
```

第一个可能造成real或ambiguous delivery的poll必须在同一个poll返回`Ready(Ok(stream))`；跨过boundary后
不得再次返回Pending。Application在同一个outer poll紧接着执行同步、已预验证、不可失败的private commit，
两者之间没有`.await`。boundary后的HTTP status、断流、receiver断开或其他fault只能由stream yield。

drop一个返回Pending的submit future必须是零handoff、零FrameSession commit、零receipt consumption。
drop一个已经成功返回但尚未poll的stream属于post-handoff，不回滚outbound segment或diff baseline。
drop整个`Application::react()` future时，handoff前和handoff后取消都保持Application ready，但handoff仍决定
哪些state已经成为权威事实：handoff前candidate不commit；handoff后保留已commit Frame、已接纳facts、已完成
ToolOutputs、Component/Signal写入和外部副作用。当前callback、reaction-owned lanes和provider stream按该顺序
drop且永不resume或replay；open assistant text保留为`Interrupted`，每个尚未完成的已接纳ToolCall物化一个预留的
`Tool execution was cancelled; its outcome is unknown.` ToolOutput。取消不会自动发起下一reaction；只有下一次
显式`react()`才重新declare，并接受兼容`Accepted`、更高epoch `FullRequired`或terminal fault。direct panic和
supervised task panic都不属于cancellation且不得发布fallback recovery；caller catch user panic后仍必须丢弃
Application。

External/Skill adapter可以先异步reserve outbound queue capacity；取得permit后必须在crossing poll同步
send并返回Ready。boundary是owned transport/queue不可撤回地接受Frame，不是下游业务callback最终读取。

## 5. 一次 reaction

```text
1. &mut Application acquires single-flight ownership
2. port.declare() -> identity + continuity + stable profile
3. validate fixed target/profile and monotonic epoch
4. start an operation-scoped Component preparation
     -> synchronous render/discover active declarations in structural/hook order
     -> invoke each unfinished `(mount generation, lexical slot)` factory once
     -> concurrently poll the wave's returned futures; first observed error drops the rest
     -> after every slot succeeds, reconcile their Signal writes
     -> early success only when clean with no unfinished hooks
     -> after a dirty sixteenth execution wave, one final reconcile without a 17th dispatch
     -> complete projection + exact bindings
     -> create fresh independent streaming contract attempts
5. FrameSession.prepare(...)
     -> select and validate canonical replay view
     -> lower #[diff]
     -> close staged ToolOutputs
     -> validate admission closure and exact budgets
     -> private PreparedFrame
6. acquire submission permission against application exit; port.submit(public Frame)
7. Ready(Ok(stream)) -> synchronous infallible FrameSession commit
8. run one fact/lane pump
     -> validate fact
     -> precompute the optional admitted root ProviderEvent
     -> commit canonical fact
     -> dispatch event
     -> broadcast selected structured text to independent contract workers
     -> committed ToolCall starts its lane immediately
     -> poll active lanes while awaiting raw handlers or streaming work
9. valid terminal + normal EOF -> drain remaining lanes
10. finalize streaming parsers, decide each contract, and settle managed effects
    -> unresolved effects retain workers and return RecoveryRequired before step 11
11. post-reconcile Component state
12. release gate; return Continue, or Break if exit was requested
```

一次`react()`最多render/submit一个Frame，不自动开始下一次reaction。pre-handoff failure不推进任何
candidate state；post-handoff HTTP、stream、handler或lane failure不回滚已经handoff的input、已经接纳的
fact、Component写入或外部副作用。continuity retry只重复declaration、Frame prepare和submit，不重复第4步。

### Managed streaming contracts

`XmlStreamingToolCall::new::<Channels>(identity)`为每个contract在每次reaction中创建独立parser、State、
诊断和effect ledger。同一contract内的所有element共享这些状态和有序sequence；不同contract可以声明
相同标签。重复element只在同一contract内构成declaration fault。旧`StreamingXml`和
`.contract(...).empty_element(...)`继续使用原有shared-route parser。

canonical admission之后先运行既有raw handler，再把保留output key和phase的内部text sidecar广播给各
contract；Commentary不进入新parser。`TextSealed`只校验完整文本，真实provider EOF才运行各contract的
`finish`。各contract独立Accept/Reject，业务Reject不撤回另一个contract的发布。新managed Live通道是
上面普通副作用规则的显式补充：只有注册receipt的效果由框架执行confirm/rollback；它不回滚canonical
history、普通Signal写入或任意未登记的外部副作用。

worker持有进行中的apply、publish和settlement，取消`react()`等待不会丢弃这些操作。流在EOF前中断时
所有仍在解析的contract进入abort；EOF后的发布恢复保留各contract已确定的结果。所有worker清理完成后
Application执行post-reconcile，再释放至多一个合并的后继reaction请求。取消清理完成后可重新使用
Application；无法确定的效果返回`RecoveryRequired`并阻止新的prepare/react和reaction demand，调用
`recover_streaming_attempt()`继续处理。后置reconcile失败保留fence和fault；恢复不会重跑已完成的拒绝
回调。完整API和恢复报告见[streaming-tool-api-design.md](streaming-tool-api-design.md)。

## 6. ProviderFact 与 canonical admission

port返回的是history可以直接接纳的ordered fact，而不是只有UI含义的event：

```rust
pub enum ProviderFact {
    TextDelta {
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        delta: String,
    },
    TextSealed {
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        text: String,
    },
    ToolCall {
        output: ProviderOutputKey,
        ordinal: u64,
        call: ProviderToolCall,
    },
    ReactionCompleted {
        primary_text: Option<ProviderOutputKey>,
    },
}
```

`ProviderOutputKey` 是reaction-local、provider-neutral identity。`ProviderFactStream`的yield顺序也是
canonical publication顺序：一个key的first fact固定其output position；后续交错facts保持该position。
如果Provider wire以不同顺序完成output，adapter必须在yield前buffer/reorder，不能把private arrival order
泄漏成不确定的canonical history。一个fact最多投影出一个Component event：

| Fact | Canonical admission | Component event |
|---|---|---|
| `TextDelta` | append partial | `TextDelta` |
| `TextSealed` | validate accumulated text and seal | none |
| `ToolCall` | register completed call and ordinal | `ToolCall` |
| completed with `Some(key)` | validate sealed primary output | derive `TextComplete` from sealed text |
| completed with `None` | validate no-primary-text completion | none |

Runtime对每个fact执行`validate -> precompute event -> canonical commit -> dispatch`。Event不得先被
Observer或Component看见，之后才写history；handler fault不回滚已经commit的fact。

terminal/ordering grammar固定如下：

- 一个output key标识一个lifecycle；first fact固定kind和canonical position，同一key不能在text与
  ToolCall之间复用；
- text lifecycle可以包含多个delta和一个seal；所有fact的AssistantPhase必须exact相同；没有delta时
  `TextSealed`可以作为first fact并直接建立sealed text；
- seal恰好一次并校验accumulated text，seal后不能再delta；
- 每个ToolCall key只出现一次，ordinal在ToolCall facts中唯一且严格递增；private/non-tool output可以造成
  gap，ordinal不能直接当Vec index；
- v1最多一个non-commentary text lifecycle；存在时primary key必须引用它，不存在时必须是None；None允许
  tool-only或empty completion，不伪造空文本；
- ReactionCompleted恰好一次且是最后一个fact。duplicate completion、post-terminal item、unknown/unsealed
  primary key、open text和normal EOF without terminal都是protocol fault。

公开OpenAI provider使用Responses API；Chat Completions保留为私有兼容实现。
Built-in target capability在v1仍可比公共fact grammar更窄。内部Chat Completions target当前是text-only：
非空`ToolCatalog`必须在handoff前typed reject，上游返回tool call也必须作为terminal upstream rejection；
这不表示Responses或公共`ReactionPort`失去ToolCall能力。`FrameCapabilities`尚未表达tool support，后续若让
Chat支持streamed tool calls，必须先扩展mount-stable capability negotiation，不能静默改变现有profile。

异常EOF、stream fault或reaction future drop不能依赖adapter再yield Abort。Runtime的RAII canonical
guard独占一份已经验证的transaction buffer；每个可见TextDelta发布前，buffer中对应item已经是
`AssistantTextStatus::Interrupted`，seal只原位改成`Sealed`。正常完成或Drop只做不可失败、无重新验证的
buffer归还，因此secondary admission ledger损坏也不能丢弃已发布tail。partial只有在exact interrupted
replay可以通过admission budget时才能发布，port lowering必须把interrupted状态编码成target可理解的
assistant text加中断边界，或fail closed。

### Required port-private causal state

encrypted reasoning、remote compaction和某些protocol correlation虽然不进入public fact或canonical
transcript，却可能是下一次合法wire continuation必需的因果artifact。port必须按provider output顺序
seal/abort并保留它们。未sealprivate output可以丢弃；已经seal的artifact不因随后stream fault回滚。

port只有在具备content-independent recovery strategy并仍保留其required private state时，才能推进epoch
并声明FullRequired；declare阶段不证明某个canonical Full payload合法。收到exact Full后，port才在
crossing poll前联合验证canonical payload、required private state、wire bytes和token limit。required
artifact丢失后只能执行provider-defined、已验证的stateless rebuild，或fail closed；generic Full不是
万能恢复。request scratch、cache hint、SSE framing和connection state等disposable state可以重建。
如果opaque compaction可能覆盖replaceable System instructions，它的recovery proof还必须绑定生成时的
normalized System snapshot；System change/clear不能仅凭ordinary canonical prefix相同就复用旧artifact。

## 7. ToolCall lanes

ToolCall 使用独立的 per-call lane：

- lane key是`call_id`；
- fact完整校验并写入canonical history后才dispatch/start lane；
- lane启动后fact stream pump立即继续；不同call lanes可以并发；
- 同一lane和所有history mutation严格保序；
- lane result按call identity隔离，ToolOutput按Provider ToolCall ordinal staging；
- valid provider terminal和EOF后，Runtime等待所有已启动lanes完成；
- 下一Frame必须闭合当前全部pending ToolCall，不能先提交无关input。

ToolOutput不是ProviderFact。handler完成时它只进入FrameSession staging；下一次successful Frame handoff才
把它作为outbound canonical input commit。local prepare或pre-handoff submit failure不能消费receipt；
successful handoff只消费一次。

Provider stream与active lanes必须在同一个pump中并发推进，不能先读完整stream再启动tools。后续断流或
终态校验失败不会撤销已经完成的ToolCall副作用或已经组装的ToolOutput。

## 8. 应用编排与 frontend

### Default run and manual scheduling

AgentView不提供framework-owned AgentLoop、public Reactor trait、ReactionHandle或Frame scheduling stream。
`Application::run()`是默认固定循环；组件在`use_preparation`中等待timer、channel、CLI request或parent
invocation：

```rust
let run_result = reactor.run().await;
// Stop and acknowledge business-owned resources here.
let shutdown_result = reactor.shutdown().await;
let reason = run_result?;
shutdown_result?;
```

`reactor`是持有`Application<P>`的变量，不是另一个runtime类型。`run()`内部的每次`react()`先等待组件
准备，再完成一整个structured reaction。Signal dirty、Provider EOF、读取latest和command名称都不会自动
触发reaction。外界发送业务输入即可，不需要额外发送reaction demand。准备hook的factory按声明顺序同步
调用，返回future在同批中并发poll；全部成功后才继续，首个被观察到的错误会drop其余future。

手动单步和现有demand集成仍可显式控制`react()`调用时机；固定`run()`不等待或消费demand。入口任务必须
保留输入，让取消或失败后的preparation重试复用同一业务输入；多个hook的准备条件是AND，任选一个事件到达
则由一个业务入口汇总。`run()`返回后，owner先收尾业务资源，再consuming `shutdown()`。

| Integration | preparation or explicit driver waits for | ReactionPort handoff |
|---|---|---|
| Autonomous Agent | timer、policy、external request或Component demand | model transport accepts request |
| Skill reaction | explicit invocation | owned observation queue accepts Frame |
| Plugin | parent-agent invocation | plugin protocol accepts Frame |

Plugin多parent由外层registry持有多个独立Application。一个port不能切换parent identity并继续使用旧
FrameSession。

### Component entry and demand

high-level application frontend只有一个普通、无root props的Component definition：

```rust
#[component]
fn chess_agent() -> Component {
    // use_signal, provider-event handlers and Component-scoped async primitives
}
```

启动配置由root closure捕获或向普通子Component传props；低层`ComponentHost<Props>`仍可保留显式props
embedding API。root不返回framework completion/program类型，也不需要`ChessApplication`、
`ComponentAgent`或`ApplicationReducer` trait。业务reducer是普通函数，由handler调用并写Signal。

使用旧式demand调度的集成可以保留mount-fenced Component-to-driver channel，但它只表达“至少需要一个
更晚turn”。固定react循环使用preparation和独立exit能力，不需要这个channel：

```rust
let reaction = use_reaction_request();

// A handler or Component-owned task first publishes its state, then requests
// at least one later driver turn.
state.set(next)?;
reaction.request()?;
```

- demand在driver真正wait前后都sticky，不丢通知；
- 多个尚未满足的demand可以coalesce；
- handle绑定mount generation，旧mount不能唤醒新mount；
- 先写Signal再request，下一Frame必须包含该state；
- `request()`不重入当前reaction，也不直接render或submit；
- 没有Application orchestration capability的低层Host调用该hook时触发Component contract panic；render
  candidate不publish。

`use_reaction_request()`只返回这一项capability；不能恢复`use_loop()`或让Component直接控制通用host
lifecycle。

### Skill / CLI frontend

Skill的普通frontend与reaction exchange分开：

```text
agentview                       -> latest committed projection
agentview <subcommand> <args>   -> typed Component operation, then latest projection
explicit reaction invocation    -> Application<SkillPort>::react()
```

latest只读，不render、不submit。typed subcommand可以更新Signal并按需要发出driver demand，但它本身不是
ProviderFact，也不隐式开始reaction。只有明确的reaction invocation进入SkillPort exchange；delayed act
必须绑定active adapter ingress generation。stream结束或drop后late act明确返回stale，不能进入下一轮。

Plugin message遵守同一reaction-local stale fence；correlation属于adapter protocol，不恢复public core
`FrameId`。

### Declarative provider event handler

Provider event handler在`view!`外声明；`view!`只描述完整projection。目标authoring shape是：

```rust
use_provider_event_handler(ProviderEvent::TEXT, move |event| {
    let state = state.clone();
    async move { state.set(reduce(event)) }
});
```

selector决定callback接收的typed Event；callback每次调用创建一个Future。Runtime完整await Future；returned
error作为当前reaction handler fault，callback invocation或Future panic原样向caller传播。

mounted handler identity是`ComponentId + HookSite + MountGeneration`，跨reaction retained；每次render
generation派生一次private binding。successful rerender原子更新callback capture；abandoned render保留旧
slot；unmount删除slot。旧binding不能dispatch到新mount。公共API不暴露`EventInput`、
`EventListener::observe`、listener identity/version或DOM bubbling/capture。

### User panic boundary

Runtime不在Component root/render、provider-event handler、legacy event listener、native tool handler、legacy
streaming decoder、engine observer或usage observer外层调用`catch_unwind`。这些user-code panic不转换成typed fault、不吞掉、
也不通过ErrorBoundary恢复旧Application。正常返回的`Result::Err`仍按各自typed contract处理。

render candidate、hook topology、listener declaration和task factory仍遵守commit-before-publish；panic unwind时未发布
candidate由RAII丢弃。这只证明candidate未publish，不构成对任意user side effect、共享`Arc`或live Signal mutation
的rollback保证。caller若自行catch，必须丢弃该Application以及可能被这次调用触及的可变user资源；继续复用它们
超出Runtime contract。v1不为这种外部catch建立状态隔离、回滚、恢复或terminalization协议。

panic unwind期间Runtime不得再次进入任何user callback或observer；RAII cleanup只销毁reaction-local资源。
因此panic路径不保证发送terminal/cleanup observation，原panic payload优先。

普通callback的production panic interception仅用于不能自然跨栈传播的runtime/ownership边界：Tokio把spawned task panic编码为
`JoinError`，supervisor取回原payload并在outer driver boundary调用`resume_unwind`；Drop catch只允许在正在销毁的
future/reaction上仲裁primary payload。两者都不得转换成业务`Result`或恢复被处理对象。workspace不覆盖Rust默认
`panic = "unwind"`profile；未来`extern "C"`边界若存在，必须阻止unwind跨ABI且不能冒充业务恢复。

新的managed streaming worker同样跨越ownership边界：它暂存decoder/reducer的原始panic payload，在
receipt清理确定后向原调用方或下一次恢复边界`resume_unwind`，不会把原始panic替换为普通业务诊断。
原等待方消失时payload仍由worker保留。只有显式managed adapter和拒绝continuation采用其专用故障契约：
adapter future panic保留预先记录的operation并按不确定结果恢复，`on_rejected`的error/panic成为terminal
adapter fault。这不扩大普通Component callback的panic恢复范围。

本节冻结的是frame-driven Component/Application Runtime及上面列出的callback。legacy `AgentTurnObserver`的
post-commit fire-and-forget contract不在本次Phase 8中暗改；它若迁移到同一panic policy，必须先引入可在outer
boundary取回panic payload的supervised observer owner，不能从Drop启动callback或丢弃Tokio `JoinHandle`。

### Component-scoped async lifecycle

Component通过三种不同生命周期的primitive拥有业务sidecar；它们不属于Provider transport，也不属于一次
reaction barrier：

```rust
pub fn spawn<F>(future: F) -> Result<(), SpawnError>
where
    F: Future<Output = ()> + Send + 'static;

pub fn use_future<Factory, F>(factory: Factory)
where
    Factory: FnOnce() -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static;

pub fn use_coroutine<Message, Factory, F>(
    capacity: usize,
    service: Factory,
) -> Coroutine<Message>
where
    Message: Send + 'static,
    Factory: FnOnce(CoroutineInbox<Message>) -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static;
```

- `spawn`用于一次性工作，只能从已经commit的provider-event handler、`use_future`/`use_coroutine` task或
  其nested task调用；render期间或没有Component task context时返回typed `SpawnError`；
- `use_future`是lexical hook。每个mount只调用一次factory并启动一个future；同一mount的rerender不重启，
  新render传入但未使用的factory被丢弃；
- `use_coroutine`也是lexical hook。每个mount只启动一个service，并跨rerender保留同一个bounded typed
  sender。`capacity`是mount-time配置；`send(message).await`提供backpressure，stale/closed rejection归还
  原message；
- bootstrap和后续mount使用相同规则：完整projection和hook topology commit后立即注册task。注册本身不poll
  Component future；supervisor随后异步执行；
- 第一次task注册把supervisor绑定到当前Tokio runtime identity；后续start、retire、monitor wait和shutdown必须
  在同一runtime执行。foreign runtime不能向原actor排队；首次检测到identity mismatch就同步fence为`Closed`、
  关闭retirement waiter并在handoff前失败。owning runtime关闭导致actor future被drop时，RAII finalizer执行同一
  fail-closed transition。`react()`、blocking demand wait和nonblocking demand take都在读取或消费driver state
  前经过同一个outer-boundary arbiter；`Closed`不能返回`Ok(false)`或`Ok(true)`，也不能继续declare、render或submit；
- unmount先关闭该mount的Signal、demand、handler、spawn和coroutine capability，再abort并await全部owned
  tasks；只有retirement完成后才能发布replacement projection并提交引用新mount的Frame。永久stale authority
  属于mount共享的`MountFence`：registration持有该fence直到`Start`入队，unmount先invalidate同一fence再发送
  `Retire`，所以要么`Start`排在`Retire`之前，要么根本不会发送。core mutex串行化发送，actor单一FIFO receiver
  保留该顺序；retirement完成后core/actor只需删除临时claim，不保留历史`ComponentId` watermark；
- task正常完成不dirty、不request demand、不render、不submit。需要新turn时task先写Signal，再调用自己捕获的
  `ReactionRequest::request()`；
- framework不接管task的普通Result，也不显式`catch_unwind` user future。Component future在内部处理业务成功/
  失败并写自己的Signal；Tokio task boundary把第一项未捕获task panic作为`JoinError`交给supervisor，后者保留
  原payload，并在当前正在poll或下一个可用的outer driver boundary直接`resume_unwind`。如果当时
  没有active driver，payload保留到下一次`react()`、driver demand wait或integration driver调用；
- `react()`和两个driver demand入口在返回任何既有terminal classification前，必须再次仲裁尚未消费的task
  panic。fresh payload优先于其他更早存在的typed terminal；payload已经传播后，后续调用只返回
  task-panic terminal fault，不能二次unwind。最终仲裁之后才发生的panic由下一outer boundary传播；
- supervisor观察panic后同步发起sibling abort，但不能把sibling drain或destructor完成作为开始unwind的前置条件。
  unwind前Application进入terminal；caller若主动`catch_unwind`，后续API只允许确定fail closed，不承诺复用。
  caller随后若consuming shutdown该terminal Application，shutdown仍要join已经abort的tasks并等待destructor，
  再返回terminal fault；这不能反向阻塞最初的unwind。

hook order/kind是commit topology的一部分；`use_future`、`use_coroutine`与其他hook发生order、kind或count
drift属于Component contract panic并直接unwind，candidate不publish，已commit mount和tasks保持不变。
`use_resource`/`use_action`需要额外的
reactive result与重复action policy，当前不属于本协议。

## 9. Unsupported ToolCall

正常Frame只暴露当前Component tree声明的tool capability。即使target返回没有可用Component的ToolCall，
Engine也不能伪造ToolOutput，更不能发送包含裸pending call的下一Frame。

处理规则：

1. 完整ToolCall先进入canonical history并登记pending；
2. Runtime查找matching ToolCall Component；
3. 未声明tool返回`unsupported_tool_call`；
4. 已声明但本轮无binding返回`tool_binding_missing`；
5. handler正常返回但无ToolOutput返回`tool_output_missing`；
6. 普通reaction中lane future返回错误、取消或执行设施失败时返回`tool_lane_failure`；drop整个`react()`则按下节
   物化unknown-outcome fallback；user handler panic直接unwind；
7. fault后不得提交下一Frame，open history不能作为合法wire input；
8. pending call不得通过丢continuation、Fresh、context reset或插入无关input绕过；
9. 无法回答的call终止当前logical target session。

可表达的工具业务失败应由ToolCall Component产生明确失败ToolOutput；基础设施失败才中止reaction。

## 10. 取消、失败与 remount

取消发生在handoff前时，drop Pending submit future不推进任何state。取消发生在handoff后时：

- 先drop当前callback future，再drop Runtime拥有的pending lane futures，最后drop fact stream；
- 保留已经handoff的input、已经接纳或seal的facts和已经组装的ToolOutputs；
- recovery guard把open public partial标记`Interrupted`，并为每个未完成的已接纳ToolCall物化exactly one
  `Tool execution was cancelled; its outcome is unknown.` ToolOutput；
- port按自己的guard处理已经seal或尚未seal的private output；
- 保留取消前成功的Signal写入和不可逆外部副作用；
- callback和lane future一旦drop就不resume、不replay；
- Application保持ready，但不自动retry、Full或开始下一reaction；外部driver显式调用下一次`react()`后，port必须
  声明兼容`Accepted`、更高epoch `FullRequired`或terminal fault；
- direct/supervised panic不走上述cancellation recovery且不物化fallback；catch user panic后仍不得复用Application。

port fault、Component handler returned fault、lane returned fault和terminal fault必须带明确stage/reason；
user panic不分类为fault。清理完成是终止
条件的一部分；fact stream、handler future和reaction-owned lanes不能遗留到下一轮。Component-scoped
long-lived tasks属于mount，正常reaction结束不取消；unmount或Application teardown时必须cancel并await。

每个持有Application的production integration必须提供consuming async shutdown；普通Rust `Drop`只允许作为
emergency best-effort abort，不能证明cleanup完成。External shutdown在API调用时立即把唯一owner移入专用cleanup
task：先cancel并join active reaction、取回Application，再fence mounts并abort + await全部Component tasks。
丢弃shutdown waiter不取消这个cleanup owner。CLI/daemon只能在该cleanup完成后回复shutdown成功；typed cleanup
failure回复错误，不能先ACK再Drop state。Agent、Skill或Plugin在形成production Application owner时必须复用同一
lifecycle contract，crate-private port role本身不是Application owner。

Component remount创建新runtime identity。旧canonical facts不撤回；新identity作为新projection node参与
后续reconciliation。旧mount的handler、lane、Signal、task和demand handle不能写入或唤醒新mount。

## 11. Observability

Observer观察与Engine相同的因果顺序，但不拥有或修改state。至少记录：

- reaction identity、TargetIdentity、epoch和FrameRevision；
- declaration结果、Full/Delta basis和handoff boundary；
- ProviderFact/output ordinal与lane/call identity；
- input submitted、partial committed、item sealed/aborted；
- handler/lane started/completed和ToolCall pending/closed；
- canonical budget与provider wire gate accepted/blocked；
- terminal stage、reason和cleanup outcome。

Fact不得先被Observer或Component看见、之后才commit canonical history。Provider的
`response.completed`只表示adapter terminal frame已经校验，不能记录成整个history或reaction commit。

## 12. 必须保持的不变量

1. Component state是业务权威；canonical transcript是shared causal truth；Provider private continuation
   不是业务数据库。
2. `RenderedProjection`始终完整；dirty tracking只能用于内部reconcile优化。
3. 一个Application固定一个logical target、一个FrameSession和一个ReactionPort；不公开替换port或mutable
   session API。
4. `declare()`是幂等snapshot read；相同declaration不推进state或invalidate Frame。
5. TargetIdentity和FrameProfile在mount期间稳定；epoch在同一identity内单调且不得复用。
6. Frame prepare不推进history、revision、diff baseline或ToolOutput receipt。
7. submit返回Pending/Err都证明未handoff；crossing poll必须同poll Ready(Ok)。
8. successful submit后的private commit同步、不可失败且无await；post-handoff fault不回滚。
9. Full/Delta由private FrameSession决定；port不能读取complete checkpoint或自行重算semantic diff。
10. matching head只是Delta必要条件；v1 replay view必须append-compatible，non-append view在缺少
    occurrence provenance时fail closed；未来provenance-bearing replacement必须Full。scope变化会把旧scope
    provider occurrences和未reconcile canonical tail转为持久ambiguity fence，value-only跨scope claim
    必须typed fail closed。
11. Component envelope、total canonical Frame和provider wire/token limits分别检查，且都不truncate。
12. 每个visible fact先commit canonical history，再dispatch Component event。
13. normal EOF必须有且只有一个valid ReactionCompleted；EOF本身不等于reaction成功。
14. partial admission必须保证interrupted replay可编码；fault/drop物化已经发布的实际文本和中止事实。
15. required port-private causal artifact不是可丢cache；丢失后只有proven rebuild或fail closed。
16. ToolCall commit后立即启动lane；fact stream和lanes并发推进，不同lanes可以并发但mutation保序。
17. ToolOutput只在下一Frame successful handoff时作为ordered input commit，receipt只消费一次。
18. 下一Frame必须闭合全部pending ToolCall，不能以Fresh、Full或context reset绕过。
19. 一次`react()`最多handoff一个Frame，不自动rerender、retry或开始下一reaction。
20. Signal dirty、读取latest、Provider EOF和task completion都不自动调用`react()`。
21. `Application::run()`是默认固定循环；direct `react()`是手动单步接口。Component preparation决定何时
    允许提交，exit决定正常结束；兼容的Component demand只表达sticky demand，固定循环不依赖它。
22. Skill latest/subcommand不隐式render或react；late act/plugin message不能越过ingress generation。
23. cancellation不回滚已commit Component state、canonical fact、sealed private artifact或外部副作用。
24. mounted handler/task/demand capability受mount generation fence，旧mount不能影响新mount；unmount先fence，
    再abort并await tasks，之后才能发布replacement projection。
25. pre-handoff和post-handoff `react()` cancellation都保持Application ready且不自动开始reaction；post-handoff
    保留已commit/admit的state，把open text标记`Interrupted`，并以exactly one预留
    `Tool execution was cancelled; its outcome is unknown.` ToolOutput闭合每个未完成的已接纳ToolCall。下一次
    显式`react()`必须从port的truthful continuity declaration继续。
26. Runtime不吞掉Component/render/handler/tool/parser/observer的原始panic；managed streaming的临时payload
    保留和adapter例外遵循上面的User panic boundary。Tokio task panic是另一种无法自然跨栈
    的user-code例外：必须在当前/下一outer driver boundary用原payload直接stack unwind。sibling abort立即发起，
    但drain不能阻塞unwind。caller自行catch任何user panic后都不能恢复Application的业务执行，并必须丢弃可能已
    mutate的user资源；Runtime不提供poisoned-state recovery。task panic在resume前额外把Application标记terminal，
    仅允许后续API确定fail closed或consuming shutdown，不能重复传播、declare、render或handoff。
27. task supervisor第一次启动actor后固定Tokio runtime identity；foreign/terminated runtime必须在下一次driver
    observation或task operation时确定`Closed`，不能继续handoff或留下永久Pending retirement。
28. production owner的shutdown acknowledgement必须发生在active reaction join、mount fence和全部task destructor
    完成之后；取消shutdown waiter不能取消已经取得唯一Application ownership的cleanup。
29. FrameSession prepare和Provider handoff前，当前operation的全部active preparation必须完成，且其
    Signal写入必须先reconcile。同一execution wave的所有未完成slot并发poll，只有全部`Ok(())`后才reconcile；
    首个被观察到的error会drop该wave其余future。最多16个execution waves，clean且无unfinished hook时early success；只有
    dirty的第16 wave之后才有一个不得dispatch新factory/future的final reconcile。success必须同时clean且无
    unfinished hook；若仍需另一wave，零handoff并以`PreparationGraphUnstable` terminal fail closed。
30. 每次显式`prepare()`或`react()`都创建新operation，并使每个active `(mount generation, lexical slot)`
    运行一次；rerender跳过本operation已完成slot，新mount运行。ordinary failure或pre-handoff cancellation
    不自动重试，并保留同一个Application供下一次显式operation使用；下一次从全部active hooks重新开始。
    同一`react()`的continuity retry不得重新运行preparation。
31. 同步mount可以公开provisional bootstrap projection；`ProjectionSnapshot::is_prepared()`是已发布projection
    的checkpoint而不是readiness cache。dirty可以与prepared同时为true；新的含preparation declaration的
    projection在当前operation完成前为provisional，prepared bit不能跳过未来operation。
32. preparation factory和future在mount fence外执行，dispatch前authorize mount且完成时verify mount；
    retired generation不能record completion或publish late Signal write，retirement前Signal写入仍
    authoritative且nontransactional。Runtime只等待返回future，不保证detached work；它不提供exactly-once，
    factory/future必须tolerate repeated/partial execution和cancellation。direct factory/future panic原样unwind。
    普通preparation error为`Preparation` stage/reason，持续remount graph为terminal
    `PreparationGraphUnstable`，legacy host遇到declaration为`ComponentPreparationsUnsupported`。

## 13. 非目标

本文不规定：

- Chess、裁判或其他业务领域规则；
- 某个ToolCall的业务schema和执行实现；
- concurrent multi-target writers、multi-checkpoint delta、fork或durable session recovery；
- public mutable history/session API；
- framework-owned AgentLoop、public Reactor trait或Component-owned scheduling policy；
- task retry/backoff、durable jobs、unbounded coroutine inbox或resource/action最终API；
- global preparation cache、readiness store或resource subsystem；
- Skill subcommand schema、Plugin multiplexing wire syntax或UI/JSONL展示格式；
- 实现顺序、迁移进度和发布里程碑。
