# 同一 Application 的异步 Reaction

状态：已实现，与 EventContext 使用同一身份和派发路径。
[engine.md](engine.md) 是权威运行时契约；本文说明异步 handle、历史分支和交互方式。

无需凭据的 [parallel_reactions 示例](../examples/parallel_reactions.rs) 展示同树异步请求、
临时工具续接、定向中断，以及主线通过业务视图观察分支结果。

## 1. 入口与分类

一个 Application 保留同一棵 Component tree、mount 和业务 Signals，允许多个 reaction 的运行时间重叠。
已有请求 streaming 时，之后发生的业务事件可以再次调用 `app.react(false)`；不要求提前收集固定批次。

```rust,ignore
// 先建立一个完成且工具协议闭合的主线 checkpoint。
app.react(false)?.await?;

let a = app.react(false)?; // 当前没有 Main，占用主线执行位。
// 之后的业务事件到来；a 此时仍可能在 streaming。
let b = app.react(true)?; // 显式创建 Temporary，Main 空闲时也保持独立。
let id = b.id();
let kind = b.kind();

b.interrupt()?;
let outcome = b.await?;
let main_outcome = a.await?;
```

`react(ephemeral: bool)` 在同步准入时原子决定分类：`true` 始终创建 `ReactionKind::Temporary`，
即使主线空闲也不取得主 port/session；`false` 保留自动分类，没有占用主线执行位时为
`ReactionKind::Main`，否则为 `ReactionKind::Temporary`。Temporary 固定使用准入时最后一份合法 checkpoint；没有 checkpoint 或
port 不支持独立请求时明确拒绝。首次 Main 可以从空历史开始；它尚未完成时不能把其正在生成的状态
当作 Temporary 的分支点。显式临时请求在没有合法 checkpoint、父缓存过期或不兼容时拒绝，
不回退到 Main。这里的 ephemeral 参数控制整次 reaction 的分支归属，与 POM 输入的 ephemeral 属性独立。

Main 直到必要结算、恢复和清理完成才释放执行位。kind 返回后不变；中断 Main 不把已存在的
Temporary 升格为 Main。已有 Application fork 仍然创建另一棵独立 mounted tree。

## 2. Reaction 就是 handle

```rust,ignore
impl<P: ReactionPort + 'static> Application<P> {
    pub fn react(&mut self, ephemeral: bool) -> Result<Reaction, ApplicationFault>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKind { Main, Temporary }

impl Reaction {
    pub fn id(&self) -> ReactionId;
    pub fn kind(&self) -> ReactionKind;
    pub fn interrupt(&self) -> Result<(), ApplicationFault>;
    pub fn react(&self) -> Result<Reaction, ApplicationFault>;
}

impl std::future::IntoFuture for Reaction {
    type Output = Result<ReactionOutcome, ApplicationFault>;
    type IntoFuture = ReactionWait;
}

pub enum ReactionOutcome {
    Completed,
    Interrupted,
    Exited(ExitReason),
}
```

`Reaction` 可 Clone，副本控制同一次操作。调用方直接使用返回值，不需要先取得裸 ID，
再调用 `app.reaction(id)`；定向中断是 `b.interrupt()`。

`react()` 的同步借用只覆盖准入和启动。Application 拥有实际任务，返回 handle 后就在其 Tokio
runtime 上推进 preparation、提交和事件消费，无需先 await。同步错误是准入错误；普通异步准备、
提交和运行失败由本次等待结果报告。`Completed` 表示 runtime 完成，不替代业务结果。

- 复用 `ReactionId`，准入时生成，早于 preparation 和 provider 提交；没有额外 handle ID。
- `id()`、`kind()` 稳定。clone 不启动请求，每次 IntoFuture 建立独立 waiter。
- 丢弃 handle、clone 或 waiter 不取消已经接纳的工作。
- 普通终态结果保留供多个 waiter 观察；handle 不强持有 Application 或整个运行任务。
- Application shutdown 停止准入、中断并收取全部 owned reaction，然后清理 provider 与 mount 资源。
  `shutdown()` 调用即开始清理，取消其 waiter 不取消清理。

## 3. 定向中断、恢复与 panic

`reaction.interrupt()` 请求中断这一个 ID，不请求 Application exit，也不取消其他 reaction。
终态后的重复 interrupt 幂等。它覆盖排队、preparation、提交前等待和 streaming；返回只说明请求
已接纳，真正结算由等待结果确认。

提交前中断不 handoff、不消费 receipt、不建立 baseline。提交后保留已 handoff 输入、已接纳 facts、
工具结果、Signal 写入和外部效果；open text 和未完成调用按既有取消规则闭合。不回滚、不重放，
也不合成正常 completion callback。

