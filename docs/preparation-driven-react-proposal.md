# 提案：用 use_preparation 驱动模型回合

日期：2026-09-08

状态：部分落地。固定 react 循环、应用退出 API、Chess 迁移和同批 preparation 并发已实现。
下文保留完整提案；其中带 hook identity 的错误诊断仍是提案目标，不是本轮并发实现的承诺。
当前实现仍以 [engine.md](engine.md) 和 [component-preparation-design.md](component-preparation-design.md) 为准。

## 1. 建议

宿主用固定循环调用 `Application::react()`，组件通过 `use_preparation` 声明本轮模型调用的必要条件。
runtime 并发等待这些准备工作，状态稳定后才提交 Frame。正常退出通过 `react()` 的返回值表达。

外界负责发送业务输入、提供资源和请求退出。组件决定何时消费输入、完成准备、更新状态；宿主不再为
每种业务编写“等事件，再调用 react”的调度逻辑。

这可以覆盖聊天、CLI、Plugin 和 Chess 等主要工作流，但不承诺未经统计的“99%”。一个 Application
仍然只有一个 logical target session；多个独立 agent 使用各自的 Application。

## 2. 当前实现与拟议变化

| 边界 | 当前实现 | 本提案 |
| --- | --- | --- |
| 调用入口 | 外界调用 `react()`，部分 example 先等 demand | 固定循环直接 await `react()` |
| 准备阶段 | `react()` 内已有 `prepare_components()` | 保留为模型调用前唯一 readiness barrier |
| 同批 hook | `FuturesUnordered` 并发 poll；首个被观察到的错误 drop 其余 future | 保留；全部成功后才进入下一批或提交 Frame |
| 跨批准备 | reconcile 发现新 mount，最多 16 批 | 保留 |
| 正常结束 | `Result<(), ApplicationFault>` 无应用完成值 | 返回 `ControlFlow::Break` |
| Chess | 外层 prepare、completion/demand select、react | 外层只有 react；准备和终局由组件表达 |

依据：[Application](../src/component/execution/application.rs)、[PreparationSet](../src/component/authoring/preparation.rs)、
[Chess driver 与根组件](../examples/chess_agentview/chess_application.rs)。

## 3. 公共 API

`use_preparation` 保持现有签名，成功完成意味着这项准备已满足：

```rust
pub fn use_preparation<Factory, PreparationFuture, Error>(factory: Factory)
where
    Factory: FnOnce() -> PreparationFuture + Send + 'static,
    PreparationFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: Display + Send + 'static;
```

新增应用退出能力，并修改两个 operation 的返回值：

```rust
pub enum ExitReason {
    Completed,
    Requested,
}

pub fn use_application_exit() -> ApplicationExitHandle;

impl ApplicationExitHandle {
    pub fn request(&self, reason: ExitReason) -> Result<(), ApplicationExitError>;
}

impl<P: ReactionPort> Application<P> {
    pub fn exit_handle(&self) -> ApplicationExitHandle;

    pub async fn react(
        &mut self,
    ) -> Result<ControlFlow<ExitReason>, ApplicationFault>;

    pub async fn prepare(
        &mut self,
    ) -> Result<ControlFlow<ExitReason>, ApplicationFault>;
}
```

同批并发不改变这个返回类型：`Ok(())`只表示该 hook 已 ready，所有同批 hook 都成功后 runtime 才能继续；
`Err(_)`结束当前 operation。exit、首个被观察到的同批错误或调用者丢弃 operation 时，runtime drop 未完成
future 来取消等待，不增加 `Cancelled` 或 `MoveOn` 之类的 hook 返回值。需要正常结束应用的组件仍调用
`use_application_exit()`。

`ApplicationExitHandle` 可 clone。组件取得的句柄受 mount generation 约束，旧组件不能终止新一代组件所在的
应用；宿主取得的句柄作用于整个 Application。应用已经 shutdown 时请求失败。

