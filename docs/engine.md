# AgentView Engine 设计

本文是 AgentView Engine 的权威设计文档。它规定稳定边界、状态所有权、调度语义和必须保持的
不变量，不记录实现进度、测试数量、提交版本或临时工作状态。

若其他实现说明与本文冲突，以本文为准。

## 1. Engine 的目标

AgentView 把 LLM 应用组织成 retained Component tree：

- Component 持有业务状态，并声明模型当前需要看到的界面；
- Engine render 完整的当前界面；
- ProviderPort 把界面转换成具体模型请求；
- 模型输出被转换成 provider-neutral Event；
- Component 消费 Event、更新业务状态或回答 ToolCall；
- 只有应用显式发起下一次 reaction 时，Engine 才再次 render 和调用模型。

Engine 保证因果顺序、历史一致性、取消和资源回收。它不替业务决定某个棋步是否合法，也不把
Provider conversation 当成业务数据库。

```text
ComponentHost                   ProviderPort
props + Signal                  local/open/wire history
      |                                  |
      | render                           | encode / stream
      v                                  v
RenderedProjection -> ApplicationHost -> Model Provider
                           ^                 |
                           | ProviderEvent   |
                           +-----------------+
                           |
                    Component consumers
```

## 2. 所有权边界

### ComponentHost

`ComponentHost` 是业务权威，拥有：

- root props；
- mounted Component identity；
- `use_signal` state；
- Component tree 表达的完整业务 POM；
- render 时生成的 reaction-local consumers。

Signal 写入立即成为业务事实。Engine 不 fork 或回滚 Component state。业务需要事务性时，应在
业务 Component 内先校验，再一次性写入自洽的新状态。

### ApplicationHost / Runtime

`ApplicationHost` 负责一次 reaction 的生命周期：

- render Component tree；
- 调用 ProviderPort；
- 消费 ProviderEvent stream；
- 调度普通 Event handler 和 ToolCall lanes；
- 等待 reaction-local 工作完成。

它不拥有业务状态，也不拥有 Provider wire history。

### ProviderPort

公共 Provider 边界保持一个方法：

```rust
#[async_trait]
pub trait ProviderPort: Send {
    async fn execute<'a>(
        &'a mut self,
        projection: RenderedProjection,
    ) -> Result<ProviderEventStream<'a>, ProviderFault>;
}
```

ProviderPort 私下拥有：

- canonical input 到具体 wire item 的编码；
- `instructions`、tools 和其他 request capability 的 lowering；
- local history、inflight items 和 pending ToolCall results；
- Provider output ledger、item identity 和顺序校验；
- ToolCall 与 ToolOutput 的配对；
- prompt cache、compaction、continuation 和 recovery。

Component 不直接调用 Provider `commit`、`abort` 或修改 history。ProviderEvent stream 在 Engine
内部关联本轮 history writer 和 ToolOutput sink；具体 Rust 表示可以调整，但不能增加 Component
可调用的 history API。

### ToolCall Component

ToolCall Component 声明一个模型可调用的能力。它接收完整 ToolCall，执行、拒绝或报告失败，
然后产生同一 `call_id` 的 ToolOutput。它不编码 Responses wire item，也不直接写 Provider
history。

`call_id` 只能标识这一次模型调用，不能保证同一业务动作在重试时仍使用相同 ID。有外部副作用
的工具默认只能承诺 at-least-once；需要 exactly-once 时，业务 Component 必须使用稳定的业务
幂等键或持久 effect journal。Engine 的本地取消不能撤销已经发生的外部动作。

## 3. Component tree 如何变成 Responses input

每次 reaction 都从完整 render 开始，而不是从上一次 UI patch 开始：

1. Component tree render 为有序的 `RenderedProjectionNode` 列表；
2. 每个 node 带稳定的 runtime `ComponentId` 和自己的有序 `CanonicalInputItem`；
3. ProviderPort 用 node identity 对照已接受的 projection diff memo，找出新增内容；
4. `#[diff]` slot 在 node 内 lower 为 `full`、`delta` 或 `omit`；
5. provider-neutral item 再编码成 Responses `instructions` 和 `input` items；
6. 新输入与当前 wire-legal history 组成本轮不可变 request snapshot。