managed effects 未解决时，reaction 保留 worker、恢复 fence 和执行位，waiter 保持 Pending。
应用调用 `app.recover_streaming_attempt().await` 显式恢复，或由 consuming shutdown 继续恢复。
清理完成后才发布终态；若本轮已经产生 `RecoveryRequired` 等 fault，等待仍报告该原始运行 fault，
不会把清理完成改写为业务成功。恢复报告保留每个 attempt 的原 EventContext。

user panic 不转换成普通业务 fault。owned task 边界保留原 payload，停止其他 reaction，令 Application
fail closed；第一个观察边界传播原 payload，后续 waiter/API 返回 terminal fault，不复制或重复传播
panic payload。caller catch 后不得继续业务执行，仍需收尾 Application；shutdown 会先 drain 已拥有的
资源再传播尚未观察的 panic。

## 4. 历史与共享业务状态

| 状态 | Main | Temporary |
| --- | --- | --- |
| 主 canonical history | 按 handoff/admission 规则推进 | 只读分支点，追加到自己的临时 history |
| 主 provider continuation | 按主线请求推进 | 不更新、不重置，不安装分支输出或 compaction |
| 主 staging、receipt、diff baseline | 按主线交付推进 | 不复制、消费或推进，使用自己的协议状态 |
| 分支点 | 主线当前合法续接状态 | 准入时最后一份合法 checkpoint |
| Component tree 和业务 Signals | 同一 mounted runtime | 同一 mounted runtime |

checkpoint 在 Main 正常完成且工具调用闭合、staging 为空时成对保存 canonical history 与 provider
私有上下文。当前 Main 的 pending 调用不会阻止使用更早的合法 checkpoint；其正在变化的 admission
buffer、当前 staging 和未交付结果不进入 Temporary。

Temporary 动作调用本轮准确绑定的 handler，真实 Signal 写入和外部副作用仍然发生。历史隔离不提供
业务状态快照、事务或回滚。每条 reaction 在 preparation/reconcile 后读取当前完整 typed POM、System、
工具定义、ephemeral context 与 bindings；共享业务状态的并发准入仍由业务负责。

并发且历史独立的 sibling reaction 可以使用相同 native call_id、output key，派发按 reaction 归属。
同一条保留的协议 lineage 内，canonical 工具 call_id 仍须唯一；这也包括其继承的 checkpoint prefix，
不承诺通过重用旧 call_id 自动覆写历史或重映射协议项。保留的原生工具记录和 XML 诊断按主线或临时
lineage 选择。Temporary 的记录不会因为处在同一 mounted tree 就自动进入 Main。

## 5. Temporary 的后续交互

一轮 reaction 最多 handoff 一个 Frame。工具结果进入本分支 staging，必须在下一次实际提交中交付，
模型才能据此继续操作。使用已结算的 Temporary handle 发起其下一轮：

```rust,ignore
let branch = app.react(true)?; // 已有合法 checkpoint；Main 可以活跃或空闲。
let first = branch.clone().await?;
// 应用声明实际结果、进度或失败及当前可用动作。
let next = branch.react()?;
let second = next.await?;
```

`branch.react()` 只继续已结算的 Temporary；它使用同一临时 lineage 的 session、工具 receipt 和
provider scratch，返回新的 ReactionId。该 lineage 同时只能有一轮活跃，后续需使用最新返回的 handle，旧 handle 不重复消费已接续的 scratch。
Main 的下一轮由 `app.react(false)` 发起；新的独立 Temporary 由 `app.react(true)` 发起，
每次从主 checkpoint 创建 sibling，不续接先前临时输出。

活跃任务始终由 Application 持有。已结算 Temporary 的 scratch 随可继续它的存活 handle 保留；
不再有相关 handle 时可回收。持有已完成 handle 不保留已完成网络流和工具 lane。

Main 只接收应用显式声明的分支业务结果。Temporary 的 transcript、私有 reasoning 和原生记录不自动
合并进主历史。Signal 写入、handle 完成或 staging 中留有结果都不表示模型已经观察到反馈；下一份
实际提交的 Frame 才完成交付。

## 6. Provider 能力与 Responses

`ReactionPort` 提供可选、object-safe 的同步能力：

```rust,ignore
fn fork_temporary(&mut self) -> Result<Box<dyn ReactionPort>, ReactionPortFault>;
```

默认明确拒绝。支持者创建新 identity、相同 profile、FullRequired 的独立 target，保留精确 canonical
prefix 及必须的私有 artifacts；不发请求、不触发观察回调、不启动 worker、不修改源端。
Application 保留独立的 checkpoint template，Main streaming 借用主 port 时，Temporary 从 template
创建 sibling，互不借用可变 continuation。native Responses 和 Debug port 支持该能力。

普通非缓存 Responses 在分支内独立保留并重放 wire history，沿用 `store:false`。启用
`CodexHttpV1Options::with_ark_caching(true)` 的方舟 adapter 使用已保存父响应：

```text
previous_response_id = checkpoint 对应的、已验证且未过期的父响应 ID
store = false
input = 本分支新增的完整协议项
```