`ExitReason` 只表达生命周期，棋局结果、Plugin invocation 结果仍保存在各自的 typed result 通道或业务状态里。
不把 `ChessResult` 塞进 `Any`，也不为退出给 `Application<P>` 增加业务结果泛型。

选择独立退出句柄，是为了让正在等待外部输入的 preparation 也可以被退出通知唤醒。退出不必依赖那个
hook 自己返回，不需要为所有 preparation 增加另一套返回枚举。

## 4. 固定 driver

以下是拟议的完整调度函数；它只决定继续或退出，不等待业务事件：

```rust
async fn drive<P: ReactionPort>(
    application: &mut Application<P>,
) -> Result<ExitReason, ApplicationFault> {
    loop {
        match application.react().await? {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(reason) => return Ok(reason),
        }
    }
}
```

宿主在 `drive` 正常返回后统一执行资源收尾和 consuming `shutdown()`，`Break` 和
`Err(ApplicationFault)` 都经过这条路径。
不能在拥有资源的最外层直接 `drive(...).await?`，使错误跳过异步清理。

panic 继续遵循既有 unwind/terminal 规则，不经过 `Result`。普通返回路径不能保证 panic 后的异步收尾；
只有 owner 显式建立适用的 unwind 清理边界时才能尝试 shutdown，也不承诺 Application 可恢复运行。

`react()` 的返回含义：

| 返回值 | 含义 |
| --- | --- |
| `Continue(())` | 本次模型回合完成，streaming 已结算，可以进入下一次 react |
| `Break(reason)` | 退出已记录，本次没有活动的 preparation/provider/streaming 工作，后续不再提交 Frame |
| `Err(fault)` | 按现有 fault/recovery 协议处理；不将失败变成成功退出 |

`prepare()` 的 `Continue` 只表示准备完成，始终不调用 Provider。它保留给预加载和调试；固定 driver
不在 react 前另外调用它，否则两次 operation 会各执行一次 hook。

重复调用已经正常退出的 `react()` 或 `prepare()` 返回同一 `Break`，不重新执行准备工作。

## 5. 一次 react 的执行顺序

```text
完成上轮 streaming 清理或恢复
    ↓
检查应用 exit
    ↓
reconcile，取得本批 preparation 和 projection
    ↓
并发等待本批 hook；首个错误或 exit drop 未完成 future
    ↓
若状态变 dirty：reconcile，准备新挂载组件
    ↓
所有必需准备完成，projection 稳定
    ↓
构造 Frame，检查 exit，取得本次提交许可
    ↓
提交 Frame，消费 Provider 输出
    ↓
完成工具发布、streaming 清理和 post-reconcile
    ↓
返回 Continue；期间收到 exit 则返回 Break
```

一次 react 只执行一轮业务 reaction。现有 continuity mismatch 的提交重试继续由 runtime 处理，
不能重跑本次已完成的 preparation，也不能变成新的业务回合。

这里的 blocking 是异步等待，不是阻塞线程。runtime 可以在内部使用 `select!` 协调退出和任务故障；
业务 driver 无需感知这些分支。Signal 写入本身仍不自动创建 reaction。

## 6. 并发屏障的精确语义

### 6.1 每批并发，跨批稳定

沿用当前 `PreparationRun` 和 `(component mount generation, lexical hook slot)` 身份：

1. 每次 react 创建新的完成记录。
2. 每批先 reconcile，并固定本次 render 声明的 hook 集合。
3. 授权后按声明顺序创建 future；factory 应同步、快速，不执行阻塞 I/O。
4. 返回的 futures 并发 poll，完成时由 runtime 验证 mount 并记录成功。
5. 全部成功后再 reconcile；新挂载 hook 在下一批执行，已完成 hook 不重跑。
6. 最多执行 16 批，保留最后一次只检查、不执行 hook 的稳定性检查。