完整 projection 中的 `RenderedProjectionDiffMarker` 只是 `#[diff]` 产生的声明元数据，不是已经
计算出的 delta。它的 `item_index` 定位本轮完整 item；ProviderPort 再把 node identity、
node-local structural path 和 diff slot 组成 memo key，并根据私有 baseline 计算 `full`、`delta`
或 `omit`。

这不是对渲染字符串做文本 diff。diff 的地址是：

```text
ComponentId + node-local structural path + diff slot
```

首次出现、context 丢失或无法安全表达增量时发送完整值；值未变化时 omit；只有结构明确且可省略
稳定字段时才发送 semantic delta。逻辑 causal history 是 append-only，当前 tree 不再渲染某个
旧 item 不代表从模型历史中撤回它。wire replay window 可以被已经验证的 compaction
原子替换，但 compaction 不能改变逻辑顺序或丢失仍然需要的因果事实。

Responses lowering 的基本映射是：

| AgentView 内容 | Responses 位置 |
| --- | --- |
| 基础 system instruction | 顶层 `instructions` |
| developer/user/assistant message | `input` 对应 message item |
| ToolCall / ToolOutput | `input` function call/output item |
| reasoning / compaction | ProviderPort 私有 wire history |

普通 Responses HTTP 使用 `store: false`，不依赖 `previous_response_id`。每次请求发送完整、合法的
wire history 加本轮新输入。`prompt_cache_key` 只优化相同前缀的缓存命中，不承担历史
正确性。

## 4. 一次 reaction

```text
render complete projection
  -> compile a wire-legal request snapshot
  -> commit newly submitted input to local history
  -> start Provider request
  -> consume ProviderEvents
       -> delta: commit partial record -> update inflight item -> publish event
       -> output_item.done: validate and seal the accumulated item
       -> ToolCall item done: publish -> schedule call_id lane
  -> Provider completes and stream reaches normal EOF
  -> await all ToolCall lanes
  -> stage ToolOutputs in ToolCall ordinal for the next Input Gate
  -> run terminal handlers
  -> validate required typed/business output
  -> commit business output
  -> return success
```

同一次 reaction 只 render 一次、调用 Provider 一次。Signal 更新只会影响下一次显式 reaction，
不会在当前 reaction 内自动 rerender 或再次调用模型。

`response.completed` 只校验 Provider 的终态摘要并结束当前 response，不是 history commit 点。
terminal handler、typed output 和整个 reaction 的成功也不是 Provider history 的提交门槛。

Provider history 按已经发生的因果事实逐段推进：请求 input 在提交时推进；每个 partial output
Event 在发布前推进；`response.output_item.done` 只封口已经累计的 item。ToolOutput 是 outbound
input：handler 完成时仅进入 provider-owned staging table，和新 projection input 一起在下一次
Input Gate handoff 才推进。后续 timeout、断流或业务失败不回滚这些已经推进的记录。

业务层拒绝模型行为不等于 reaction 基础设施失败。例如 Chess 裁判判定棋步非法时，模型确实
产生过该输出；Engine 保留模型输出，同时由业务 Component 写入非法原因，让下一轮模型看到
纠错反馈。

## 5. Provider history

ProviderPort 内部必须区分三种东西：

```text
local_history
  已提交的 input、每个已发布 partial record、item boundary、ToolCall 和已经 handoff 的 ToolOutput
  是 append-only 本地因果日志，可以暂时包含未完成 item 或未闭合 ToolCall

inflight_items
  从 local_history 中的 partial records 累计出的当前 output assembly
  尚未 item.done 或 aborted；不能原样进入下一次 wire request

staged_inputs
  已被 ToolCall handler 接受、按 call_id 隔离保存的 ToolOutput；它们是下一次 outbound input
  candidate 的一部分，但在 Gate handoff 前不属于 local_history，不能因本地 prepare failure 消失

wire_snapshot
  每次发送前从 local_history 编译出的不可变、语法合法请求
  其中所有 ToolCall 都必须已经闭合
```

history 的推进点：

1. projection lowering、diff、pending ToolCall closure 和编码全部成功后，生成不可变 request snapshot；
2. request 交给 transport 时，本轮 projection input 与已 staged ToolOutput 作为一个 submission
   segment 写入 local history；
3. 每个可见 wire delta 先作为 partial record 追加到 local history，再更新 inflight assembly，
   最后发布对应 Event；
4. `response.output_item.done` 校验 item identity、类型和累计内容后，在 local history 追加 sealed
   boundary；它不再是该模型输出首次进入 history 的时刻，也不等待 `response.completed`；
