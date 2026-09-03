# Frame-driven Runtime 新计划

日期：2026-08-28

状态：**Phase 0 至 Phase 8 gates 已独立关闭；Phase 9 Tasks 1-6、FDR-048至FDR-050及FDR-052已独立签收。FDR-051在accepted CLI bytes上因legal fragmented/joint act再次reset/15秒timeout而重开，post-closure RED已在source edit前冻结。disposable foreground profiling将缺陷定位到CLI-owned重复large serde/allocation、identical-text metering与Full preparation；scoped candidate改为structured bounded header/raw-payload/footer transport，但client/server均将original `DaemonRequest` serde stream直接写入HMAC，保持accepted MAC input byte-exact，且三段read共用同一absolute 1秒request timeout。response仅bounded serialize一次，经complete serde string-only validation后byte-exact写stdout；adjacent equal text仅复用exact serializer contribution，不丢弃event。legacy-MAC、non-string rejection、shared-deadline、allocation/security/NoRetry deterministic proofs通过；五轮alternating focused cycles、双feature各三轮complete CLI `24/24`、两轮all-feature及一轮no-default full matrix、dual check/rustdoc/strict-Clippy、FDR-052 package boundary、format/diff、119-path freeze与cleanup均通过。当前仅为implementation candidate；FDR-051、Task 7与Phase 9保持open，其他audit未恢复，等待独立复核。**

本文重新审视了 `engine.md`、`agent-loop-public-api-review.md`、
`reactor-driven-reactions.md` 以及当前 Responses、Chat Completions、External 三条执行路径。
它吸收今天讨论中仍然成立的部分，也重新评估已经舍弃的 `Frame`、`FrameSession`、
`AgentLoop`、`use_loop`、middleware 和 external `observe / act` 方案。

`engine.md` 是当前权威目标 contract；本文只保留实施顺序、迁移锚点和test gates。
`agent-loop-public-api-review.md` 与 `reactor-driven-reactions.md` 均已 superseded，不再作为实现或public
API来源。

配套架构图见
[`frame-driven-runtime.architecture.html`](frame-driven-runtime.architecture.html)，可编辑源文件是
[`frame-driven-runtime.architecture.json`](frame-driven-runtime.architecture.json)。

## 0. Review 输入与冲突处理

本计划使用了三条独立 review：

| Review | 主要结论 | 本计划如何处理 |
|---|---|---|
| Agent / Skill / Plugin boundary | 公共核心应是固定 port 的 Frame lifecycle，不是 Provider-shaped wrapper | 采用 `Application<P> + private FrameSession + ReactionPort` |
| Provider responsibility/source audit | 当前 one-method ProviderPort 无法表达 pre-render declaration 和 real handoff；Responses/Chat 重复 history/diff | 改为 declare/submit 两阶段，并按源码锚点分阶段迁移 |
| Frontend/async lifecycle audit | 首次 mount 会 bootstrap deadlock；notifier 和 task runtime 不能靠名字解决 | 增加 bootstrap；把 mount-fenced reaction demand 与 async hooks放在独立阶段 |
| Phase 0 adversarial protocol review | cancellation window、private causal artifact、budget closure、Full precondition、terminal grammar和phase dependency未闭合 | 增加poll-level handoff、required/disposable private state、JCS Full reserve、`prepared_against`、complete fact grammar并重排phases |

交叉review还否决了四项初稿：public Frame不得泄漏complete checkpoint；history不得重新引入
`HistoryEffect` side channel；declaration不需要nonce/lease；v1不能假设一种尚未定义的canonical semantic
compaction。本文采用幂等snapshot + exact Frame precondition，并把v1 replay固定为CompleteTranscript。

## 1. 结论

新的公共中心不是 `ProviderPort`，也不是一个框架拥有的 `AgentLoop`，而是一次结构化 reaction：

```text
Agent / Skill / Plugin driver
        |
        | decides when one reaction is needed
        v
Application<P>::react()
        |
        | port declaration -> complete Component reconcile
        | -> private Frame prepare -> port.submit(Frame)
        | -> ordered facts -> history commit -> Component dispatch
        | -> post-reaction reconcile
        v
reaction complete
```

核心决定如下：

1. `RenderedProjection` 是 Component 的完整、retained、provider-neutral DOM；它从来不是 delta。
2. `Frame` 是给一个 logical target handoff 的 target-ready 值；它已经完成 canonical history
   reconciliation 和真正的 `#[diff]` lowering。
3. public `Frame` 只携带本次 exact `Full` 或 `DeltaFrom` submission。完整 DOM、完整 checkpoint、
   reaction bindings 和 commit candidate 只存在于 private `PreparedFrame`。
4. canonical history、`#[diff]` compiler、ToolOutput staging 和 Frame cursor 从 Provider 实现中移到
   private `FrameSession`；Agent、Skill、Plugin 共用这一套 kernel。
5. `ReactionPort` 只保留 integration-specific state；Provider实现中包括 response id、reasoning/private
   output、prompt cache、compaction artifact、SSE ledger 和 transport state。
6. `ReactionPort` 在 Component reconcile 前声明自己已接受的 revision、稳定 profile、budget 和 capability；
   Component 只看稳定的 `FrameContext`，看不到 `Full / DeltaFrom` 或 target cursor。
7. 外层 driver 拥有何时调用 `react()` 的策略。Signal dirty、读取 latest、Provider EOF 都不会自动
   开始下一次 reaction。
8. 不引入 `HistoryEffect`。Runtime 消费一条有序的 `ProviderFactStream`，从同一个 fact 先提交
   canonical history，再把零个或一个 Component event dispatch 给 bindings。

## 2. 术语与所有权

| 名称 | 可见性 | Owner | 语义 |
|---|---|---|---|
| `RenderedProjection` | public read-only value | Component Runtime | 一次成功 reconcile 的完整 DOM |
| `ProjectionRevision` | internal/read-only metadata | Component Runtime | latest rendering 是否陈旧，与 Frame revision 无关 |
| `FrameProfile` | public target contract | logical target session | mount 期间稳定的 Component envelope、Frame limit、token hints 和 capabilities |
| `TargetIdentity` | public opaque value | ReactionPort | 一个 port instance 所代表的 mount-stable logical target identity |
| `TargetEpoch` | public opaque value | ReactionPort | port 丢失 delivery continuity 时推进的 epoch |
| `TargetContinuity` | public declaration | ReactionPort | `FullRequired` 或仍被承认的 `FrameRevision` |
| `TargetDeclaration` | public immutable value | ReactionPort | continuity + mount-stable `FrameProfile` |
| `Frame` | public immutable value | structured handoff | exact target-visible submission，不包含 private full checkpoint |
| `FrameRevision` | public opaque token | private FrameSession | target-session scoped delivery revision，不是 dispatch authority |
| `PreparedFrame` | private, non-cloneable | Reaction Runtime | public Frame + full checkpoint + bindings + commit candidate |
| `FrameSession` | crate-private in v1 | one `Application<P>` | canonical history + one active target delivery state |
| `ReactionPort` | public integration boundary | Agent/Skill/Plugin adapter | 声明 continuity，并把 `Frame` 提交成 fact stream |
| `ProviderFact` | target-neutral ordered fact | Provider/Skill/Plugin adapter | history 可以直接接纳的模型输出事实 |
| `Application<P>` | public orchestration owner | outer driver | Component + private FrameSession + one fixed ReactionPort |
| external driver | integration role only | Agent/Skill/Plugin | 等待 timer、request、CLI、wake 或 parent invocation后调用 `react()` |

### 2.1 第一版 session scope

第一版一个 `Application<P>` 同时只绑定一个 `ReactionPort` 和 active logical target session：

- canonical history 只有一份，不再由 Responses、Chat、Skill 各自重复实现；
- port object 在多轮 reaction 中由 `Application` 固定拥有，不能通过每次 `react()` 换入另一个 target；
- port 丢失 remote/wire continuity 时推进 `TargetEpoch` 并声明 `FullRequired`；canonical history保留，
  delivery/diff cursor在下一次 Full handoff时 rebase；
- `FrameProfile` 改变在 v1 fail closed，不在同一个 mount 内产生多套 budget-dependent DOM；
- 多个 target 并发写同一个 canonical history 暂不支持。

这并不限制三个 use case 使用同一个 kernel。Agent、Skill、Plugin 是三种 adapter 和 scheduling
shape，而不是三套 history/diff 实现。未来若一个 logical history 需要同时服务多个 target cursor，
再把 private `FrameSession` 拆成 `CanonicalHistoryState + TargetDeliveryState`，并增加 epoch/CAS
transaction coordinator；第一版不提前公开 mutable session registry。

