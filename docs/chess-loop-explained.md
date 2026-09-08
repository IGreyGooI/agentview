# Chess 运行循环说明

本文描述仓库中当前的 `chess_agentview` example，解释一次模型白方回合、一次 Stockfish 黑方回合、一次重试和终局在现有代码里如何发生。

配套图：

- [工作流图](chess-loop.workflow.html)
- [准备屏障时序图：黑方落子与下一次模型请求](chess-loop.sequence.html)

[图源与交付校验记录](chess-loop.delivery.json) 包含两张图的 SHA-256、结构检查和桌面截图复核结果。

源码入口：[Chess facade、driver、prepare hook 与 UCI actor](../examples/chess_agentview/chess_application.rs)、[状态机和 typed effect](../examples/chess_agentview/application_state.rs)、[XML streaming contract 与发布器](../examples/chess_agentview/chess_action_component.rs)、[`Application` runtime](../src/component/execution/application.rs) 和 [`use_preparation`](../src/component/authoring/preparation.rs)。

## 先看边界

`ChessApplication` 只持有 `Application<P>`、停止信号和 actor 的 typed completion。
它不自己保存 provider reaction 队列，也不直接调用 Stockfish。

[`ChessApplication::run` 和 `drive_application`](../examples/chess_agentview/chess_application.rs#L113-L200)
反复执行下面的顺序：

1. `runtime.prepare().await`。
2. 读取 `actor_exit`；已有完成结果就返回它。
3. 同时等待 actor 完成或 `runtime.wait_for_reaction_request()`。
4. 再次读取 `actor_exit`，避免刚得到 demand 时又把终局交给 provider。
5. 仅在仍未终局时调用 `runtime.react().await`。

因此 provider 不是游戏循环的时钟；它只在 Chess 组件显式提出需求时被调用。
actor 的 completion 是 `ChessActorExit::Completed(Result<ChessResult, ChessActorFailure>)`，
所以正常棋局结果、UCI 清理失败和外部停止不会被混成一条无类型通知。

`run` 遇到 driver 错误时先通过 `stop_actor` 请求 actor 收尾，再关闭 `Application`。
正常终局已经由 actor 清理完成，不会再次请求停止。

## 初次 mount 与首次 prepare

根组件是
[`chess_application`](../examples/chess_agentview/chess_application.rs#L214-L270)。
它保留以下状态和服务：

- `ChessState`：棋盘、历史、phase、重试计数和最终 outcome。
- `requested_attempt: Signal<Option<ModelAttemptKey>>`：已经请求过的模型回合。
- 容量为 1 的 `Coroutine<ChessPreparationRequest>`：唯一的 UCI actor 入口。

组件 mount 时会启动 coroutine；UCI actor 会建立 Stockfish 进程并等待 `ChessPreparationRequest`。
棋局状态仍为 `Ready`，此时不会因为 mount 自动给 provider 发 frame，也不会自动执行 `go`。

首次 `runtime.prepare()` 渲染组件并调用其 `use_preparation` hook。
hook 发送带 `oneshot::Sender<()>` 的 `ChessPreparationRequest`，然后等待 actor 回执。
actor 收到首个请求后，才针对 `Ready` 状态处理 `ChessEvent::Start`。

对默认的白方 agent，`Start` 令状态变为 `AwaitingModel`；
状态机产生 `RequestReaction`，但 actor 不直接触碰 provider。
hook 收到 actor 回执后读取 `ChessState::current_attempt()`，得到 `ModelAttemptKey { ply: 0, attempt_index: 0 }`。

只有这个 key 不等于 `requested_attempt` 时，hook 才调用 `reaction.request()`，
然后写入该 key。这个 dedup 使同一回合的重复 prepare 不会重复提交 provider demand。

## prepare 是屏障，不是 provider 调用

[`Application::prepare`](../src/component/execution/application.rs#L579-L614)
先检查外层 driver boundary，再完成前一轮 streaming 的清理，最后 await `prepare_components()`。
它不会准备或提交 provider frame。

[`prepare_components`](../src/component/execution/application.rs#L755-L780) 的前 16 个执行 wave 都按顺序做三件事：

1. reconcile 组件，取得 projection、bindings 和本 wave 的 preparation declarations。
2. 逐个 await preparation declaration。
3. 若 hook 的 Signal 写入让组件变 dirty，就重新 reconcile；最多执行 16 个 wave，必要时再做一次不执行 hook 的最终检查。

一次 `prepare_components()` 有一个新的 `PreparationRun`。
同一组件 mount generation 的同一 hook slot 在这个 operation 中最多运行一次；
新 mount 的子组件 hook 可以在后续 wave 运行。
下一次显式 `prepare()` 会新建 `PreparationRun`，因此会再次运行活跃 hook。

已完成 hook 捕获的输入即使被另一个 hook 改变，也不会在同一 operation 中自动重跑。
有先后依赖的工作要放在一个 hook 中顺序执行，或由父组件准备完成后挂载子组件。
这里的 ready 是本次声明的准备工作完成，不是任意后台任务都已经结束。

这个规则在
[`PreparationDeclaration::prepare`](../src/component/authoring/preparation.rs#L44-L87)
中由 `(mount generation, slot)` 记录实现。它不是跨 operation 的缓存。

`react()` 也有一次隐式 prepare：
[`react_inner`](../src/component/execution/application.rs#L662-L752)
先 refresh declaration，再调用同一个 `prepare_components()`，之后才构造和提交 provider frame。

在白方已经处于 `AwaitingModel` 时，这次隐式 prepare 仍会向 actor 发 request 并等待回执，
但因为 `requested_attempt` 已经等于当前 key，所以不会制造第二个 demand。

## 状态机的实际职责

[`reduce`](../examples/chess_agentview/application_state.rs#L180-L298) 是状态转换的唯一规则源。
`reduce_signal` 用一次 `Signal::update` 原子地调用它，避免发布器和 actor 各自读写副本而丢失更新。

主要 phase 是 `Ready -> AwaitingModel -> AwaitingStockfish -> AwaitingModel -> ... -> Finished`。

- `Start` 只在 `Ready` 生效。
- `ModelAction` 只有其 `ModelAttemptKey` 等于当前 key 时生效。
- `StockfishCompleted` 只有在 `AwaitingStockfish` 生效。
- `Finished` 忽略后来的事件。

`request_next_actor` 根据当前 side-to-move 产生 `RequestReaction` 或 `RequestStockfish`。
effect 表示业务状态已经要求下一方行动；它本身不是立即执行 UCI 或 provider 的命令。

## XML 输出如何成为模型动作

当状态是 `AwaitingModel` 时，根组件挂载
[`chess_action_component`](../examples/chess_agentview/chess_action_component.rs#L359-L405)。
它声明一个 identity 为 `chess.action`、版本为 `v1` 的 `XmlStreamingToolCall`。

这里有两种不同的状态：

| 状态 | 生命周期 | 用途 |
| --- | --- | --- |
| `Signal<ChessState>` | 根组件挂载期间，跨多个模型回合 | 保存棋盘、历史、phase、重试和终局；发布器和 actor 都通过 reducer 更新它 |
| `ChessActionAttempt` | 本次合约 attempt | 保存已完整接受的 `thought` 和候选 `actions`，供 `decide_action` 检查 |

每次 reaction 为这个合约创建自己的 parser 和 `ChessActionAttempt`。
`thought`、`choose_move`、`resign` 共用这个 parser 和临时状态，所以可以验证元素之间的先后关系。
其他组件声明的合约使用独立的 parser 和 attempt 状态；它们不会共用 Chess 的这份临时状态。
合约最终输出的是 `ChessAction`；`thought` 只参与本次验证，发布器不会把它写入 `ChessState`。

当前合约要求严格的两个输出元素，且没有 live value：
`<thought>简短的局面评估</thought><choose_move uci="e2e4" />`，
或者 `<thought>简短的局面评估</thought><resign />`。

[`thought_contract` 和 `configure_action`](../examples/chess_agentview/chess_action_component.rs#L192-L233)
规定：

- `thought` 必须恰好出现一次。
- 文本完成时必须去除空白后仍非空。
- `choose_move` 或 `resign` 只能在完整 `thought` 已被接受后完成。
- `choose_move` 是自闭合元素，并要求 canonical lowercase UCI 属性。

流可能把元素拆成多段。框架在 EOF 后才交给
[`decide_action`](../examples/chess_agentview/chess_action_component.rs#L235-L295)
做最终摘要判定；不完整的 `<thought>`、缺 thought、重复 thought、thought 在 action 之后、
缺动作和多个动作都会被拒绝。部分文本不会被当成已完成 thought。

## XML 拒绝与棋规拒绝不同

XML 或 contract 失败没有 accepted output。
`on_rejected` 提取 `InvalidActionReason`，直接把
`ModelAction { result: Err(reason) }` 归约进 `ChessState`，并返回
`StreamingToolRejectionAction::Complete`。

它故意不在 streaming 尚未结算时再返回 `RequestReaction`。
对于可重试拒绝，本次 `react()` 完成后，driver 的下一轮 `prepare()` 看到新的 attempt key，才发出下一次 demand。
如果拒绝次数已达到上限，状态直接进入终局，不再产生 key 或 demand。

例如模型第一次输出：

```xml
<choose_move uci="e2e4" />
```

它缺少 thought，得到 `MissingThought`。状态保持相同棋局、`ply` 仍为 0，
但 retry 计数变为 1；下一轮的 key 是 `ModelAttemptKey { ply: 0, attempt_index: 1 }`。

prompt 也会带上 `previous_decision="rejected:missing_thought"` 和 corrective reason。
第三次可纠正拒绝后，状态机以 `ModelForfeit` 终局。

棋规拒绝走另一条路径。若 thought 有效，但动作是语法正确、走法不合法的 `<choose_move uci="e2e5" />`，
先通过 XML contract，随后发布器调用 reducer；
`validate_model_action` 检查 agent side、终局状态和合法走子，返回 `IllegalMove`。
这仍是一次已发布的 typed action，因为 reducer 已同步消费它；随后状态机安排相同的重试机制。

## 发布器的同步确认与幂等性

[`ChessActionPublisher`](../examples/chess_agentview/chess_action_component.rs#L90-L190)
在 accepted attempt 中要求正好一个 `Output(ChessAction)`，再创建带 receipt 的 publication operation。

第一次 `publish` 会用捕获的 `ModelAttemptKey` 调用 `reduce_signal`：

- reducer 返回非空 effect：记录 `Published` 并返回成功。
- reducer 返回空 effect：该 action 已过期或状态不再接收它，记录 `NotPublished`。

receipt 一旦落在 `Published` 或 `NotPublished`，同一个 operation 的重复 `publish` 不会再次修改棋局；
`resolve` 也从同一 receipt 给出恢复答案。这里没有等待另一个 actor 的确认窗口，
因为 reducer 的 `Signal::update` 已经同步消费了动作。

## `e2e4` 到 `e7e5` 的一条完整轨迹

1. 首次 prepare 使状态从 `Ready` 进入 `AwaitingModel`，请求 key `{ ply: 0, attempt_index: 0 }`。
2. driver 得到 demand，调用 `react()`；它先完成自己的隐式 prepare，再提交包含棋盘、合法走法和 XML policy 的 frame。
3. provider 流输出 `<thought>...</thought><choose_move uci="e2e4" />`。
4. XML contract 接受动作；发布器同步归约，提交 `e2e4`，状态改为 `AwaitingStockfish`。
5. 框架完成 streaming 结算、清理和 post-reconcile，`react()` 返回。虽然 reducer 返回了 `RequestStockfish`，本次调用不会启动 UCI `go`。
6. driver 下一轮显式 prepare 向 actor 发送新的 `ChessPreparationRequest`。
7. actor 观察到 `AwaitingStockfish`，从已提交 history 重建 `StockfishRequest`，执行 `best_move`，例如得到 `e7e5`。
8. actor 归约 `StockfishCompleted(Ok(e7e5))`，提交黑方走法，状态变为 `AwaitingModel`。
9. actor 回执该 prepare；hook 读到新 key `{ ply: 2, attempt_index: 0 }`，它不同于旧 key，于是请求下一次 provider reaction。

实际 UCI work 在
[`run_stockfish_actor`](../examples/chess_agentview/chess_application.rs#L272-L357)
中按 request 串行执行。它不直接消费 reducer 返回的 `RequestStockfish`；
它以当前 phase 为准，只在收到后续 prepare request 且 phase 为 `AwaitingStockfish` 时搜索。

## 终局、取消和故障

当 prepare 后 actor 发现 `ChessState::outcome()` 已存在，它先调用 `shutdown_engine`，
再发布 `ChessActorExit::Completed` 并回执 request。driver 在 prepare 后、以及拿到 demand 后都检查 completion，
所以终局不会再提交一帧 provider reaction。

这覆盖了 ply limit、将死、和棋、认输、Stockfish 返回非法走法或搜索失败等终局。
如果 Stockfish 启动失败，actor 用 `EngineFailed` 归约成 `StockfishFailed`；
prepare hook 发现 actor 已完成后正常返回，让 driver 读取这个 typed outcome，而不是向 provider 求助。
如果启动失败后连子进程清理也无法确认，则返回 `ChessActorFailure::StockfishCleanup`。

如果 request 已送入 actor，而调用者取消了正在等待回执的 `runtime.prepare()`，hook 的 oneshot receiver 会被丢弃。
已经进入容量为 1 coroutine 的 `ChessPreparationRequest` 和正在进行的 UCI 搜索不会被取消；
actor 仍会完成当前 phase 的工作，通过 sender 回执时忽略 receiver 已丢弃造成的发送失败。

后续 prepare 会再发一个 request。若前一个请求已经完成 `e7e5`，状态已是 `AwaitingModel`，
phase guard 阻止第二个 request 重复执行 Stockfish。这个行为由
[`cancelled_preparation_does_not_duplicate_stockfish_work` 测试](../examples/chess_agentview/chess_application.rs)
覆盖。

相反，若 hook 无法发送 request 或 actor 在没有 completion 的情况下消失，hook 返回 preparation error；
`Application::prepare` 把它作为 `ApplicationFault` 传播，provider 仍未被调用。
provider/streaming 层的错误则由 `Application::react` 的 streaming cleanup 和 fault 路径处理；
如果此时 actor 尚未完成，`ChessApplication::run` 会请求 actor 停止并等待 UCI 清理结果；
actor 已完成时则直接进入 `Application` shutdown。

当前实现的可执行验证集中在
[`chess_application.rs` 的 Unix tests](../examples/chess_agentview/chess_application.rs)：
初始化不提交 provider、`e2e4` 延后到下一次 prepare 才搜索、重复 prepare 不重复搜索、
取消 prepare 不重复搜索，以及终局不多提交 provider frame。