实现优先使用现有 `futures` 依赖的并发 future 集合。hook future 由 react operation 持有，
不通过 `tokio::spawn` 转成脱离调用者的后台任务。

同批 futures 没有完成顺序保证。runtime 不从 Signal 读取或闭包捕获推断依赖关系。
两个 hook 修改同一业务状态时，组件需要用原子 reducer 或明确串行步骤保证一致性。

### 6.2 必须等的是模型调用

一个慢 hook 会延迟共享 Frame 的提交；其他独立 hook 仍可继续准备，后台 actor 也能继续工作。
日志、遥测等不影响模型正确性的工作不应声明为 preparation。

同一个 Application 内，所有已声明 preparation 都是必需条件，即 AND：

```text
棋盘 ready AND 规则 ready AND 本轮上下文 ready → 可以提交 Frame
```

“消息 OR 定时器 OR Plugin 请求”应由一个业务事件入口汇总，让一个 hook 等待任意有效事件。
不能分别声明三个无限等待的 preparation，却期待其中一个完成就放行。

### 6.3 依赖与首版限制

有先后关系的工作写进一个 hook 顺序 await；也可以由父 hook 完成后写入 Signal，挂载下一阶段子组件。
同一 operation 中，已经完成的 hook 不因捕获参数变化而自动重跑。

首版固定每批的 hook 集合，不在同批 pending 时执行中途 reconcile。因此 A 写状态要求卸载 B，
不会立即取消本批已运行的 B；B 仍需完成、由业务取消信号唤醒，或由应用 exit 取消。
不能让 B 无限等待，并仅靠 A 的重新渲染意图解除屏障。

这个限制保持当前 preparation 的分批模型可预测。若未来确有“准备途中切换页面并立即卸载等待者”的
需求，再独立设计 pending hook 的增删、取消和错误仲裁，不在此次改动里隐式加入。

所有 hook 都立即返回、或根本没有 hook 时，固定循环会连续调用模型。这适用于自主运行；聊天等按输入
推进的应用必须在 preparation 中实际等待新输入。runtime 不通过 sleep 或 dirty 标记猜测业务节奏。

## 7. exit、取消与错误

### 7.1 应用退出是持久状态

exit 请求记录在 Application 内并唤醒等待者，不是一次可能丢失的通知。采用第一个成功请求的
`ExitReason`，之后的请求幂等，不替换原因。原因只是生命周期提示，不能替代 typed result 或掩盖故障。

准备阶段观察到 exit 时，取消未完成的 hook futures，不等待所有组件 ready，不启动下一批或新提交。
同步执行中且不让出控制权的用户代码不能被异步退出强行打断。

### 7.2 exit 与模型提交的先后关系

exit 和“取得提交许可”必须经过同一个同步状态检查，不能仅在 prepare 前读一次布尔值：

| 哪一方先取得边界 | 行为 |
| --- | --- |
| exit 先记录 | 本次不调用 `ReactionPort::submit`，返回 Break |
| 提交许可先取得 | 允许当前提交和 reaction 完成，随后 Break，不进入下一轮 |

提交许可在即将调用 Provider 时取得；它与既有 Frame handoff/history commit 是不同边界，不改变后者。
许可之后到达的 exit 属于温和退出，不保证中断当前网络请求。continuity retry 也必须再次检查退出许可。

正在发布的动作、未确定的 publication receipt 和 streaming recovery 不能被 exit 跳过。
只有本轮结算完成才返回正常 Break；需要恢复时仍可返回 `RecoveryRequired`，并保留退出状态。
实际运行故障和监督到的 panic 维持原有优先级，不被正常 exit 覆盖。

### 7.3 取消不是回滚

一个 hook 返回错误时，runtime 报告首个被观察到的 preparation fault，并取消同批剩余 futures。
多个并发错误按实际观察顺序处理，不承诺词法顺序；已完成的 Signal 写入和外部副作用不回滚。
带 hook identity 的 error diagnostic 是本提案的后续目标，本轮并发实现不把它变成新的公共错误契约。