一个 port instance 永久代表一个 logical target。`Application` 在 mount 时固定 opaque
`TargetIdentity`，后续 declaration identity 变化立即 fail closed。`TargetEpoch` 在同一 identity 内单调
推进且不得复用，它只表示同一 target 的 continuity reset；真正切换 Provider account、Skill session
或 Plugin parent 时必须创建新的 `Application<P>`，不得继承旧 Application 的 canonical history。

`Application` 不公开 `port_mut()` 或替换 port 的 API。Skill/Plugin 若需要从外部注入 act、消息或
invocation，应在把 port 移入 `Application` 前取得 cloneable control handle；control handle不能直接
提交 Frame或修改 FrameSession。

### 2.2 Canonical、delivery 与 Provider-private state

| State class | 第一版 owner | 内容 |
|---|---|---|
| `CanonicalHistoryState` | private FrameSession | 已 handoff的append-only ordinary canonical input；已接纳的public text partial/seal/interrupt；ToolCall、ToolOutput和deterministic causal log；不含replaceable System snapshot |
| `TargetDeliveryState` | private FrameSession | FrameSession namespace、current revision、retained complete checkpoint、normalized System snapshot、selected replay-view identity、`#[diff]` baseline、staged ToolOutput receipt和disposable `PreparedFrame` candidate |
| required port-private causal state | ReactionPort | encrypted reasoning/private output、sealed provider compaction artifact、ordered private output span和 protocol-required continuation/correlation item |
| disposable port-private state | ReactionPort | request scratch、response id/remote cursor hint、prompt cache hint、SSE framing、completed ledger、connection和retry state |

第一版由同一个 crate-private `FrameSession` 原子持有前两类，但它们不是同一类 history；未来支持多个
target cursor时，可以在不改变 Component API 的前提下拆开。

Required private causal state 不进入 shared canonical history，但也不是 cache：port 必须按 provider output
顺序 seal、abort并保留它，直到 provider protocol 不再需要。remote compaction 只有在完整验证并 seal
对应 private output 后才能安装；安装后不因后续 stream fault 回滚。pre-handoff/local compaction
candidate和未 seal private output可以丢弃，但如果丢弃后无法构造合法 continuation，不能继续声明原
accepted head。

port 只有在具备 content-independent recovery strategy、并保留该 strategy 所需的 required private
state时，才允许推进 `TargetEpoch` 并声明 `FullRequired`。这是 capability proof，不是 exact candidate
proof：`declare()` 看不到 canonical history。`submit(frame)` 收到 exact Full 后，必须在 crossing poll前
把 canonical payload、required private artifacts、真实 wire bytes和token limit一起验证；失败返回
structured、确定 pre-handoff 的 fault。required artifact 丢失后，port只有两条合法路径：执行
provider-defined、已验证的 stateless rebuild，或者在 declaration 阶段 fail closed；不能把 generic
`Full` 描述为无条件恢复。
opaque compaction若可能覆盖System instructions，还必须绑定生成时的normalized System snapshot；Full
recovery不能只验证ordinary canonical prefix就跨System change/clear复用该artifact。

Disposable state 丢失可以重建，并且不改变 shared canonical history 或 Component state。Provider wire
body/token budget 与 canonical Frame budget 分开校验，private artifact 不计入 canonical byte meter。

### 2.3 HistoryPolicy 的位置

`HistoryPolicy` 保持 private pure-function 职责：从只读 `CanonicalTranscript` 和稳定
`FrameProfile` 选择 replay view。它不拥有 history、不提交 Event、不推进 Frame cursor，也不参与
ToolOutput receipt。

v1 只允许一种 canonical replay view：`CompleteTranscript`。它按原顺序包含全部committed append-only
ordinary canonical items；不能删前缀、生成summary、替换closed turn或注入provider artifact。
Component-owned System是独立的replaceable snapshot，不进入CompleteTranscript，也不构成FDR-021所defer的
ordinary replay replacement。FrameSession仍验证全部pending ToolCall会被本次staged ToolOutput闭合。完整
replay无法满足Full reserve时返回typed`CanonicalReplayTooLarge`并终止当前session的后续handoff，不进行
隐式truncate或semantic compaction。

这个保守规则让“合法next-Full是否存在”成为可执行判定，也避免Phase 0发明无法验证等价性的summary
协议。未来增加canonical checkpoint/summary时，必须先定义它作为显式canonical fact的生产者、覆盖区间、
digest、ToolCall closure和每个selected occurrence的origin/provenance grammar。该扩展存在后，
replay-view replacement强制下一Frame为Full；扩展落地前non-append view必须typed fail closed，不能仅凭
item value猜来源。该扩展不能改变v1 CompleteTranscript的含义。

`ReactionPort::declare()` 看不到 canonical history；port只收到已经 materialize 好的exact
`FrameSubmission`。Provider-specific reasoning、encrypted items和remote compaction reference不进入
canonical view，仍由Provider wire session私有保存。当前provider-specific `HistoryPolicy`命名需要在
迁移时拆分或重命名，避免把wire encoding误认为shared history ownership。

## 3. 候选公共形状

以下签名用于固定职责，不是最终命名承诺：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetIdentity(NonZeroU128);