5. 完整 ToolCall 同样在 item done 时进入 local history，并登记为 pending；
6. ToolOutput 先进入 per-call result table；Provider output 顺序封口后，下一次 Input Gate 将它们
   和 projection input 按 ToolCall ordinal 组成同一个不可变 submission，并只在 handoff 时追加到
   local history；
7. ToolCall 和 ToolOutput 的追加都经过串行 sequencer；同一 `call_id` 第二次出现、重复 result、
   orphan result 都是 ledger fault；
8. terminal handler 或 typed/business output 的成功与否，不回滚已经追加的 Provider history。

local history 可以暂时是“open”的，但 wire snapshot 绝不能 open。发起任何下一次 HTTP 请求前，
ProviderPort 必须检查：

- 当前 response 已结束；正常 item 已 sealed，异常中止的 partial item 已标记 aborted；
- 每个 ToolCall 都存在唯一的 ToolOutput，并且该 ToolOutput 就在下一次请求中提交；
- item identity、顺序和类型可以编码成合法 Responses input。

检查通过才发送；检查失败则在本地返回 fault，不发送 HTTP。`response.completed`、normal EOF 和
Component reaction success 都不是额外的 history commit gate。

wire compiler 必须把 committed partial records 折叠成具体 Provider 接受的合法表示，同时保留
“这是 incomplete/aborted output”的事实。若某种 partial output 无法被可靠地转换成下一次请求的
合法 history，ProviderPort 就不得把它发布成 ProviderEvent。

对于已经发布但随后中止的 text partial，下一次 wire history 必须同时包含已经累计的实际文本和
中止事实：优先使用 Provider 原生 incomplete/aborted item；Provider 不支持时，lower 为合法的
assistant 内容并紧跟一个模型可见的 interruption marker。不能只留下 marker 而丢掉已发布文本。

timeout、断流或取消只会终止当前 inflight response：已经提交的 input、所有已经发布的 partial
records、sealed items 和已经组装的 ToolOutputs 都保留；尚未 sealed 的 assembly 标记为 aborted，
不能静默删除，也不能冒充正常完成。下一次 reaction 从它们编译出的 wire-legal history 和当前
Component projection 恢复。

### Input Gate

每个 outbound model request 都先经过一个 ProviderPort 私有的 Input Gate。Gate 的 owner 是具体
Provider adapter；它一次接收当前 instruction/policy、只读 retained causal replay、按此前 ToolCall
provider ordinal 排好的全部 staged ToolOutput、新的完整 Component projection、独立且可丢弃的
Projection Diff Memo、native tool declarations、execution scope，以及编码和 request limit/config。
旧 replay 只读；ToolOutput、projection 和 instruction/policy 变化共同构成一个新的 submission；
memo 只是 compiler state，model output/Event 不属于 Gate。

GateReady 必须拥有同一份 exact immutable wire snapshot、对应的新 causal submission candidate、memo
candidate、exact staged-output consumption receipt 和 observer receipt。closure、ordinal、scope、epoch、
native declaration、encoding 和大小限制全部在 handoff 前针对 exact wire snapshot 校验。lowering、
reconcile、closure、encoding、request-build 或 body-limit 的本地失败不得推进 causal history、memo 或
staging，也不得发送 HTTP。

request 交给 transport 的瞬间，Gate 原子追加新的 input submission、安装对应 complete-projection
memo 并消费 exact staged receipt；retained replay 不会再次追加。此后 HTTP、SSE、timeout、取消或
业务失败都不能回滚它们。`ProviderPort::execute` 返回的 Err 只表示这个 handoff 前的 setup/gate
failure；handoff 后的 reqwest await、HTTP status、content-type 或 transport failure 作为 stream 的
第一个 Err 出现。因此 Runtime 只在 execute 成功返回 stream 后观察一次 `InputSubmitted`，并且该
观察先于任何 ProviderEvent 或 stream error。

Gate 必须检查 exact wire snapshot：每个 pending ToolCall 有唯一 ToolOutput，call/result identity
匹配，结果按 ToolCall ordinal 排列，且全部 output 在任何无关 projection input 前。Fresh、retained
和 retry 沿用同一检查；不能借由 Fresh、丢 continuation 或 context 切换绕过 closure。

### Projection Diff Memo