调用者丢弃 react future 同样不撤销已发生的工作。下一次调用创建新的 `PreparationRun`；有副作用的
preparation 必须容忍重试。actor 已接收的工作由 actor 自己管理，丢弃等待回执的 future 不等于取消工作。

### 7.4 Break 与 shutdown 的边界

Break 不表示所有业务资源都已优雅关闭。组件后台任务和 pending mount retirement 仍按既有生命周期
由 owner 收尾；不能直接丢弃这些记录并声称清理完成。

`Application::shutdown()` 当前会 fence、abort 并等待组件任务结束，它不自动保证 Stockfish 收到 `quit`。
Chess facade 应先 stop/await actor，确认 UCI 清理，再 consuming shutdown。外部退出也使用这条路径。

CLI 信号监听器或 Plugin 宿主可在循环开始前取得 `exit_handle()`；它们只发送退出请求，不竞争调用 react。
强制停止正在进行的 Provider 请求需要现有取消、超时和恢复机制；本次只定义温和 exit。

## 8. 外部输入如何接入

外界把业务事件发给 component 所属的 inbox、状态服务或 coroutine。preparation 等待的是业务事件，
不是另一个名叫 reaction request 的调度通知。

聊天或 Plugin 的准备过程应类似下面的业务伪代码：

```text
如果已经有本轮待处理输入：复用它
否则：等待下一条输入，并立即保存为本轮待处理输入
准备这条输入需要的上下文
返回 ready
```

输入与输出保留关联 ID，直到业务明确完成。不能在每次 preparation 重试时无条件 recv 下一条消息，
否则已取出的旧输入可能被跳过或覆盖。取出事件后应在下一次 await 前保存到保留状态，或使用业务已有的
reserve/ack 机制；runtime 不为外部队列提供通用 exactly-once 保证。

一次用户输入可能需要多个模型回合和工具步骤。输入所属业务任务尚未完成时，preparation 复用当前任务、
等待本步需要的工具结果；只有任务完成后才等待下一条用户输入。

提交前取消或失败时，重试复用同一输入。handoff 后发生的错误遵循现有 history/recovery 规则，
不能因为还没收到完整回复就自动把该输入当成一个新 invocation 再投递。

第一版不增加通用消息 broker、Frame 输入 lease 或新的事件总线。先在接入层证明持久输入和关联 ID 的
语义，再根据真实重复代码决定是否抽取公共能力。

## 9. Chess 的迁移

### 删除调度重复

- driver 中的显式 `runtime.prepare()`、completion/demand `select!` 和两处 completion 检查。
- 根组件的 `use_reaction_request()`、`reaction.request()` 和 `requested_attempt`。
- 把终局通知当成外部调度时钟的逻辑。

### 保留业务与资源约束

- `Signal<ChessState>`、纯 reducer 和 `ModelAttemptKey`。后者仍用于动作去重和拒绝陈旧发布。
- XML contract 内的 thought/action 顺序校验、attempt 临时状态和独立 parser。
- publisher 的同步 `reduce_signal` 和 `Pending/Published/NotPublished` receipt。
- Stockfish coroutine、串行 UCI 工作、phase guard 和有界引擎清理。
- actor 的 typed completion/result，以及 facade 的错误聚合和最终收尾。

准备 hook 仍向 actor 发送请求并等待回执；回执之后先检查 typed completion。正常终局调用
`exit.request(ExitReason::Completed)`；actor failure 作为错误传播；只有 `AwaitingModel` 才正常放行。
actor 启动失败、已经关闭导致的发送失败，也必须先检查 typed completion，不能当作 ready 提交模型。

正常一轮为：

```text
react #1
  preparation：actor Start → AwaitingModel
  模型：thought + e2e4
  publish：棋盘更新 → AwaitingStockfish
  返回 Continue

react #2
  preparation：actor 搜索 e7e5 → 更新棋盘 → AwaitingModel
  模型：根据包含 e7e5 的 Frame 输出下一手
  publish、结算 → Continue
```