多个 Temporary 可引用同一父 ID。首次 Full 校验 canonical prefix，后续轮次仍引用固定父 ID，
只上传本分支新增的完整输入、reasoning、phase、工具调用与结果。当前临时响应不会建立可持久续接的
新父 ID，也不污染主 port。当前 ephemeral prompt 只加入这一请求，后续 scratch 不重放它。

已保存的父响应过期、prefix proof 损坏、System/工具定义不兼容时明确拒绝，不直接降级为本地父节点。
若 Main 因当前 ephemeral 输入明确发送 `store:false`，且没有远端父节点，则完整 terminal envelope、
output ledger 和 usage 验证后可以封存完整 retained wire 的 Local checkpoint。它冻结完整 prefix、
System、工具定义及 parallel-tool 设置；每个子轮完整上传 retained context，强制 `store:false`，
不发送 caching 或 `previous_response_id`，即使该子轮没有 ephemeral 也一样。当前 ephemeral 仅追加到
当前请求，不进入本地 prefix。准备中、失败、取消、工具协议未闭合或 proof 不匹配的窗口仍拒绝分叉。
Local 不改写 Main 或兄弟分支，也不把未存储响应 ID 冒充远端父节点。
Temporary 禁用会改写固定父 prefix 的后台和 forced compaction。
`response.created` 提前给出的 ID 不表示当前输出已经成为 checkpoint；只有完整成功验证的 terminal
response 才可建立已保存父响应。中断当前请求不使上一份已保存的父 ID 自动失效。

`store` 控制远端当前响应是否存储；Main/Temporary 控制本地历史和交付归属。Main 中断时，已发布的
canonical tail 和取消产生的工具结果仍保留；远端 checkpoint 未推进不表示本地主历史回滚。

参考：[方舟创建 Response](https://www.volcengine.com/docs/82379/1569618)、
[树状分叉与存储规则](https://ark.volcengine.com/region:cn-beijing/docs/responses-api-text-generation)、
[Response 生命周期](https://ark.volcengine.com/region:cn-beijing/docs/response-lifecycle)。
离线协议测试覆盖固定父 ID、非存储分支、sibling 与工具续接；不据此声称完成了真实方舟服务的并发验收。

## 7. EventContext

同一棵树中的 Main 和 Temporary 使用同一 ApplicationId。context 的 `reaction_id()` 对应 handle，
`reaction_kind()` 返回同一个 Main/Temporary 分类；host/command 事件没有虚构的 model reaction 或 kind。
FrameRevision 只绑定实际成功 handoff 的 Frame，continuity retry 不提前固定错误 revision。

每次派发生成 EventId，多播共享该 ID；source/caused_by 保留实际来源和已知派生关系。
异步结果、诊断、发布、取消反馈和恢复一直保留原 EventContext，不读取“当前 reaction”来重写来源。
`use_application_id()` 表示当前 view 的接收应用，不能区分同 App 的不同 reaction。
这些 runtime 字段不进入业务工具 JSON Schema。

每条 reaction 独立拥有 admission、工具 staging、parser、streaming supervisor 和取消状态。
preparation/reconcile 在共享 tree 上仲裁，网络流和工具 lane 独立推进；同一 reaction 内严格保序。
正常 completion 仍在正常 EOF、动作与必要结算后派发；interrupt 不伪造正常完成。

## 8. 其他 owner 入口

`prepare()` 仲裁共享 tree 的 preparation/reconcile，不提交 Frame，网络流可以继续运行。
`run()` 顺序发起并等待 Main，直到正常 exit 或 fault；丢弃 run waiter 不取消已接纳的 reaction。
`fork`、`fork_to` 和 `reset_model_context` 要求所有 reaction 已结算；保留原有 source 健康、协议闭合和
清理规则。它们不成为绕过活跃 stream 或 managed recovery 的入口。

## 9. 行为验证

测试消费 typed POM、Frame、canonical 数据和协议事件，不解析或快照 rendered prompt：

1. Main Pending 时，后续事件创建 Temporary 并独立完成；handle 不 await 也能推进。
2. 多条历史独立的流交错并使用相同局部 output/call ID，来源、结果、下一 Frame 的反馈仍正确归属；同一 retained lineage 的重复 canonical call_id 继续拒绝。
3. clone、多 waiter、丢弃 waiter 不增加请求或隐式取消；终态 interrupt 幂等。
4. 定向中断排队、preparation、handoff 前和 streaming 中的操作，其他流继续；无假 completion。
5. Temporary 完成、失败或中断保持主历史、checkpoint、receipt 与 baseline；Main 已发布事实不回滚。
6. 临时工具结果进入后续分支 Frame，模型能继续行动；主线只通过显式业务 view 接收分支结果。
7. XML 异步效果、诊断和恢复保留原来源；managed recovery 保留 owner 和执行位。
8. shutdown、panic 和 handle 回收不丢任务、清理或原 payload；能力不足和无合法 checkpoint 明确失败。