impl TargetIdentity {
    pub const fn new(value: NonZeroU128) -> Self;
    pub const fn get(self) -> NonZeroU128;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetEpoch(NonZeroU64);

impl TargetEpoch {
    pub const fn new(value: NonZeroU64) -> Self;
    pub const fn get(self) -> NonZeroU64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameRevision {
    // opaque FrameSession namespace + target identity + epoch + monotonic sequence
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetContinuity {
    FullRequired { epoch: TargetEpoch },
    Accepted {
        epoch: TargetEpoch,
        revision: FrameRevision,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBasis {
    Full,
    DeltaFrom(FrameRevision),
}

pub struct FrameConstraints {
    // Exact hard limit over the complete canonical FrameSubmission encoding.
    pub max_frame_bytes: usize,

    // Mount-stable guaranteed envelope for the complete Component projection
    // and the Component-declared ToolCatalog.
    pub max_component_bytes: usize,

    // Target estimates exposed as hints, never used as the core byte meter.
    pub context_window_tokens: Option<u64>,
    pub reserved_output_tokens: Option<u64>,
}

pub struct FrameProfile {
    pub constraints: FrameConstraints,
    pub capabilities: FrameCapabilities,
}

pub struct TargetDeclaration {
    identity: TargetIdentity,
    continuity: TargetContinuity,
    profile: FrameProfile,
}

impl TargetDeclaration {
    pub fn full(
        identity: TargetIdentity,
        epoch: TargetEpoch,
        profile: FrameProfile,
    ) -> Self;
    pub fn resume(
        revision: FrameRevision,
        profile: FrameProfile,
    ) -> Self;
}

pub struct Frame {
    revision: FrameRevision,
    target: TargetIdentity,
    epoch: TargetEpoch,
    prepared_against: TargetContinuity,
    prepared_profile: FrameProfile,
    basis: FrameBasis,
    submission: FrameSubmission,
}

pub struct FrameSubmission {
    // Exact target-visible canonical payload + ToolCatalog.
    // Delta submissions do not expose the private complete checkpoint.
}

impl FrameSubmission {
    /// Exact sections for this Full or Delta submission only.
    pub fn replay(&self) -> &[CanonicalInputItem];
    pub fn staged_inputs(&self) -> &[CanonicalInputItem];
    pub fn projection(&self) -> &ProjectionSubmission;
    pub fn tools(&self) -> &ToolCatalog;

    /// Stable framework encoding used by max_frame_bytes accounting.
    /// A Provider may still lower the structured items into its own wire format.
    pub fn canonical_bytes(&self) -> &[u8];
}

impl Frame {
    pub fn revision(&self) -> FrameRevision;
    pub fn target(&self) -> TargetIdentity;
    pub fn epoch(&self) -> TargetEpoch;
    pub fn prepared_against(&self) -> &TargetContinuity;
    pub fn prepared_profile(&self) -> &FrameProfile;
    pub fn basis(&self) -> FrameBasis;
    pub fn submission(&self) -> &FrameSubmission;
}

pub type ProviderFactStream<'a> = Pin<
    Box<
        dyn Stream<Item = Result<ProviderFact, ReactionPortFault>>
            + Send
            + 'a,
    >,
>;

#[async_trait]
pub trait ReactionPort: Send {
    /// Declares continuity and a mount-stable profile. It cannot inspect
    /// canonical history or advance delivery state.
    fn declare(&mut self) -> Result<TargetDeclaration, ReactionPortFault>;

    /// Every `Pending` and `Ready(Err(_))` proves the Frame has not crossed
    /// the handoff boundary. The poll that first causes real or ambiguous
    /// delivery must return `Ready(Ok(stream))`; every later failure is
    /// yielded by that stream.
    async fn submit<'a>(
        &'a mut self,
        frame: Frame,
    ) -> Result<ProviderFactStream<'a>, SubmitFault>;
}

pub enum SubmitFault {
    /// Guaranteed pre-handoff; Application may discard and re-prepare.
    ContinuityChanged,
    /// Guaranteed pre-handoff; profile is mount-stable, so this mount terminates.
    ProfileChanged,
    /// Any other guaranteed pre-handoff port failure.
    Rejected(ReactionPortFault),
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

pub struct Application<P> {
    // private Component runtime + FrameSession + fixed P
}

impl<P: ReactionPort> Application<P> {
    pub fn mount(
        root: impl Fn() -> Component + Send + Sync + 'static,
        port: P,
    ) -> Result<Self, ApplicationFault>;

    pub fn current_projection(&self) -> ProjectionSnapshot<'_>;

    pub async fn react(&mut self) -> Result<(), ApplicationFault>;
}
```

`submit()` 返回 `Ok(stream)` 就是唯一 public handoff proof，不增加 `AcceptedFrame`、declaration nonce
或 public `commit()` wrapper。它的 Future 具有逐 poll cancellation contract：任何返回 `Pending` 的
poll都证明 Frame 尚未 handoff；第一个可能造成真实或 ambiguous delivery 的 poll必须在同一个 poll
返回 `Ready(Ok(stream))`。`Application::react()` 紧接着做一次同步、已预验证、不可失败的 private
commit；`Ok(stream)` 和 commit 之间没有 `.await`。boundary 后的 fault只能由 stream yield。
post-handoff `Retryable` fault或unfinished stream Drop可推进epoch；Drop后的下一次`declare()`必须如实给出
仍可使用exact continuation的兼容`Accepted`、更高epoch `FullRequired`或terminal declaration fault，不能继续
声明stale `Accepted`。post-handoff `Terminal` fault成为sticky logical-target fault，后续`declare()`不能重新
给出`FullRequired`。

`Frame` 携带 declaration 的 target identity、epoch、exact `prepared_against` continuity和选定 basis。
port必须在 real delivery 前把 identity、prepared-against和完整`prepared_profile`同自己的当前 snapshot
重新比较；只有仍然匹配的 Frame可以提交。Full也必须携带precondition：例如compiler因为canonical rebase从
`Accepted(epoch, R1)` prepare Full，submit前port变成`Accepted(epoch, R2)`时必须拒绝，不能因为basis是
Full就绕过race fence。profile-only变化返回确定pre-handoff的`SubmitFault::ProfileChanged`。

Frame只由crate-private validated constructor创建。constructor强制：新revision内嵌的target/epoch与
Frame外层字段相同；`prepared_against.epoch()`相同；Accepted revision属于同一target/epoch；
`DeltaFrom(base)`精确对应prepared-against中的Accepted revision；base与新revision属于同一FrameSession
namespace且sequence严格递增。Full从Accepted prepare时仍绑定exact target/epoch/precondition，但Accepted
revision不必与新revision同namespace，也不约束新revision sequence；successful handoff把port rebase到
当前FrameSession的新revision。

`ReactionPortFault`与`ApplicationFault`只暴露payload-free structured classification。前者固定为
`kind/code/reason`，后者固定为`stage/kind/code/reason`；provider/model text、wire payload、credentials、
tool/call identity和arbitrary source都不得跨过公开fault boundary。Component、handler、tool、parser与observer
panic不进入structured fault boundary，原payload直接沿caller stack unwind。
`ReactionAdmissionReason`和`ToolOutputStagingReason`是closed、payload-free的子分类；具体variant在Phase 9
curated public export前保持crate-private，不携带任意字符串或底层source。

`declare()` 是幂等 snapshot read，字段完全相同的重复调用不会使已经 prepare 的 Frame 失效。
identity、epoch或 accepted revision变化会使旧 Frame得到确定未交付的
`SubmitFault::ContinuityChanged`；profile在同一 Application mount内改变则 fail closed。这个 contract
不把 declaration 调用本身当成 lease或 supersede操作。

Built-in Responses和Chat adapters必须原生实现`ReactionPort`；Skill和Plugin实现同一contract，不需要伪装
成Provider。旧`ProviderPort::execute(RenderedProjection)`只作为migration compatibility API保留：新API
public release时进入deprecated、default-enabled `legacy-provider-port` feature，窗口恰好一个后续minor
release，然后删除。compatibility path不作为新correctness tests或built-in实现的adapter，也不能继续
拥有新shared canonical history和semantic diff。
同一built-in provider实例不能交替使用legacy与Frame-native mode；mode只在真实handoff crossing poll
claim，确定的pre-handoff local failure不claim，claim后另一入口typed fail closed。

### 3.1 Frame 里没有什么

public `Frame` 不包含：

- complete `RenderedProjection`；
- complete canonical checkpoint；
- Component Signal 或 closures；
- reaction bindings；
- diff candidate 或 commit receipt；
- Provider response id、wire history 或 compaction state。

这些内容中，前五项由 private `PreparedFrame` 保持 generation-exact；Provider-specific 内容由
具体 port adapter 保持。`DeltaFrom` port 只能看到 exact delta/tail，不能绕过协商读取完整状态。

`FrameSubmission` 的语义是：

- `Full`：本次 policy 选定的完整 canonical replay view、全部必须 handoff 的 staged ToolOutputs、
  shared reconciliation 后的 self-contained Component segment；replay/staged 已经表示的 occurrence不会
  在Component section重复；完整projection中所有System fragments按node/item render顺序合并成零或一个
  snapshot并固定放在Component section首位，缺失表示clear；raw complete projection仍只保存在private
  checkpoint；
- `DeltaFrom(base)`：base 之后的 canonical tail、全部必须 handoff 的 staged ToolOutputs、相对 base
  lower 后的 `#[diff]` operations；它不携带System，表示保留accepted snapshot；
- 两种 submission 都是一个不可变、可直接编码的 ordered value，不允许 target 再查询 Component
  或 private FrameSession 来补材料。

System不是ordinary append item：所有`Instruction(System, pom)`的top-level children按render顺序拼接，
不是“最后一个item获胜”也不dedup；零children归一为`None`。initial、change、clear都强制Full，只有与
checkpoint snapshot相等时才允许Delta。System不进入canonical replay、ordinary occurrence ledger或
`#[diff]`。snapshot和Frame checkpoint只在successful handoff后同步commit，port不得从隐藏history猜测
replacement。

### 3.2 Stable canonical byte meter

两个public byte limit都使用versioned framework meter，不使用Provider tokenizer或任意
`serde_json::to_vec`实现细节。v1 meter是RFC 8785 JSON Canonicalization Scheme（JCS）编码的以下DTO；
UTF-8 byte length就是计量值，object key由JCS排序，array保持canonical causal order：

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
  staged_inputs: ordered ToolOutputs and other mandatory inputs,
  component: ComponentEnvelopeV1,
}
```

`ProjectionSubmissionV1.items`按Component render顺序扁平化ordinary items。Full在首位放零或一个
normalized System snapshot，然后放shared reconciliation后的self-contained ordinary Component segment；
`replay + staged_inputs + component`整体不依赖旧delivery checkpoint，但已经由replay/staged表示的
provider-output occurrence不会重复编码。raw complete projection保留在private checkpoint；
`max_component_bytes`计量normalized System snapshot加完整ordinary projection和ToolCatalog。Delta只放
compiler已经lower完成的full/delta ordinary items且不携带System，omit不编码，node identity和diff
address不离开private checkpoint；port不得再次semantic dedup或System replacement inference。
`CanonicalInputItemV1`沿用canonical transcript的versioned tagged encoding；sealed
`AssistantText`省略默认status，interrupted text显式编码`status: "interrupted"`。ToolCatalog按name的
JCS string comparator升序排列，重复name非法；这个数组顺序属于canonical meter。

`max_component_bytes`计算Full `ComponentEnvelopeV1`，包括它自己的version、projection/tool字段名、容器和
ToolCatalog；`max_frame_bytes`计算完整`FrameSubmissionV1`，包括version、三个section的字段名和全部容器。
`TargetIdentity`、epoch、revision、prepared-against和basis是fixed-size control metadata，不进入semantic
payload meter；它们仍通过structured Frame交给port。`FrameSubmission::canonical_bytes()`返回上述exact
JCS bytes，不能按Provider实现改变。

Full reserve是可执行公式。令`empty`为使用actual CompleteTranscript replay和actual staged inputs、但
`component: null`的JCS submission：

```text
full_non_component_bytes = len(JCS(empty)) - len("null")
required_full_bytes = full_non_component_bytes + max_component_bytes
required_full_bytes <= max_frame_bytes
```

mount先对空transcript/staging验证一次。此后每次canonical fact或ToolOutput admission都用候选完整
transcript/staging重新验证；Delta admission也验证对应hypothetical Full，而不只检查较小的Delta。实际
prepare还要求`len(FrameSubmission::canonical_bytes()) <= max_frame_bytes`。任何加减都checked，profile中
零值、`max_component_bytes > max_frame_bytes`或初始reserve失败都使mount返回typed invalid-profile fault。

## 4. Frame 与 rendering

### 4.1 RenderedProjection 始终完整

Component reconcile 始终产生完整 `RenderedProjection`。clean subtree 可以复用 retained fragment，
但这个优化不能改变输出边界。`#[diff]` 在 projection 中仍然只是 address/template metadata；真正的
full、delta 或 omit 由 Frame compiler 根据 private delivery cursor 决定。

`FrameContext` 若进入 Component authoring，只包含 mount-stable `FrameProfile`：

```rust
let frame = use_frame_context();
let component_budget = frame.max_component_bytes();
let context_window = frame.context_window_tokens();
```

它不暴露：

- 当前 Frame 是 `Full` 还是 `DeltaFrom`；
- 当前 port 接受哪个 revision；
- Provider response id 或 compaction decision；
- 动态的“还剩多少 wire bytes”。

完整 projection 和 Component 声明的 `ToolCatalog` 必须始终适配 `max_component_bytes`。这是 mount-stable
authoring envelope，不是动态的“总额度减去当前 history”。完整 `FrameSubmission` 的 stable canonical
encoding必须适配 `max_frame_bytes`；HistoryPolicy 只能选择一个为完整 Component envelope保留空间的合法
replay view，不能把不断增长的 history成本转嫁成逐轮缩小的 Component budget。

这两个 canonical hard limit形成闭包：`max_component_bytes <= max_frame_bytes`；每次 partial、ToolCall
和 ToolOutput admission都必须验证 interrupted/next-Full 的合法 replay仍能在保留完整 Component
envelope后编码。无法闭合的 causal fact或业务 ToolOutput产生 typed terminal fault，不能先发布再发现
下一次 Full无解。Frame prepare最后仍对 exact Full或Delta做总量检查。

token 数由 port 提供为估算提示；Provider port 在 real handoff 前仍对自己的 private causal artifact、
真实 tokenizer和wire body做第三层校验。三层都不静默 truncate，wire-limit failure必须是确定未交付的
pre-handoff fault。

### 4.2 Bootstrap、latest 与 dirty

`Application::mount` 先调用一次无副作用、可重复的 `P::declare()` 取得 mount-stable `FrameProfile`，
随后执行不创建 Frame、不调用 `submit()` 的 bootstrap reconcile。否则 root 尚未 mount，
Component-owned future/coroutine无法启动，bare Skill latest也没有 rendering。重复 `declare()` 只会
读取当前 snapshot；字段相同则完全幂等。它从不代表 delivery，也不能推进 port continuity或单纯因为
调用次数使已 prepare Frame失效。

`current_projection()` 的 contract 改为保留最后一次成功 commit，即使 Signal 已 dirty。read API 同时
返回 projection revision 和 dirty bit，使 caller 能区分“最新 committed”与“当前 state 已等待
reconcile”。读取不会自行 reconcile 或 react。

正常 reaction 结束前必须再次 reconcile，把 handlers 和 ToolCall lanes 产生的 Signal 更新提交为
新的 complete DOM。fault或handoff前 cancellation 可以保留 dirty state和上一版 committed rendering；
下一次 reconcile/react 会先处理 dirty state。handoff后 drop `react()` future保留已commit Frame、已接纳facts、
已完成ToolOutputs、Component写入和外部副作用，以`Interrupted`保留open text，并为每个unresolved admitted
ToolCall物化预留的`Tool execution was cancelled; its outcome is unknown.` ToolOutput；Application保持ready，
只有下一次显式`react()`才继续运行。

## 5. 一次 reaction 的精确顺序

```text
1. `&mut Application<P>` establishes single-flight
2. `P.declare()` -> TargetIdentity + TargetContinuity + FrameProfile
3. validate mount-stable identity/profile and monotonic target epoch
4. reconcile Component -> complete RenderedProjection + exact bindings
5. FrameSession.prepare(declaration, projection, staged ToolOutputs)
     -> reconcile canonical history
     -> lower #[diff]
     -> validate hard canonical budget
     -> build non-cloneable PreparedFrame
6. P.submit(public Frame)
7. on Ok(ProviderFactStream), synchronously and infallibly commit
     -> canonical outbound submission
     -> FrameRevision/head checkpoint
     -> complete #[diff] baseline
     -> exact staged ToolOutput receipt
8. run one ordered fact/lane pump
     -> poll ProviderFactStream and active ToolCall lanes concurrently
     -> for each fact: validate -> precompute the optional admitted root ProviderEvent
        -> commit canonical history -> dispatch event
     -> a committed ToolCall starts its lane in the same pump iteration
9. after valid provider terminal + EOF, drain already-started ToolCall lanes
     -> stage ToolOutputs by ToolCall ordinal
10. finalize streaming parsers and dispatch EOF diagnostics
11. post-reaction Component reconcile
12. release gate and return
```

Frame prepare 不推进任何 cursor。只有 `submit()` 返回 `Ok(stream)` 后的同步 commit 才推进。
post-submit HTTP、SSE、timeout、handler 或 terminal fault，不回滚已 handoff input，也不回滚已经对
Component 可见的 facts。

### 5.1 Full 与 DeltaFrom

v1 只保留current head checkpoint。checkpoint私下保存complete projection、normalized System snapshot、
diff baseline、exact policy-selected replay view identity以及验证append compatibility所需的materialized
basis：

- port 声明 `FullRequired { epoch }`：`Full`；
- `DeltaFrom(head)` 必须同时满足：同 TargetIdentity、FrameSession namespace和epoch；accepted revision
  正好等于 current head；retained checkpoint/diff baseline存在；支持 semantic delta；mount/execution
  scope兼容；新的 selected replay view可表达为旧 view的严格 append extension；staged ToolOutputs能闭合
  base/tail中表示的 calls；
- revision 属于旧 FrameSession、已淘汰 checkpoint 或未知 namespace：回退 `Full`；同一identity的epoch
  倒退或复用仍是protocol fault，不进入fallback；
- initial System snapshot、snapshot内容变化或clear都强制Full；snapshot相等才继续检查其他Delta条件。
  Full的Component section至多一个normalized System item，Delta不携带System；
- v1 CompleteTranscript不会产生replacement；若内部观察到non-append view，在没有origin/provenance
  grammar时typed fail closed。未来HistoryPolicy先引入该grammar后，删除前缀、替换summary或改变window
  basis才以`Full`交付；matching head只是Delta的必要条件，不是充分条件；
- mount/execution scope变化清空authored occurrence ledger和`#[diff]` baseline；append-compatible replay中
  旧scope provider outputs以及checkpoint之后尚未reconcile的replay tail/staged inputs进入持久的
  ambiguous multiset。同scope仍按canonical value/count先claim node ledger、再claim本scope provider
  occurrence；普通item若只剩跨scope ambiguous match，typed terminal
  `AmbiguousProjectionProvenance`，不能静默omit或重复提交。`#[diff]` append-forced item绕过所有claim；
- Full fallback 再次检查 `max_component_bytes` reserve和 exact `max_frame_bytes`，失败则在 submit 前 fault；
- port 收到 Full 后必须 rebase 与旧 revision 绑定的 private transport/session continuation。

`FrameRevision` 是 delivery baseline token，不是 `TranscriptRevision`。两个 transcript 可以有相同 item
count 但不同 projection 内容，因此不能用 transcript 长度替代 Frame revision。

### 5.2 Continuity race 与 Provider compaction

- provider-private compaction 保持语义 continuity：声明同一 epoch 的 `Accepted(head)`，仍可提交
  `DeltaFrom(head)`；未来provenance-bearing canonical HistoryPolicy改写 replay view后必须Full，v1
  non-append view则fail closed，这些是不同状态；
- port 已丢失 remote/wire continuity：推进 `TargetEpoch` 并声明 `FullRequired`；
- target identity改变：当前 Application terminal fault，不能把旧 canonical history Full到新 target；
- identity相同但 epoch倒退或复用：protocol fault；
- 每次成功校验declaration都同步、不可回滚地推进`highest_observed_epoch`；即使后续发生retryable
  pre-handoff rejection，下一次较低epoch也必须在render/submit前fail closed。committed delivery epoch只在
  successful handoff推进，两者是不同水位；
- `declare()` 后、`submit()` 前 continuity 改变：`submit()` 返回 `SubmitFault::ContinuityChanged`，
  且必须保证 Frame 尚未交付；Runtime丢弃 `PreparedFrame`，重新 declare并用同一 complete
  projection prepare `Full`。v1只自动重试一次，再次变化返回 typed unstable-continuity fault；
- `submit()` 已返回 `Ok(stream)` 后才发现 context 失效：已提交 Frame 不 rollback、不在同一次
  `react()` 自动重发；stream返回 fault，port推进 epoch，下一次显式 `react()` 生成新的 Full Frame；
- ambiguous transport delivery被视为已经跨过 boundary：同一个 submit poll返回 `Ready(Ok(stream))`，
  随后的 stream yield fault；不能先返回 `Pending` 或伪装成 pre-submit `Err`。

### 5.3 External 的 delayed act

structured submit 允许返回的 fact stream 在 `react()` 生命周期内长期 pending，但不允许 Frame
脱离该 structured reaction 后再提交。

External port 的 handoff boundary 是 Frame 被不可撤回地交给它拥有的 outbound transport/queue，而不是
异步 receiver之后某一刻才执行的“业务 callback 已读”。实现可以在 handoff 前异步 reserve capacity；
取得 permit后必须在同一个 submit poll同步 send并返回 `Ready(Ok(stream))`。queue acceptance以后 caller
断开、读取失败或 act超时都是 stream fault，不回滚已经 commit 的 Frame。

返回的 stream可以等待 cloneable control handle稍后注入的 `act`。每个 accepted Frame在 External
adapter 内创建一个 reaction-local ingress generation；对外 protocol correlation必须绑定该 generation。
stream结束或被 drop时关闭 generation，late act/plugin message明确返回 stale，不得进入下一次 reaction。
这个 correlation属于具体 adapter，不恢复 public core `FrameId`。

通用 `ReactionPort` 不提供 out-of-band same-revision Full resync；continuity丢失后由下一次显式
`react()` 生成新的 Full Frame。

## 6. 唯一的 Provider fact stream

当前 `ProviderEvent::Text(TextTurnEvent)` 缺少 output identity、phase、seal 和 abort lifecycle，无法让
shared history 自己可靠提交。因此 `ReactionPort::submit()` 返回的 stream item需要提升为 causal fact：

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

`ProviderOutputKey` 是 reaction-local、provider-neutral lifecycle identity。Responses adapter把output index
映射成key；Chat adapter为它的单一text lifecycle分配key 0。ProviderFactStream yield顺序同时定义
canonical publication顺序：一个key的first fact建立其output position，后续交错facts保持该position；
wire completion乱序时adapter必须先buffer/reorder再yield。Provider-private reasoning/compaction不进入这个
public fact enum，仍由Provider wire session私下按原顺序保存。

映射规则：

| Fact | Canonical history | Component event |
|---|---|---|
| `TextDelta` | append partial | `TextDelta` |
| `TextSealed` | validate + seal accumulated text | none |
| `ToolCall` | register completed call + ordinal | `ToolCall` |
| `ReactionCompleted { primary_text: Some(key) }` | validate terminal lifecycle and sealed key | derive `TextComplete` from sealed text |
| `ReactionCompleted { primary_text: None }` | validate no-primary-text terminal lifecycle | none |

没有第二条 effect stream，也没有 `event + effect` 双 payload。一个 fact 最多产生一个 Component event，
避免一个 handler fault 以后 history 已经包含尚未 dispatch 的多个 visible events。

terminal/ordering grammar在shared admission层固定：

- 一个`ProviderOutputKey`标识一个output lifecycle，不是只能出现一次fact；第一次fact固定其kind和
  canonical position；同一key可以有多个TextDelta和一个TextSealed，但不能在text与ToolCall之间复用；
- 同一text key的`AssistantPhase`在所有delta/seal中必须exact相同；phase drift是protocol fault；
- TextDelta只能进入尚未seal的text output；TextSealed恰好一次并校验accumulated text；没有delta时
  TextSealed可以作为first fact并直接建立sealed text；
- 每个ToolCall key只出现一次；`ordinal`在ToolCall facts中唯一、严格递增，允许因private/non-tool output
  留下gap；ToolOutput staging只使用这个相对顺序，不把ordinal当数组下标；
- v1最多有一个non-commentary text lifecycle；若存在，`primary_text`必须`Some`并引用它；如果不存在，
  `primary_text`必须`None`，允许tool-only或empty completion且不伪造空文本；
- `ReactionCompleted`恰好一次且是最后一个fact；之后继续yield、重复completion、引用未知或未seal output、
  仍有open text以及没有terminal marker的normal EOF都是protocol fault。

异常 EOF、stream fault 或 reaction future drop 时无法依赖 adapter 再 yield `Abort`。Reaction Runtime
必须持有RAII history guard并独占已经验证的canonical transaction buffer：每个可见TextDelta发布前，
buffer中对应item已经是显式`AssistantTextStatus::Interrupted`；normal seal只原位改成`Sealed`。normal
completion或Drop只做不可失败、无重新验证的buffer归还，secondary admission ledger损坏也不能丢弃已发布
tail。Provider wire guard和canonical guard必须只处理已经yield的同一批facts；port lowering必须编码
中断边界或fail closed。

ToolOutput 不是 ProviderFact。ToolCall lane 把 typed ToolOutput 直接交给 private FrameSession，后者按
ToolCall ordinal staging，并在下一次 Frame handoff 才把它作为 outbound canonical input commit。

## 7. 三种 use case

| Use case | 谁决定 react | `ReactionPort::submit` boundary | facts 从哪里来 | 长期 owner |
|---|---|---|---|---|
| Autonomous Agent | agent driver：policy、timer、request、Component demand | model Provider transport 接受 request | Provider stream | agent driver + one `Application<ProviderReactionPort>` |
| Skill reaction | explicit skill invocation | owned outbound queue不可撤回地接受 observation | reaction-fenced control handle注入的 act/protocol stream | one `Application<SkillPort>` per logical skill session |
| Plugin | parent-agent invocation | plugin protocol 接受 Frame | parent/plugin event stream | outer registry中 one Application per parent session |

三者共享 `Application<P>::react()`、private `FrameSession`、Frame compiler、canonical fact admission 和
Component dispatch。它们不共享一个万能 high-level loop，也不要求 Skill/Plugin 实现
Provider-specific API。Plugin multiplexing发生在多个 Application的外部 registry，不允许一个 port
切换 logical parent identity并复用同一份 history。

Skill 的普通 frontend 与 reaction exchange 是两条不同路径：

- `latest` 只读取 `current_projection()`，不 render、不 submit；
- Component-defined typed subcommand只更新业务 state，并按需要发出 driver demand；它本身不伪装成
  Provider fact；
- 只有明确需要一次 external reaction 的 invocation才调用 `Application<SkillPort>::react()`，随后通过
  reaction-fenced act产生 facts。

Plugin 的 parent message同样必须绑定 active adapter ingress generation；late message不能越过 reaction
或 parent-session边界。多 parent registry只管理多个独立 Application，不共享 mutable FrameSession。

Component-to-driver demand 是 autonomous use case 的必要能力，但不是 loop policy。最小 contract 是：

- request 在当前 reaction 期间发出时，至少对一个更晚 turn 保持 sticky；
- 多个尚未满足的 request 可以 coalesce；
- handle 受 mount generation fence；
- 先写 Signal、再 request，下一 Frame 必须包含该 state；
- 它只表达 demand，不表达 Continue/Sleep/Stop，也不能重入当前 reaction。

在这个 contract 有实现和 no-lost-request tests 前，不宣称 high-level autonomous entry或 Agent example
feature-complete。kernel可以先用 internal mount-fenced demand handle测试；具体 Component hook名字在 Frame
core稳定后冻结，但不能把 demand capability排到 public examples之后才验证。

## 8. 对旧方案的裁决

| 旧方案 | 裁决 | 原因 |
|---|---|---|
| public `Frame` | 恢复 | 三种 target 需要共同的 target-ready handoff value |
| public `FrameId` | 拒绝 | structured call 已绑定 dispatch；不需要 authority/correlation id |
| public `FrameRevision` | 恢复 | Full/Delta recovery 需要 opaque target-session baseline token |
| `Application<P>::react()` | 保留 | 固定 port 的 application orchestration和 single-flight入口 |
| public `ReactionHandle` | 拒绝 | 与 `&mut Application<P>` 重复 ownership和 single-flight职责 |
| public `Reactor` trait | 拒绝 | 外部 driver 是 scheduling role，不是统一协议或 framework type |
| public Frame scheduling stream/EventLoop | 拒绝 | Frame 不应自己拥有何时 react 的 policy |
| framework-owned `AgentLoop` | 拒绝 | Agent、Skill、Plugin 的等待/触发语义不同 |
| `use_loop()` + Continue/Sleep/Stop | 拒绝 | Component 不再控制通用 host progression |
| framework-wide sticky wake epoch | 拒绝 | 只保留 adapter-owned、mount-fenced reaction demand |
| ProviderPort owns canonical history/diff | 拒绝 | 造成三种 use case 复制 state machine，也是当前职责膨胀来源 |
| ReactionPort owns private continuation | 保留 | Provider、Skill、Plugin各自的transport/protocol state不进入 shared session |
| generic Provider middleware owns state | 拒绝 | 看不到 pre-render declaration，也不能证明 real handoff；以后只做 tracing/retry/metrics decorator |
| public mutable `FrameSession` | 拒绝 | 会暴露 commit、baseline 和 history mutation 权限 |
| private `FrameSession` | 恢复 | shared history/diff/receipt 需要一个原子 transaction owner |
| `HistoryEffect` | 拒绝 | 唯一 ordered ProviderFact stream 直接驱动 history commit |
| external `observe / act` as core API | 拒绝 | 只保留 Skill/CLI compatibility adapter，不污染 Reaction kernel |
| declarative provider event hook | 保留、后置 | `use_provider_event_handler` 仍是目标 authoring surface |
| generic `use_hook<T>` | 延期 | 先实现共享 hook topology，不先公开万能 hook |
| `use_task()` | 拒绝该名字 | 用 Dioxus-like lifecycle categories 取代一个含糊 task API |
| `spawn` | 保留、后置 | committed callback 内的一次性 mount-scoped task |
| `use_future` | 保留、后置 | mount commit 后启动，unmount 时 abort + await |
| `use_coroutine` | 保留、后置 | bounded typed inbox，适合 Component-owned WebSocket |
| `use_resource` / `use_action` | 延期 | reactive tracking 和重复 action policy 尚未定义 |
| Signal dirty 自动 react | 拒绝 | dirty 只表示 DOM 需要 reconcile |
| Component-to-driver notification | 恢复 contract、后置 API | autonomous agent 需要，但不能恢复 universal loop policy |

## 9. 分阶段实施

### Phase 0：冻结协议，不改行为

产物：

- 将本文的 ownership、fixed-port fence、head-only checkpoint 和 submit 语义回写 `engine.md`，整体删除
  被否决的 AgentLoop/`use_loop` 权威语义；
- 冻结 `ProviderOutputKey`、`ProviderFact` terminal grammar、ordering 和 optional root event projection；
- 冻结JCS `ComponentEnvelopeV1`/`FrameSubmissionV1` byte meter、`max_component_bytes`/`max_frame_bytes`
  计量范围、exact Full reserve和admission closure；
- 冻结v1 canonical view为CompleteTranscript、无shared semantic compaction，以及open ToolCall closure；
- 冻结 `Application<P>` / `ReactionPort::declare` / `ReactionPort::submit` 的 borrow、重复 declare、
  target identity、handoff poll linearization和 cancellation contract；
- 区分 canonical、target delivery、required private causal和 disposable private state，并冻结 loss/recovery；
- 冻结旧`ProviderPort`为一个minor release的deprecated `legacy-provider-port` compatibility window；
  legacy adapter不用于证明新pipeline correctness。

Exit gate：没有 unresolved ownership、commit point 或 budget unit；不开始 hook/task 工作。

### Phase 1：Protocol 与 owner skeleton

改动：

- 引入 `TargetIdentity`、`TargetEpoch`、`TargetContinuity`、`FrameProfile`、`FrameRevision`、`Frame`、
  `ProviderFact` 和 `ReactionPort` 的最小类型；
- 在`component::execution::reaction`实验模块公开protocol types，允许真实下游crate验证trait/lifetime；
  `Application<P>`在完整pipeline存在前保持crate-private，Phase 9再进入curated public API；
- 引入持有 fixed port、Component runtime和 crate-private `FrameSession` shell 的 `Application<P>` owner；
- `FrameSession` shell先固定 `CanonicalHistoryState` 与 `TargetDeliveryState` 的所有权，不提前实现 compiler；
- compile-only API tests验证experimental public path、borrowed fact stream lifetime和move-only Frame；
- 提前引入一个只表达`submit -> synchronous commit`的private最小handoff transaction harness；fake port逐
  poll测试drop Pending为零handoff/零commit，crossing poll直接Ready(Ok)并在同一outer poll完成commit；
  完整PreparedFrame receipt/compiler在Phase 4替换这个shell，完整reaction integration仍在Phase 5；

Exit gate：后续 phase依赖的 owner和协议类型已经存在，但 built-in provider行为尚未迁移。

### Phase 2：Component rendering lifecycle

改动：

- `ComponentHost::current_projection()` 在 dirty 时保留上一 successful commit；
- 增加 projection revision + dirty metadata；
- 增加只调用无副作用 `declare()`、不调用 `submit()` 的 bootstrap reconcile；
- 把当前 `ApplicationHost` 的 render/bindings prepare 与 stream dispatch 拆开，为 public
  `Application<P>` ownership做准备；
- 正常 reaction 后执行 reconcile；fault/cancel 保留 dirty + previous commit。

主要锚点：`src/component/host.rs:93-147`、
`src/component/execution/application_host.rs:139-271`。

### Phase 3：ProviderFact 与 shared canonical admission

改动：

- 用有 output identity/phase/seal 的 `ProviderFactStream` 替换 port-returned `ProviderEventStream`；
- Component-facing `ProviderEvent` 作为 fact 的 infallible projection继续保留；
- 引入 canonical partial/seal/abort guard；
- 把 `ToolOutputSink` 从 `ToolCall`/Provider 移入 Phase 1 已存在的 private session-owned lane staging；
- 保证 validate -> precompute event -> commit -> dispatch 顺序；
- Phase 3可以先以private state machine冻结grammar；在Phase 4 exact interrupted/next-Full budget meter完成前，
  不接入production reaction pipeline。

主要锚点：`src/component/execution/port.rs:322-457`、
`src/provider/async_openai.rs:730-802`、`:1293-1375`。

### Phase 4：Shared Frame compiler 与 FrameSession transaction

改动：

- 将 `ProjectionDiffState` 从 provider module 移到 Frame compiler；
- 合并 Responses/Chat 重复的 projection reconciliation；
- 形成一个同时理解 append policy 和 diff template 的 `ReconciledProjectionPlan`，避免机械交换
  “diff first / history first” 顺序；
- 将provider occurrence ledger按execution scope分区；旧scope和scope切换前未reconcile tail持久标记为
  ambiguous，普通value-only跨scope collision在Frame prepare阶段typed fail closed；
- 填充 private `FrameSession`，引入 `PreparedFrame`、TargetIdentity/epoch fence和head checkpoint；
- checkpoint保存 exact selected replay-view basis；只有 append-compatible view才允许 Delta；
- 实现CompleteTranscript policy、JCS meter、Component reserve、total Frame hard limit和partial/ToolOutput
  admission closure；
- prepare 只产 candidate；commit 同步、不可失败，并消费 exact ToolOutput receipt。

主要锚点：`src/provider/projection_diff.rs:14-263`、
`src/provider/async_openai/continuation.rs:114-255`、
`src/provider/async_openai/chat_completions/history.rs:51-138`。

### Phase 5：新的 structured reaction pipeline

改动：

- 实现 declare -> reconcile -> prepare -> submit -> commit -> fact/lane pump -> post-reconcile；
- `Frame`/bindings/render generation 作为一个 private transaction；
- identity变化 fail closed；epoch、accepted revision或replay-view basis不匹配时 Full fallback；
- ToolCall canonical commit后立即启动 lane，fact stream与active lanes并发 poll；
- Full fallback重新校验 Component reserve和exact total Frame hard limit；
- external delayed fact stream可以在 successful submit 后保持 pending；
- pre-submit continuity race只允许一次自动 redeclare/reprepare，之后 fail closed。

Exit gate：所有 pre-submit failure 不推进 state；所有 post-submit failure 不回滚 state。

### Phase 6：原生迁移 built-in targets

顺序：

1. Responses ReactionPort：保留 first transport poll handoff、SSE/output ledger、private continuation；
   区分 required private causal artifact与disposable state，移除 shared canonical history/diff ownership。
2. Chat Completions ReactionPort：复用同一 Frame compiler/fact admission，只保留 Chat wire encoding。
3. Debug ReactionPort：只读取 exact Frame submission，不伪装 retained Provider history。

当前进度：Responses、Chat Completions和Debug均已完成native `ReactionPort`与conformance。Chat v1明确
为text-only：非空ToolCatalog在handoff前typed reject，上游tool call terminal reject；这项target-specific
限制不能被描述成公共`ReactionPort`不支持工具。

不使用 legacy adapter 来证明 correctness；旧 Text/ToolCall stream 无法表达 seal/abort identity，
只能作为短期 best-effort compatibility path。

### Phase 7：Agent / External / Skill / Plugin integrations

改动：

- External channel 发送 `Frame`，不再发送 rendered projection string；
- 删除独立 string generation/diff baseline；
- 先异步 reserve、再在 crossing poll同步 queue send；queue acceptance成为 `submit()` success proof；
- `act` 产生 ordered ProviderFacts，并由 adapter ingress generation拒绝 late act；
- port移入 `Application` 前可以创建 cloneable control handle，用于注入 act/plugin messages但不能提交
  Frame或修改 history；
- Skill `latest`/typed subcommand frontend与 Skill reaction exchange分开测试；
- 增加 internal mount-fenced Component-to-driver demand seam和 no-lost-demand tests，供 Agent driver等待；
- continuity丢失后只在下一次显式 `react()` 生成新 Full，不提供通用 same-revision out-of-band resync；
- 分别增加最小 Skill 和 Plugin port；Plugin多 parent由外部 registry持有多个 Application，证明三种
  use case没有复制 history/diff。

当前进度：Phase 7 已完成并通过独立review。External原生实现`ReactionPort`，outbound queue传递exact
move-only `Frame`；control liveness、ingress generation、continuity reset和nonterminal EOF fault均在
External边界闭合（`external.rs:176`, `:211`, `:494`, `:634`）。internal driver demand保持sticky、
coalescing和mount fence（`driver_demand.rs:26`, `:51`）；Skill与Plugin只是在同一queued exchange上的薄角色，
不拥有history、diff或session（`integration.rs:21`, `:71`）。Agent、Skill、Plugin canonical golden、Skill
frontend separation和Plugin multi-parent registry分别由`integration/tests.rs:80`、`:131`、`:180`覆盖。
`Application`和public Component demand hook仍按原计划留在Phase 8/9，不因本阶段closure提前公开。

### Phase 8：Component authoring 与 async lifecycle

依赖 Phase 1-7 的 mount/reconcile和driver-demand contract：

1. generalized `(HookSite, HookKind)` topology 和 atomic render transaction；
2. `use_provider_event_handler`；
3. `use_reaction_request() -> ReactionRequest`，只用`.request()`发出mount-fenced Component-to-driver
   reaction demand；
4. mount task registry、supervisor、abort + await；
5. `spawn`、`use_future`、bounded `use_coroutine`；
6. 最后再评估 `use_resource`、`use_action`。

这一阶段不重新引入 `use_loop`。WebSocket 若是 Provider/Plugin transport，仍属于 ReactionPort实现；
只有 Component business sidecar 才使用 `use_coroutine`。

当前实现已经覆盖上述1-5项：所有hook共享lexical `(HookSite, HookKind)` topology；provider handler和
reaction demand都是mount-fenced capability；Application-owned supervisor负责`spawn`、`use_future`和bounded
`use_coroutine`。bootstrap与后续mount均在projection/topology commit后立即注册new-mount task；同一mount的
rerender不重启task。unmount先fence capability，再abort并await全部owned task，之后才发布replacement
projection。

v1不实现user panic recovery：Component root/render、provider/legacy event handler、native tool、streaming decoder、
engine observer和usage observer外层都没有`catch_unwind`；panic不转typed fault、不吞掉。render candidate依靠RAII
保持commit-before-publish，但不承诺回滚任意user side effect或live shared state。hook topology/availability违规
同样panic并直接unwind，candidate不publish。正常`Result::Err`仍保留typed fail-closed语义。
unwind期间Runtime不再次进入user callback或observer；panic路径不保证terminal/cleanup observation，原payload
优先传播。

如果caller在Runtime外层自行`catch_unwind`，它必须丢弃该Application以及可能被本次user callback触及的可变
资源；v1不提供隔离副本、transactional rollback或poisoned-state recovery。未发布candidate的RAII销毁不能证明
共享`Arc`、live Signal或其他外部side effect已经回滚。

Component task future也不被framework显式catch；Tokio不可避免地用`JoinError`跨task boundary传递panic。
supervisor取回第一项原payload，并在当前/下一可用outer driver boundary直接`resume_unwind`；sibling abort立即发起，
但drain不能成为unwind前置条件。unwind前Application进入terminal，caller若主动catch，后续API确定fail closed。
caller随后consuming shutdown时仍join已经abort的tasks并等待destructor，再返回terminal fault；该cleanup不阻塞
最初的unwind。
`react()`、driver demand wait和nonblocking demand take在返回既有terminal fault前都再次检查同一个panic monitor；
因此已经锁存且尚未传播的task panic会抑制post-handoff cancellation fallback recovery并以原payload优先传播；
已经传播的payload只对应一次unwind，后续调用不重新declare/render/submit，只返回task-panic terminal或执行
consuming shutdown。
Drop catch只用于正在销毁资源的primary-payload仲裁。workspace与下游均保留Rust `panic = "unwind"`语义。
本阶段只覆盖frame-driven Component/Application Runtime；legacy `AgentTurnObserver`的post-commit fire-and-forget
语义保持独立，未来统一时需要supervised observer owner，不能从Drop调用callback或丢弃observer task handle。

supervisor第一次启动actor时固定Tokio runtime identity；后续task operation和monitor wait若发生在foreign
runtime，必须在enqueue/Frame handoff前同步关闭supervisor与retirement waiter。actor因owning runtime shutdown
被drop时由RAII finalizer执行同一`Closed` transition。retirement bookkeeping只在`Retire`排队、abort和join期间
保留per-Component临时claim及live waiter，完成后删除。永久stale authority由`MountFence`提供：registration持锁
完成`Start` enqueue，unmount持同一锁invalidate后才enqueue `Retire`；core mutex和actor FIFO保证已接受的`Start`
先于对应`Retire`。因此状态由current/in-flight cardinality约束，不按历史remount或结构ID增长。

Application的`react()`、blocking demand wait与nonblocking demand take共享同一个outer-boundary arbiter。
supervisor为`Closed`时三者都在declare/render/submit或消费sticky demand之前fail closed；fresh task panic仍在既有
terminal和`Closed`分类之前取得优先级。

production External owner提供consuming shutdown：调用时立即把唯一owner交给独立cleanup task，cancel并join
active reaction，恢复Application后再fence、abort + await全部mount task。丢弃shutdown waiter不取消cleanup；
CLI只能在task destructor全部完成后ACK。Agent/Skill/Plugin目前是crate-private port role而不是production
Application owner；Phase 9引入它们的owner时必须使用同一shutdown contract。

Signal write、task completion、coroutine message和Provider EOF都不隐式调用`react()`；Component task需要后续
turn时，必须先写Signal，再通过自己捕获的`ReactionRequest`发出sticky demand。普通task result由Component
future内部处理；`use_resource`/`use_action`仍保持deferred。

FDR-035至FDR-037的implementation candidate与回归已经完成；完整gate重新验证和独立review sign-off仍在
进行，因此本计划不提前关闭Phase 8 gate。

### Phase 9：public API、兼容与 executable examples

改动：

- 公开完整的`Application<P>`，并把Phase 1 experimental reaction module中已经经过下游compile/lifetime
  tests的`ReactionPort`、`TargetIdentity`、`TargetEpoch`、`TargetContinuity`、`FrameProfile`、`Frame`、
  `FrameRevision`和`ProviderFact`提升到curated public path；此时增加no-`port_mut()`外部API gate；
- 新API release时把旧`ProviderPort::execute(RenderedProjection)`放入default-enabled、deprecated
  `legacy-provider-port` feature；下一个minor release删除；
- `ComponentReactionRuntime` 继续作为 low-level explicit-reaction compatibility API；
- 用一个小型 Agent、一个 Skill、一个 Plugin executable example分别验证 scheduling、frontend/exchange
  分离和 stale-ingress fence，而不只是证明三者能实现同一个 trait；
- Chess 在 Frame core 稳定前继续作为 explicit orchestration example，不做机械式半迁移。

## 10. Test gates

### Ordering and transactions

- `declare()` 严格先于 Component reconcile；相同 snapshot重复读取幂等，不推进 state或使 Frame失效；
- third-party port可用public constructor创建稳定TargetIdentity；identity/profile变化和epoch倒退fail closed；
- bootstrap可以调用 declare取得 profile，但绝不调用 submit；
- Frame 与 bindings 必须来自同一 render generation；
- prepare failure 不推进 history、revision、diff baseline 或 ToolOutput receipt；
- 手工 poll submit future：每个 `Pending` 后 drop都是零 handoff/零 commit；crossing poll必须同 poll
  `Ready(Ok(stream))`；`Ready(Err)` 必须确定未交付；
- successful submit 后 commit 无 await、无 failure；
- successful submit后尚未 poll stream就 drop属于 post-handoff，不回滚 commit；
- drop整个`react()` future：handoff前和handoff后Application都保持ready且不自动开始reaction；handoff后
  无fact continuity loss要求下一次显式reaction使用更高epoch Full，open text只replay一次，unresolved admitted
  ToolCall以一个预留`Tool execution was cancelled; its outcome is unknown.` ToolOutput闭合；只有port仍保留
  exact compatible continuation时才允许Delta；
- post-handoff cancellation不会resume或replay已经drop的ordinary/XML/EOF/reaction-completion callback；
- post-handoff cancellation与task panic交错时，已锁存panic抑制fallback recovery，并在`react()`、driver demand
  wait、nonblocking demand take和External边界传播同一个原payload；caller catch后的再次调用仍只返回typed
  task-panic terminal/stale且不发生新handoff；
- post-submit first stream error仍保留 outbound submission和 diff baseline；
- fact 先 commit history，再进入 Component handler；handler returned fault不回滚 fact；handler panic直接unwind；
- root/render、sync/async handler、native tool、streaming decoder和observer panic均由test harness证明原样传播，
  production路径不得出现panic-to-fault或swallow；candidate panic不得publish topology/projection/task factory；
- valid terminal + normal EOF 后 lanes、EOF diagnostics、post-reconcile 全部结束才返回。

### Full/Delta and budgets

- first Frame为 Full；matching head加 append-compatible replay view等全部条件满足才能 DeltaFrom；
- 完整projection中多个System fragment按node/item render顺序合并为一个normalized snapshot；
  initial/change/clear都强制Full，snapshot相同时才可继续Delta eligibility；
- Full的Component section至多一个位于首位的System item，Delta不携带System；canonical history、
  staged ToolOutput、ordinary occurrence reconciliation和`#[diff]`的state都不包含System；
- prepare candidate被丢弃、pre-handoff submit failure或`ContinuityChanged`不推进System snapshot；只有
  successful handoff与revision/checkpoint原子commit，之后的FullRequired必须重述已接受snapshot；
- System normalization的canonical bytes计入`max_component_bytes`和exact `max_frame_bytes`，且不允许
  port根据隐藏history重新推断replacement；
- test-only non-append canonical replay view在缺少occurrence provenance时fail closed；未来
  provenance-bearing replacement即使accepted head匹配也强制Full。v1 production policy只产生
  CompleteTranscript；provider-private compaction在port仍承认head时不强制Full；
- scope reset后相同canonical value的新authored occurrence不得被旧provider occurrence吞掉；无显式origin
  时返回`AmbiguousProjectionProvenance`。覆盖direct remount前的未reconcile tail、重复同值count、成功
  无关Frame后fence仍保留、same-scope claim优先，以及`#[diff]` append item绕过ambiguity；
- 一个 Application不能切换 logical target；identity变化 terminal fault，新 target必须新建 Application且
  不继承 history；epoch倒退/复用是 protocol fault；
- TargetEpoch reset保留 canonical history；Full submit后重置 port-private baseline；
- declare/submit之间 epoch变化只允许确定未交付的 `ContinuityChanged`，自动重试一次；
- 从`Accepted(R1)` prepare的Full携带exact precondition；submit前变成`Accepted(R2)`必须
  `ContinuityChanged`而不是handoff；
- post-submit context loss不自动重发，下一次显式 react产生新 Full；
- required private causal artifact丢失只有 proven stateless rebuild或 fail closed，不能无条件 Full；
- complete projection永远存在于 private checkpoint，public Delta Frame不泄漏 full snapshot；
- JCS golden vectors固定Component/Frame container overhead、Unicode、object-key和array-order meter；
- mount empty-state reserve、`max_component_bytes` reserve、exact `max_frame_bytes`和Provider wire/token
  limit分别测试；任何一层都不静默truncate；
- TextDelta、ToolCall和ToolOutput admission验证 interrupted/next-Full在 Component reserve下仍可编码。

### Partial output and tools

- TextDelta/TextSealed lifecycle和ReactionCompleted primary output identity可验证；
- text phase drift、text/ToolCall key reuse、duplicate/backward ToolCall ordinal分别fault；ordinal gap合法且不
  作为数组下标；
- tool-only completion、duplicate completion、post-terminal fact、unknown/unsealed primary key、open-text
  completion和normal EOF without terminal分别测试；
- stream error/drop无校验归还已始终记录为interrupted的partial；test-only secondary-ledger corruption也
  不得丢失已release event对应的canonical tail；
- 未 yield 的 Provider-private output不进入 canonical history；sealed required private output按 provider
  ordinal保留，后续 stream fault不回滚；
- ToolCall history commit后在同一 pump iteration启动 lane；stream保持 pending时lane仍能推进；ToolOutput
  按 ordinal staging；
- pending ToolCall lane期间drop post-handoff `react()`会取消lane，并在下一次显式提交的Frame中为该call发送
  exactly one预留`Tool execution was cancelled; its outcome is unknown.` ToolOutput；fallback必须跨失败prepare和
  pre-handoff submit cancellation保留，不能用Full、Delta或新reaction绕过unresolved call，也不能留下stale slot；
- local prepare failure后 exact ToolOutput receipt可重试；submit成功后只消费一次。

### Rendering and integrations

- bootstrap mount只调用 side-effect-free declare、不调用 submit；bare latest有 committed projection；
- dirty 时 latest仍可读并标记 stale；Signal write不自动 react；
- normal reaction post-reconcile使下一 Frame看到 handler state；
- External先 reserve再同步 queue send；queue acceptance让 submit成功，receiver之后失败成为 stream fault；
- act/plugin message必须匹配 active ingress generation；stream结束后的 late message明确 stale；
- External continuity丢失由下一次显式 react产生新 Full；
- Skill latest read和typed subcommand不隐式 render/submit；只有明确 reaction invocation调用 react；
- mount-fenced Component demand在 driver真正等待前后都不丢失，多个 pending demand可以 coalesce；
- Agent、Skill、Plugin golden tests对相同输入使用同一 canonical compiler结果，并各有一个 executable
  lifecycle test。

## 11. 不在本计划中偷带的内容

- public mutable history/session API；
- concurrent multi-target writers；
- multi-checkpoint delta、fork 或 durable session recovery；
- Provider retry middleware拥有 commit state；
- Component-specific scheduler policy；
- automatic reaction on dirty；
- task retry/backoff、durable jobs 或 unbounded coroutine inbox；
- Skill subcommand schema和 Plugin multiplexing protocol的最终语法。

这些可以后续设计，但不能改变本文的 Frame handoff、history-first fact admission 和 private atomic
session boundary。
