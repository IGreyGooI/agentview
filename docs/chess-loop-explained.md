# Chess 运行循环说明

本文描述仓库中当前的 `chess_agentview` example，解释模型白方回合、Stockfish 黑方回合、重试和终局如何由默认的 `Application::run()` 驱动。

配套图仍保留在仓库中，但它们描述的是旧的 `prepare -> demand -> react` 编排，等待重新生成：

- [历史工作流图](chess-loop.workflow.html)
- [历史准备屏障时序图](chess-loop.sequence.html)
- [图源与旧交付校验记录](chess-loop.delivery.json)

本文件是当前行为的权威说明。源码入口：[Chess facade、driver、准备 hook 与 UCI actor](../examples/chess_agentview/chess_application.rs)、[状态机和事件处理结果](../examples/chess_agentview/application_state.rs)、[XML streaming contract 与发布器](../examples/chess_agentview/chess_action_component.rs)、[`Application` runtime](../src/component/execution/application.rs)、[`use_preparation`](../src/component/authoring/preparation.rs) 和 `use_application_exit`。

## 默认 run

`ChessApplication` 只持有 `Application<P>`（在此例中命名为 `reactor`）、停止信号和 actor 的 typed completion。它不保存 provider reaction 队列，也不直接调用 Stockfish。

Chess 直接调用默认入口，不再定义 `drive_application`：

```rust,ignore
let run_result = reactor.run().await;
```

`Application::run()` 内部反复调用 `react()`：`Continue(())` 继续下一轮，`Break(reason)` 返回 `Ok(reason)`，错误直接返回。它借用 Application，不自动执行 shutdown。`ExitReason` 只表示生命周期，Chess facade 随后从 `ChessActorExit::Completed(Result<ChessResult, ChessActorFailure>)` 读取最终棋局结果。需要手动单步时仍可直接调用 `react()`。

`ChessApplication::run` 在运行出错且 actor 尚未给出 completion 时，先调用 `stop_actor`，等待 UCI actor 的停止结果，再消费 `Application` 做 `shutdown()`。正常终局已经由 actor 清理 UCI 并发布 completion，因而不会重复停止。外部 `Requested` 退出若没有 typed completion，会让 Chess facade 报错，并沿用这条收尾路径。

## 准备是唯一就绪屏障

根组件 `chess_application` 保留三项长期状态或服务：

- `ChessState`：棋盘、历史、phase、重试计数和最终 outcome；
- 容量为 1 的 `Coroutine<ChessPreparationRequest>`：唯一的 UCI actor 入口；
- `use_application_exit()` 返回的应用级退出 capability。

它不再有 `requested_attempt`，也不调用 `use_reaction_request`。一次成功的外层 `react()` 已经足以提交一个 provider frame，因此没有独立 demand 需要去重或等待。

`react()` 在构造 frame 前运行 `use_preparation`。hook 对每次准备请求执行以下顺序：

1. 先检查 `actor_exit`；若 actor 已正常完成，请求 `ExitReason::Completed`，而不是让 provider 接管终局。
2. 向 actor 发送带 `oneshot` 回执的 `ChessPreparationRequest`，并等待回执。
3. 发送或回执结束后统一检查 typed completion，再传播发送或回执错误。只有 `Completed(Ok(_))` 会请求 `Completed` 退出；`Completed(Err(_))` 和 `Stopped(_)` 是 preparation error。actor 仍在运行时 `completion = None` 是正常状态，随后由 `AwaitingModel` 放行；只有发送或回执失败后仍没有 completion，或状态已经 `Finished` 但仍没有 completion 时，才是 preparation error。
4. 若 actor 仍在运行，读取 `ChessState::phase()`。只有 `AwaitingModel` 可以放行到 frame；`Ready`、`AwaitingStockfish` 和没有 completion 的 `Finished` 都会失败，绝不会提交 provider frame。

actor 只有在关掉 UCI 后才发布正常 `Completed` completion。因此 hook 发出 `Completed` 退出时，清理已经完成；`react()` 返回 `Break` 前不会产生额外模型请求。

`Application::prepare()` 仍可供低层验证使用，但 facade 不再把它当作外部编排步骤。`react()` 内部自己执行同一条准备屏障。

## 状态机与 actor

[`reduce`](../examples/chess_agentview/application_state.rs) 是唯一的 Chess 状态转换源。`reduce_signal` 通过一次 `Signal::update` 原子地应用它，避免发布器和 actor 各自读写副本。

reducer 只返回 `ChessReduction::Applied` 或 `Ignored`：当前 attempt 的非法动作会更新重试计数和 feedback，因此仍是 `Applied`；过期或不适用的事件是 `Ignored`。不再返回调度 effect，后续工作和终局结果由 actor 从 state 读取。拒绝原因只保存在 feedback 中。