Projection Diff Memo 是 causal history 之外的 Provider 私有状态。它的 retained value 是
`ProjectionDiffState`、语义 sidecar 和可供下一轮 lowering 使用的 complete projection baseline；
不是模型 history，也不是 response EOF 的 commit 标志。

Memo 的 key/scope 包含 provider history epoch、ComponentHost instance、mount generation、
ComponentId path、node-local structural path 和 diff slot。render generation 只标识一次 candidate，
不构成 baseline key。prepare 仅生成 candidate；只有同一 Input Gate handoff 才 advance。memo 对
history epoch 单向依赖：history epoch 不匹配、memo 缺失或损坏时，只 invalidate memo 并在下一轮
发送 full projection，保留并重放 causal history。普通 props update 保留当前 Component mount 和
memo scope，使下一次完整 render 可以相对上一 baseline lower；Component remount 必然改变 memo
scope，因此新 mount 的首次 render 发送 full。

## 6. Event 调度

普通 Text Event 保持串行：按 ProviderEvent ordinal 消费，一个 Event 的匹配 handlers 按
Component 结构顺序逐个 `await`。

ToolCall 使用独立的 per-lane 调度：

- lane key 是 `call_id`；
- ToolCall 只有在完整 item 校验并写入 local history 后才进入 lane；
- handler future 启动后，Provider event pump 立即继续读取下一个 Event；
- 不同 `call_id` 的 lanes 可以并发；
- 同一 `call_id` lane 内严格保序；
- Provider history mutation 不并发，全部经过一个串行 sequencer；
- lane result 先按 `call_id` 隔离保存，不能改变 Provider output item 的原始 ordinal；
- Provider EOF 后，Runtime 必须等待所有 lanes 完成，才能结束 reaction；
- 下一次模型请求必须回答当前所有 pending ToolCall，不能先提交无关的新回合。

`parallel_tool_calls` 只表示模型是否可以在同一 response 中产生多个 call。它不改变上述闭合规则，
也不把普通 Event dispatch 变成并发。

一个 response 中的多个 ToolCall 不需要“马上”逐个闭合；它们可以并发执行。但下一次提交给
Responses API 的请求中，每个 ToolCall 都必须存在唯一的对应 ToolOutput。这个回答不能推迟到
更后面的回合。

ToolCall lane 在完整 item done 后即可启动，不等待 `response.completed`；event pump 同时继续读取
后续 Event。这是明确的低延迟选择，不代表整轮 response 已完成。若后续断流或终态校验失败，
已经 item.done 的 ToolCall 和已经产生的 ToolOutput 仍然保留；Engine 也不会假装能够撤销已经
完成的工具副作用。因此普通工具语义是 at-least-once；需要 exactly-once 的业务必须使用稳定
幂等键或 effect journal。

## 7. 应用编排与 AgentLoop

`ApplicationHost` 只执行一次 reaction；`AgentLoop` 是它上面的长期应用编排层。一个
`AgentLoop` 长期持有同一组：

- `ComponentHost`，包括 root Component、props 和 retained Signal state；
- `ApplicationHost` 及其 `ProviderPort`；
- mounted Component task registry；
- 单调递增、不会丢通知的 wake epoch。

`AgentLoop` 根据 Component 在本轮给出的 disposition，决定清理完成后立即开始下一次 reaction，
还是等待 wake。它不包含业务 reducer，也不把自身或 `ApplicationHost` 暴露给 Component。

Loop policy 的语义 owner 必须是 mounted Component tree。Component 通过 `use_loop()` 决定正常
流程是立即继续还是等待，并通过 `use_task()` 持有的 wake capability 请求从等待中继续；
`AgentLoop` 只机械地持有 Host、task registry 和 wake epoch，并在安全边界执行这些决定。它不得从
Signal dirty、Provider EOF、CLI 读取或某个 command 名称自行推断下一轮 reaction。

```text
AgentLoop
  | owns
  +-- ComponentHost -- root Component + Signal state
  +-- ApplicationHost -- one reaction at a time
  |     +-- ProviderPort
  +-- Component task registry
  +-- wake epoch

reaction:
  render complete projection
    -> ApplicationHost dispatch
    -> ProviderEvents -> Component handlers
    -> cleanup
    -> continue_now | continue_on_wake
```

### 唯一的 Component 入口

应用的前端定义只有一个普通 root Component：