XML 拒绝仍返回 `StreamingToolRejectionAction::Complete`。reducer 更新 retry key 后，下一次 react
自然准备并重试；不额外请求 reaction。达到重试上限时，下一次 preparation 处理终局，零额外模型调用。

准备被取消后，单一 actor 继续完成已接收的 UCI 工作；后续请求按最新 phase 处理。
现有串行 actor 和 phase guard 已可防止重复搜索，迁移不要求额外引入一套引擎 publication 协议。

正常终局由 actor 先关闭引擎并记录结果，hook 再请求 Completed。外部 Requested exit 则先解除 barrier，
facade 随后 stop/await actor，最后 shutdown Application。

## 10. 兼容与实施顺序

这是明确的 API 和调度语义变更：`react/prepare` 的返回类型改变，同批 hook 不再按 await 顺序执行。
其中同批并发部分不改变 `use_preparation`、`react()` 或 `prepare()` 的返回类型。
不引入功能相同的 `run_step/drive_next`；仍以 `react` 为唯一模型回合入口。

现有 demand API 可以暂时保留给旧集成显式调用，但新循环不等待也不消费 demand。
不能混用两种驱动模式并期望 request 等于一个额外回合。共享 streaming 里的 recovery fence 必须保留，
不能为了删除 Chess demand 顺带删除它。

1. 增加退出状态、组件/宿主句柄、返回值和提交许可检查，覆盖退出竞态及恢复路径。
2. PreparationSet 已改为每批并发，保留 mount fencing、完成身份和 16 批限制。
3. 简化 Chess driver 与 hook，验证正常回合、拒绝重试、取消准备和终局。
4. 用一个可控的外部输入场景验证等待、重试复用输入和关闭入口；迁移受影响调用方及编译测试。
5. 更新 engine、preparation 文档和 Chess 图；盘点旧 demand 使用者后再决定公共 API 的移除范围。

本提案获采纳前，既有 Chess 图仍描述当前实现，不改成未来方案。

## 11. 验收标准

| 场景 | 必须证明 |
| --- | --- |
| 固定循环 | 无业务 select、无 demand；每个 Continue 完成一轮 reaction |
| 同批并发 | A 尚未 ready 时 B 已进入等待；两者都 ready 才提交 |
| 同批错误 | A 永久 pending 时 B 的错误仍返回，且 A 的 future 被 drop |
| 顺序依赖 | 父准备完成后才挂载并执行子 hook，Frame 包含子准备结果 |
| 身份与预算 | 同一 operation 同 slot 一次；下一 operation 重跑；第 17 批无提交 |
| 冻结批次 | A 要求卸载 pending B 不会隐式放行；B 完成或 exit 才结束等待 |
| 退出通知 | 请求先于等待也不丢失；pending hook 被取消；重复退出幂等；旧 mount 句柄失效 |
| 提交竞态 | exit 先取得许可边界时零 submit；当前提交先取得时结算后 Break，零后继调用 |
| 未确定发布 | exit 不越过 receipt/recovery；未恢复不能报告正常 Break |
| 错误与 panic | 不被 exit 吞掉；并发取消保留已完成 Signal 写入；返回错误走收尾，panic 保留既有 unwind 语义 |
| Chess | 黑棋在下一次 react 的 preparation 执行；取消等待不重复 go；终局无多余 Frame |
| 外部输入 | 无输入时不调用模型；准备重试复用原输入；已 handoff 的输入不当成新调用重放 |
| 资源清理 | 正常与外部退出都确认 UCI 关闭后 shutdown；不以 task abort 代替引擎确认 |

验证以可控 future gate、记录提交次数的假 Provider、假 UCI 进程为主；不需要付费模型调用。
受影响的 preparation API、组件宏编译测试和 Chess tests 在默认 features 与 `--no-default-features` 下运行。