phase 仍为 `Ready -> AwaitingModel -> AwaitingStockfish -> AwaitingModel -> ... -> Finished`：

- `Start` 只在 `Ready` 生效；
- `ModelAction` 只接受当前 `ModelAttemptKey`；
- `StockfishCompleted` 只在 `AwaitingStockfish` 生效；
- `Finished` 忽略后续事件。

UCI actor 按 preparation request 串行运行，但每次都以当前 phase 为准：

- `Ready`：归约 `Start`；
- `AwaitingStockfish`：复制一份已提交 history，并用它执行一次搜索；
- `AwaitingModel`：不做 engine work，只回执 ready；
- 终局：先关闭 UCI，发布 `ChessActorExit::Completed`，再回执并退出。

这个 phase guard 也是取消安全的关键：同一请求被后续准备再次观察时，已经完成的黑方搜索不会被重复执行。

## XML 如何成为模型动作

状态为 `AwaitingModel` 时，根组件挂载 [`chess_action_component`](../examples/chess_agentview/chess_action_component.rs)。它声明一个 identity 为 `chess.action`、版本为 `v1` 的 `XmlStreamingToolCall`。

每次 provider reaction 都有一份新的 `ChessActionAttempt`、独立 parser 和 occurrence ledger。`thought`、`choose_move`、`resign` 共用这份 contract-local state，所以能验证它们的顺序；其他 contract 即使声明同名标签，也拥有自己的 parser、状态和计数。

此例要求恰好两个元素：

```xml
<thought>简短的局面评估</thought><choose_move uci="e2e4" />
```

或：

```xml
<thought>简短的局面评估</thought><resign />
```

`thought` 必须恰好一次、非空且先于动作完成。`choose_move` 和 `resign` 只能有一个；`choose_move` 的 `uci` 必须是 canonical lowercase UCI。EOF 后 `decide_action` 统一判断缺 thought、空或不完整 thought、重复或晚到 thought、缺动作和多动作。attempt state 只记录有效 thought 是否已完成及单个 action，不保存 thought 正文；重复元素仍由 occurrence ledger 计数并拒绝。thought 不会写入 `ChessState`。

被接受的 `ChessAction` 经 `ChessActionPublisher` 同步归约到状态。publication receipt 将同一 operation 固定为 `Published` 或 `NotPublished`，所以重放不会重复落子，过期 attempt 也不能消费新回合。拒绝直接归约 typed feedback，并返回 `StreamingToolRejectionAction::Complete`；它不再请求 demand。

## `e2e4` 到 `e7e5`

1. 第一次 `react()` 的准备阶段向 actor 发请求。actor 将 `Ready` 归约为 `AwaitingModel`，回执后 hook 放行 provider frame。
2. provider 输出 `<thought>...</thought><choose_move uci="e2e4" />`。
3. XML contract 接受动作，发布器同步提交 `e2e4`，状态变为 `AwaitingStockfish`；第一次 `react()` 返回 `Continue(())`。
4. `Application::run()` 开始第二次 `react()`。这次的准备阶段看到 `AwaitingStockfish`，actor 执行一次搜索，例如 `e7e5`，并归约为 `AwaitingModel`。
5. actor 回执后，同一次第二次 `react()` 提交下一帧 provider frame。

模型 XML 拒绝也走同样节奏：本次 reaction 结束后 reducer 递增相同 ply 的 attempt index；下一次固定 `react()` 的准备阶段看到 `AwaitingModel`，直接提交重试 frame。没有 `requested_attempt` 或额外 demand 参与这个过程。

## 终局、取消和故障

若模型走子、Stockfish 走子或启动失败使状态终局，actor 在下一次准备 request 中收尾 UCI，并发布 typed completion。hook 请求 `ExitReason::Completed`，当前 `react()` 在提交 frame 前返回 `Break`。facade 随后读取 actor 的 `ChessResult`。

如果调用者在准备期间丢弃 `react()` future，已成功送入 coroutine 的 request 和已开始的 UCI 搜索仍由 actor 持续完成。之后的 `react()` 会再次请求准备，但 phase 已经是 `AwaitingModel`，所以 actor 只回执，不会再次运行 `go`。

发送 request 失败、oneshot 断开而 actor 没有成功 completion、actor 停止、UCI 清理失败和 provider fault 都是错误路径。错误路径不会把未确认的 actor 状态当成 provider-ready，并由 facade 聚合 actor stop 与 Application shutdown 的错误。

Unix fake-UCI tests 覆盖首次就绪不提交 frame、黑方搜索在下一次 `react()` 准备阶段发生、取消后不重复 `go`、重试、无可用 engine 零 provider 提交，以及终局不会多提交 frame。