```rust
#[component]
fn chess_agent(props: ChessAgentProps) -> Component {
    // use_signal, use_provider_event_handler, use_loop and use_task
}
```

root 不接收 `EventInput`，不返回特殊的 completion/program 类型，也不需要
`ChessApplication`、`ComponentAgent` 或 `ApplicationReducer` trait。业务 reducer 是应用自己的
普通函数，由 Event handler 调用并把新状态写入 Signal。应用作者也不需要直接编排
`ApplicationHost` 或 `ComponentReactionRuntime`；这些属于 Engine 和低层显式 reaction API。

每次 reaction 面向 Provider 的结果仍然是完整 `RenderedProjection`。Engine 可以跳过 clean
Component 的重复执行并复用 retained fragment，也可以在整棵 tree 都 clean 时复用缓存结果；这只
是 render 优化，绝不能把 partial projection 交给 ProviderPort。Signal dirty 表示 projection 需要
重算，不代表 AgentLoop 应该自动开始下一次 Provider reaction。

### Skill / CLI frontend

Skill 模式没有框架保留的 `observe` 或通用 `act` subcommand。裸调用 CLI 只返回最新 committed
rendering；其他操作是 mounted Component 定义的 subcommands：

```text
agentview                       -> latest committed rendering
agentview <subcommand> <args>   -> Component-defined operation, then latest committed rendering
```

读取 latest、调用 subcommand 和返回 CLI output 本身都不驱动 render 或下一次 reaction。subcommand
handler 可以更新 Signal、提交 task 或显式使用 `use_loop()`；正常循环是否推进仍由 Component 决定。
subcommand 的具体声明语法、参数 schema 和调度形式留到 frontend command API 单独设计。

### `use_provider_event_handler()`

Provider Event handler 在 `view!` 外声明。`view!` 只描述交给 ProviderPort 的完整 projection，不包含
listener node：

```rust
#[component]
fn chess_agent(props: ChessAgentProps) -> Component {
    let ChessAgentProps { initial_state, board } = props;
    let state = use_signal(move || initial_state);
    let event_state = state.clone();

    use_provider_event_handler(
        ProviderEvent::TEXT,
        move |event| {
            let state = event_state.clone();
            async move {
                state.set(reduce(event))
            }
        },
    );

    view! {
        chess_board(board)
    }
}
```

selector 决定 callback 接收的具体 typed Event。listener 可能收到多个 Event，因此参数不是一个已经
创建好的 Future，而是每次调用都创建新 Future 的 callback。概念签名是：

```rust
pub fn use_provider_event_handler<Event, Handler, HandlerFuture, Error>(
    selector: ProviderEventSelector<Event>,
    handler: Handler,
)
where
    Event: Clone + Send + Sync + 'static,
    Handler: FnMut(Event) -> HandlerFuture + Send + 'static,
    HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: Display + Send + 'static;
```

`Future` 是异步执行形式，`Result` 是 Future 的输出；二者不是互斥选择。Runtime 必须完整 `await`
handler Future，并把 returned error 或 panic 作为当前 reaction 的 handler fault。

handler registration 和一次 reaction 的 dispatch binding 是两层不同生命周期：

```text
MountedProviderEventHandler
  identity = ComponentId + HookSite + MountGeneration
  selector + current callback
  retained across reactions
            |
            | bind for each render/reaction generation
            v
ReactionProviderEventBinding
  dispatches only the current ProviderEvent stream
  dropped when the reaction completes or is cancelled
```

successful rerender 原子更新 mounted slot 中的 callback capture；失败或 abandoned render 丢弃
candidate，继续保留上一版 callback。clean Component 没有重复执行时，mounted slot 仍可为下一轮
生成 binding。Component 从 tree 中卸载或显式 remount 时删除 slot；旧 binding 和旧 callback 不得
进入新 mount。

同一个 Provider Event 匹配的 handlers 按 Component 结构顺序和 HookSite 顺序串行 `await`。
Provider Event 是 typed multicast，不引入 DOM `ElementId`、bubbling 或 capture。

公共 authoring API 不暴露 `EventInput`、`EventListener::observe(...)`、`listen_to(...)` 或用户填写的
listener identity/version。旧 API 可以暂时保留给 streaming XML 和低层兼容路径，等 streaming
listener 单独设计后再移除。

`#[component]` proc macro 直接识别调用并分配静态 HookSite：

```text
use_provider_event_handler(selector, callback)
  -> HookRenderContext::use_provider_event_handler_at(site, selector, callback)
  -> render transaction stages a mounted-slot candidate
  -> successful commit updates the mounted handler registry
  -> ComponentHost derives the current generation's RenderBindings
```

实现复用现有 typed `AsyncHandler` 的 Future/error/panic 擦除和 dispatch 逻辑，但不要求先公开一个
通用 `use_hook<T>`。通用 hook kernel 可以以后与 `use_task` 一起评估，不属于本 API 的前置条件。

### `use_loop()`

Component 用一个窄 handle 决定当前 reaction 之后如何推进：

```rust
let loop_control = use_loop();

loop_control.continue_now();
loop_control.continue_on_wake();
```

`continue_now()` 表示当前 reaction 的 stream、handlers、ToolCall lanes 和 cleanup 全部结束后，立即
开始下一次 reaction；它不允许在当前 handler 内重入 render 或 Provider。

`continue_on_wake()` 表示 cleanup 后停止推进，直到 wake epoch 超过本轮开始时观察到的 epoch。
Signal 写入本身不满足这个条件。Component 获得的是 decision handle，不是 `AgentLoop`、
`ApplicationHost` 或一个可任意操作 Host 生命周期的引用。

### `use_task()` 与显式 wake

`use_task()` 提交由 mounted Component 拥有的后台 future。实际提交是 committed handler 中发生的
effect，不是 render 本身的副作用：

```rust
let loop_control = use_loop();
let task = use_task();

use_provider_event_handler(ProviderEvent::TEXT, move |event| {
    let task = task.clone();
    let state = state.clone();
    let loop_control = loop_control.clone();

    async move {
        task.submit(move |wake| async move {
            let next = run_background_work(event).await;
            state.set(next)?;
            wake.wake();
            Ok(())
        })?;

        loop_control.continue_on_wake();
        Ok(())
    }
});
```

Runtime 为每个 submitted task 提供权限受限的 `TaskWakeHandle`。task 可以在完成前、完成时或长期
sidecar 的多个进度点调用 `wake.wake()`。`wake()` 只推进所属 Component mount 的 wake epoch；它
不修改 Signal、不直接 render，也不重入 Provider。task 要把业务结果写入 projection，仍然必须
显式更新 Signal。

task completion 不隐式 wake。这样，不影响 Agent 推进的 maintenance task 可以安静结束，长期
sidecar 也能精确选择哪些状态变化需要一次新 reaction。

AgentLoop 在每轮开始时记录 observed epoch。若 task 在 Loop 真正进入等待前调用 `wake()`，本轮
结束时已经能观察到更大的 epoch，因此立即开始下一轮；若 `wake()` 发生在等待后，watch 通知唤醒
Loop。多个尚未观察的 wake 可以合并为一次后续 reaction；wake 是状态变化通知，不是任务队列。

`use_task` 的 task 属于 Component mount，而不是某一次 Provider reaction。Component 从 tree 中
卸载、显式 remount 或 AgentLoop 停止时，Runtime 取消对应 tasks 并使旧 `TaskWakeHandle` 失效；
旧 task 或旧 wake handle 不能唤醒新的 mount。

## 8. Unsupported ToolCall

正常情况下，Provider request 只暴露当前 Component tree 声明的工具能力。即使 Provider 返回了
没有可用 Component 的 ToolCall，Engine 也不能伪造 ToolOutput，更不能发送包含裸 ToolCall 的
下一次请求。

处理规则是：

1. 完整 ToolCall 仍先进入 local history，并登记为 pending；
2. Runtime 查找匹配 ToolCall Component；
3. 模型调用了未声明工具时返回 `unsupported_tool_call`；
4. 工具已经声明但本轮没有绑定 handler 时返回 `tool_binding_missing`；
5. handler 正常返回但没有产生 ToolOutput 时返回 `tool_output_missing`；
6. lane panic、取消或执行设施失败时返回对应的 `tool_lane_failure`；
7. fault 发生后，ProviderPort 不发起下一次 HTTP 请求；当前 open history 不能作为 wire input；
8. pending ToolCall 不得通过丢弃 continuation、Fresh、插入其他 input 或开始无关回合来绕过；
9. 若该 call 无法被 Component 回答，本次 Provider session 终止，不能继续调用模型。

工具执行失败若属于可表达的业务结果，应由 ToolCall Component 产生明确的失败 ToolOutput；
基础设施失败才中止 reaction。

## 9. 取消、失败与 remount

取消 reaction 时：

- drop Provider stream；
- 取消 Runtime 拥有的 pending lane futures；
- 保留已经提交的 input、已经 item.done 的模型 items 和已经组装的 ToolOutputs；
- 将尚未 item.done 的 inflight items 标记为 aborted；
- 保留取消前已经成功的 Signal 写入；
- 不自动发起重试或下一次模型调用。

Provider fault、Component handler fault、lane panic 和 terminal handler fault 都必须带明确 stage 和
reason。清理完成是终止条件的一部分；Provider stream、handler future 和 ToolCall lane 等
reaction-owned 工作不能遗留到下一次 reaction。`use_task` 提交的工作由 Component mount 拥有，
不因一次 reaction 正常结束而取消；若 fault 导致 AgentLoop 停止，则随 Loop 一起取消。

Component remount 会创建新的 runtime identity。旧 node 的 Provider history 不撤回；新 identity
作为新的 projection node 参与后续 reconciliation。旧 mount 的 handler、lane 和 Signal handle
不得写入新 mount；旧 mount 的 tasks 必须取消，旧 `TaskWakeHandle` 不得唤醒新 mount。

## 10. Observability

Observer 观察与 Engine 相同的因果顺序，但不拥有或修改状态。至少应能记录：

- reaction identity；
- ProviderEvent ordinal；
- lane / `call_id`；
- input submitted、partial committed、item sealed/aborted；
- handler started/completed、ToolCall pending/closed；
- wire snapshot accepted 或 blocked；
- terminal stage、reason 和 cleanup outcome。

Event 不得先被 Observer 或 Component 看见、之后才写 local history。日志中的
`response.completed` 只能表示 Provider terminal frame 已校验，不能把它写成整个 history 的
commit。

## 11. 必须保持的不变量

1. Component state 是业务权威；Provider history 是私有、可丢弃的 execution context。
2. 公共 ProviderPort 只接收完整 projection，只输出 provider-neutral Event stream。
3. 每个 partial ProviderEvent 发布前，对应 partial record 已经提交到 local history。
4. local history 可以暂时 open；wire snapshot 必须始终可以直接发送给具体 Provider。
5. 任何下一次 Responses 请求都必须回答当前全部 pending ToolCall；不得把回答推迟到后续回合。
6. 不同 ToolCall lanes 可以并发；同一 lane 和 history mutation 必须保序。
7. Provider EOF 不等于 reaction 完成；所有 lanes、terminal handlers 和必要输出都必须完成。
8. input 在 request submission 时推进；model output 的每个 visible partial 在 Event 发布前推进；
   item done 只负责 sealed；ToolOutput 只在下一次 Input Gate handoff 的有序 submission 中推进。
9. `response.completed`、normal EOF 和 Component reaction success 都不是 history commit gate。
10. timeout、断流、取消和业务失败不回滚已经推进的 local history。
11. ApplicationHost 不回滚已成功的 Component state 或外部副作用。
12. 同一次 reaction 不自动 rerender，也不自动再次调用 Provider。
13. Provider-specific response id、wire item 和 cache state 不得泄漏到 Component authoring API。
14. AgentLoop 只根据 Component 显式给出的 `continue_now`，或 `continue_on_wake` 后观察到的新 wake，
    推进下一次 reaction。
15. Signal dirty、reaction disposition 和 wake epoch 是三个独立状态；任何一个都不能冒充另外两个。
16. task completion 不隐式 wake；只有有效 `TaskWakeHandle::wake` 推进对应 mount 的 wake epoch。
17. 每次交给 ProviderPort 的 projection 都是完整 projection；dirty tracking 只能用于内部 render 优化。
18. Provider Event handler slot 属于 Component mount；每轮 dispatch binding 只属于当前 reaction generation。
19. Skill / CLI 读取 latest 和调用 Component subcommand 都不隐式 render，也不拥有正常 loop policy。

## 12. 非目标

本文不规定：

- Chess、裁判或其他业务领域规则；
- 某个 ToolCall 的业务 schema 和执行实现；
- LiteLLM 的内部 cache/session 命名；
- UI 展示格式、JSONL 字段全集或测试计划；
- 实现顺序、迁移进度和发布里程碑。
