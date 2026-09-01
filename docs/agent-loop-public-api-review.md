# AgentLoop Public API Change Review

日期：2026-08-27

状态：**已被 [`frame-driven-runtime-plan.md`](frame-driven-runtime-plan.md) 取代，不再是实现或 public API
来源。**

其中关于 declarative provider event handler和Component-scoped async lifecycle的研究输入仍可参考；
framework-owned `AgentLoop`、`use_loop`、Continue/Sleep/Stop和universal wake epoch已经被否决。当前权威
contract见 [`engine.md`](engine.md)。

评审基线：`c6de13b95edf2e599152e03392162d1b67e66d12`

权威运行时语义见 [`engine.md`](engine.md#7-应用编排与-agentloop)。本文只把拟议的
public API 变化整理成一份可直接审查的 contract packet。评审通过后，结论应回写
`engine.md`，然后才能生成 implementation plan。

两轮专项 review 的 correctness findings 已直接合并到正文。reviewer 建议的 hook/task
全量改名没有采纳；`use_loop`、`use_task`、`submit` 和 `wake.wake()` 保持已确定的
authoring vocabulary。

## 1. 评审目标

本次变化提供一个长期运行的应用编排层，使应用作者只需要定义一个普通 root Component：

```rust
#[component]
fn chess_agent() -> Component {
    // use_signal, use_provider_event_handler, use_loop, use_task
}
```

应用作者不再需要为高层 Agent 应用直接编排：

- `ComponentHost`；
- `ApplicationHost`；
- `ComponentReactionRuntime`；
- `ComponentReactionProps<Props, Output>`；
- root 参数中的 `EventInput<ProviderEvent>`；
- `view!` 内的 `EventListener::observe(identity, version)`。

低层的显式单次 reaction API 仍然保留。此次变化不是删除所有 execution API，而是新增一条
推荐的 high-level application path。

## 2. 已冻结的架构决定

以下内容已经在 `engine.md` 中确定，不属于本轮命名投票：

1. Component tree 是 loop policy 的语义 owner。
2. `AgentLoop` 只机械持有长期 Host、task registry 和 sticky wake epoch。
3. `ApplicationHost` 仍然只执行一次完整 reaction；它不拥有循环策略。
4. `ProviderPort::execute` 每次仍接收完整 `RenderedProjection`。
5. Signal dirty、reaction disposition 和 explicit wake epoch 是三个独立状态。
6. Signal 写入不会自动推进 AgentLoop。
7. task completion 不会自动 wake；只有显式 `TaskWakeHandle::wake()` 才会 wake。
8. 每次有效 `wake()` 都推进 sticky wake epoch，并保证在调用线性化之后
   至少启动一个 turn；调用时 AgentLoop 是否已 sleep 不影响该保证。
9. `view!` 只描述 Provider projection，不包含普通 Provider Event listener node。
10. CLI latest read、Component subcommand 和 Provider EOF 都不会自行驱动下一次 reaction。
11. Continue 是每轮的默认，不提供 `continue_now()`。Component 只显式请求 Sleep
    或 Stop，并按 `Stop > Sleep > Continue` 单调合并；结构顺序不产生权级。

## 3. 当前 API

当前高层示例实际需要同时理解 authoring 和 execution 两层：

```rust
#[component]
fn application_root(
    props: ComponentReactionProps<AppProps, AppOutput>,
    events: EventInput<ProviderEvent>,
) -> Component {
    let output = use_signal(AppOutput::initial);
    props
        .publish(output.clone())
        .expect("mounted output publication");
    let text = events.select(ProviderEvent::TEXT);

    view! {
        prompt { "..." }
        {
            EventListener::observe("app.answer", "v1")
                .listen_to(text)
                .on_event(move |event| async move {
                    output.set(reduce(event))
                })
        }
    }
}

let mut runtime = ComponentReactionRuntime::new(provider, application_root, props);
let output = runtime.dispatch_llm_reaction().await?;
```

这条路径的问题不是 domain code 太多，而是 framework plumbing 出现在了 Component 的公共入口：

- 特殊 props wrapper 只为 publish typed output；
- `EventInput` 从 root 向子 Component 手工传递；
- listener identity/version 由应用作者维护；
- listener 是 `view!` projection tree 中的一个无 prompt node；
- 外层业务代码手工决定每一次 reaction 何时再次执行。

## 4. 候选 high-level 用法

下面是供评审的目标形状。名字和细节仍可被 reviewer 否决，但所有权方向不变。

```rust
use agentview::component::prelude::*;
use agentview::launch;

#[component]
fn chess_agent() -> Component {
    let state = use_signal(ChessState::initial);
    let loop_control = use_loop();
    let tasks = use_task();
    let rendered_state = state
        .with(Clone::clone)
        .expect("mounted state is readable during render");

    // AgentLoop 默认立即继续；该 Component 显式选择等待 wake。
    loop_control.continue_on_wake();

    use_provider_event_handler(ProviderEvent::TEXT, {
        let state = state.clone();
        let loop_control = loop_control.clone();
        let tasks = tasks.clone();

        move |event| {
            let state = state.clone();
            let loop_control = loop_control.clone();
            let tasks = tasks.clone();

            async move {
                let TextTurnEvent::TextComplete(output) = event else {
                    return Ok::<(), String>(());
                };
                let next = reduce(output).map_err(|error| error.to_string())?;
                state.set(next).map_err(|error| error.to_string())?;

                tasks.submit(move |wake| async move {
                    let next = run_background_work()
                        .await
                        .map_err(|error| error.to_string())?;
                    state.set(next).map_err(|error| error.to_string())?;
                    wake.wake();
                    Ok::<(), String>(())
                })
                .map_err(|error| error.to_string())?;

                loop_control.continue_on_wake();
                Ok::<(), String>(())
            }
        }
    });

    view! {
        chess_board(rendered_state)
    }
}

launch(provider, chess_agent).await?;
```

这个入口不返回 `Completion`、`GameResult` 或任意 framework-defined typed result。需要逐轮 typed
output 的 embedding 继续使用 `ComponentReactionRuntime`；迁移后的自主应用应把业务权威状态和
reducer 放进 Component tree。

autonomous root renderer 没有 `Props`：它上方没有 parent，也没有运行期间更新 root props 的来源。
启动配置由无参数 closure 捕获，或者由该 root 传给普通子 Component。普通 Component props，以及
允许外部调用 `set_props()` 的 low-level `ComponentHost<Props>`，都继续保留。

## 5. 候选 public surface

以下签名是 review target，不是实现承诺：

```rust
pub async fn launch<P>(
    provider: P,
    root: impl Fn() -> Component + Send + 'static,
) -> Result<(), ApplicationHostFault>
where
    P: ProviderPort;

pub fn use_provider_event_handler<Event, Handler, HandlerFuture, Error>(
    selector: ProviderEventSelector<Event>,
    handler: Handler,
)
where
    Event: Clone + Send + Sync + 'static,
    Handler: FnMut(Event) -> HandlerFuture + Send + 'static,
    HandlerFuture: Future<Output = Result<(), Error>> + Send + 'static,
    Error: Display + Send + 'static;

pub type ProviderEventSelector<Event> = EventSelector<ProviderEvent, Event>;

pub fn use_loop() -> LoopControl;

#[derive(Clone)]
pub struct LoopControl { /* private */ }

impl LoopControl {
    pub fn stop(&self);
    pub fn continue_on_wake(&self);
}

pub fn use_task() -> TaskSubmitter;

#[derive(Clone)]
pub struct TaskSubmitter { /* private */ }

impl TaskSubmitter {
    // `submit` and TaskWakeHandle are confirmed. The typed task-result delivery
    // shape is the next API decision and is intentionally not frozen here.
}

pub struct TaskWakeHandle { /* private */ }

impl TaskWakeHandle {
    pub fn wake(&self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TaskWakeObservation {
    Applied,
    StaleMount,
    LoopStopping,
}

// Added to the existing non-exhaustive EngineObservation enum:
// TaskWake {
//     component: String,
//     hook_site: u32,
//     outcome: TaskWakeObservation,
// }

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TaskSubmitError {
    #[error("task submission is only valid in a committed effect scope")]
    OutsideCommittedEffect,
    #[error("task owner is no longer mounted")]
    StaleMount,
    #[error("AgentLoop is stopping")]
    LoopStopping,
}
```

`launch` 不增加一层 speculative fault taxonomy，直接返回现有 `ApplicationHostFault`。
其中任何 public callback fault 必须继续以 typed data 暴露：

```text
stage        = ProviderEvent | NativeTool | StreamingDecoded |
               StreamingInvalid | StreamingTerminal | FrontendCommand
component    = stable runtime component identity/path
hook_site    = stable HookSite
kind         = Returned | Panicked | Binding
source       = typed source when available, otherwise display message
```

`launch` 不能把这些上下文重新擦除成单一 message。submitted task panic 不属于这个可恢复
fault surface；它按第 7 节恢复 unwind。

候选 export policy：

- `launch` 是应用唯一的 high-level execution entry，并从 crate root 导出；`AgentLoop` 是其背后的
  internal mechanical runtime，不提供另一套 public builder API；
- `ApplicationHostFault` 沿用现有 module path，并由 crate root re-export 供 `launch` 调用者使用；
- `use_provider_event_handler`、`use_loop` 和 `use_task` 从 `component::prelude` 可用；
- 返回的 handle 和 fault types 在 `component::authoring` 下可命名，但不要求加入 glob prelude；
- concrete implementation 可以位于 `component::execution` 与 `component::authoring`；
- `ApplicationHost`、`ComponentHost`、`ComponentReactionRuntime` 继续通过原 module path 提供，
  但不成为 high-level 示例的入口；
- `ProviderEventSelector<Event>` 是现有
  `EventSelector<ProviderEvent, Event>` 的 public type alias，不是新 newtype。这保证
  `ProviderEvent::TEXT` 等现有常量可直接传入，不需要转换，也不破坏现有
  selector 的 source compatibility。应用仍不手工构造 selector。

## 6. Loop contract

已确认的 Loop flow 核心是一个只能向终止方向提升的三态决定：

```text
Continue (default) < Sleep < Stop
```

具体规则：

- 每轮只有一个 reaction-global disposition，开始时重置为 `Continue`。框架不提供
  `continue_now()` 或其他显式 Continue effect。
- 任意 mounted Component 都可以取得 `LoopControl` 并更新这个全局决定。
- `continue_on_wake()` 执行 `disposition = max(disposition, Sleep)`。
- `stop()` 执行 `disposition = max(disposition, Stop)`。
- 同一 Component、多个 Components 和并发 callbacks 都共享这个单调聚合；重复调用幂等。
- parent、child、HookSite 编号和调用顺序都不产生优先级。
- Sleep 或 Stop 一旦提出，本轮不能被另一个 Component 降回 Continue。
- callback 中的请求只属于当前 reaction，在下一轮重置。render 中的请求作为
  committed render effect 保留；Runtime 在 clean Component 被跳过时重放它，使渲染优化
  不改变 Loop 语义。successful rerender 替换该 effect，unmount/remount 清除旧 effect。

在这个简化 API 下，以下 Runtime 边界不变：

- disposition 只在 Provider stream、普通 handlers、ToolCall lanes、EOF diagnostic handlers 和 cleanup
  全部结束后采样；任何调用都不能重入当前 reaction。
- `LoopControl` 是 cloneable mount handle，但不是一个永远指向“当前 reaction”的全局开关。
  它只能通过 Runtime 安装的当前 execution scope 影响允许的 Loop-flow state。
- Runtime 必须为所有 framework-dispatched user callback 安装当前 reaction execution scope，包括
  Provider Event handlers、native tool callback 及其 Future、streaming decoded/invalid
  handlers 以及未来 frontend commands。不能只让普通 Provider Event handler 获得有效 token。
- mounted callback 可以跨 clean reactions 复用 capture。每次调用时由 reaction binding 安装新的
  execution scope，因此 capture 中的 `LoopControl` 不保存旧 reaction id，也不能直接取得未来
  reaction 的 disposition accumulator。
- 在 submitted task、任意 detached future 或 reaction cleanup 之后调用 `LoopControl` 不能修改
  当前或下一轮 disposition；Runtime 将其 fence 为 no-op，并可写入 internal diagnostic。后台 task
  只能使用 `TaskWakeHandle` 请求推进。这不是 `launch` 调用者可以恢复的 public fault。
- 取消 reaction 时必须先 close reaction token，再 drop/cancel user futures。这样取消路径中的
  stale handle 不会在有效 scope 关闭之后继续写 disposition。
- 每个 turn 开始时 snapshot 当前 wake epoch。这个 turn 满足 snapshot 之前的所有
  wake requests；turn 开始后的新 `wake()` 必须由至少一个更晚的 turn 满足。
- 这里的一个 turn 指 AgentLoop 启动的一次完整 Provider reaction，包括调用
  `ApplicationHost::dispatch_llm_reaction` / `ProviderPort::execute`；只唤醒 async task 但不开始
  Provider reaction 不算满足 `wake()`。
- reaction cleanup 后的正常调度顺序是：`Stop > pending Wake > Sleep > Continue`。
  pending Wake 与 disposition 正交；它能让 Sleep 后立即再运行一轮，但不能复活 Stop。
- pending Wake 必须在任何下一个 turn 开始前被检查，而不是只在 Sleep 分支检查。
  当 disposition 本来就是 Continue 时，自然开始的下一个 turn 满足该 wake request。
- Runtime 必须先 arm/enable wake waiter 和 supervisor-fault waiter，然后才做最后一次
  epoch/fault 读取。epoch 已推进则记录 pending Wake，supervisor 已 fault 则立即终止；
  只有最终决定仍是 Sleep 且没有 pending Wake/fault 时才真正 await。先读再注册
  waiter 存在 lost-wake 窗口，禁止这种实现。
- supervisor fault 不只在 sleep 时检查。AgentLoop 必须让它与 render/preflight、Provider
  setup 及 Input Gate、stream dispatch、lane draining、terminal/cleanup transition 和 wake waiting 全部
  参与 race；一旦 fault，关闭 token、取消当前 reaction，完成回收后终止。
- 所有从一个 phase 向下一个 phase 的 forward transition，包括开始下一轮之前，
  都必须在 supervisor waiter 已 arm 后做最终 terminal-cause load。如果 progress 和 fault 同时
  ready，fault 必须胜出，禁止再调用一次 `ProviderPort::execute`。不能把这个契约仅依赖
  `select!` 的随机 branch 顺序。
- Runtime terminal failure 优先于任何正常 Loop-flow 结果。task panic 和 structured fault 的
  详细仲裁见第 7 节。
- 多个在同一个后续 turn 开始前线性化的 wake 可以由该 turn 一起满足；
  wake 不是“一次调用对应一个 turn”的计数队列。

## 7. Task and wake contract

- `use_task()` 返回当前 mounted Component 的 task submission capability。
- `submit()` 只能作为 committed handler 或未来 frontend command 的 effect；render 期间不能真正
  spawn task。
- v1 的每次 `submit()` 都创建一个匿名 task；不提供 key、replace、restart policy 或 public task id。
- task 属于 Component mount，不属于某次 reaction，因此正常 reaction 结束不会取消它。
- Component unmount、explicit remount、AgentLoop stop/fault 会取消 task；只 drop `JoinHandle` 不算
  cancellation。
- `TaskWakeHandle` 属于产生它的 mount。有效 `wake()` 不只是 notify waiter；它提交
  sticky next-turn request，并保证在调用线性化后至少启动一个 turn。
- mount 失效后 `wake()` 是 fenced no-op，绝不能代表新 mount 发出请求。mount 失效检查
  和 wake epoch increment 共用一个线性化点：若 wake 先成功，已接受的 next-turn
  request 不会被后续 unmount/remount 擦除；若 unmount/remount 先生效，旧 handle
  调用才是 no-op。
- 只有 AgentLoop Stop/fault 或调用者取消能在已接受 wake 后终止 next-turn
  guarantee。因 unmount 而取消 task 不会擦除该 task 在取消前已成功提交的 wake。
- mount validity check 与 epoch increment 必须是一个线性化操作，避免 stale-wake race。
- task 可以多次 wake；每次有效 wake 都推进 epoch。同一个后续 turn 开始前的
  多次 wake 可以由该 turn 共同满足；turn 开始后的 wake 要求再有一个更晚的 turn。
- task 成功完成只从 registry 移除，不隐式 wake。
- task 若要让新业务状态出现在 projection，必须先显式写 Signal，再显式 wake。
- task Future 输出 typed `Result<T, E>`。Runtime 只执行、取消和 mount-fence task，并把完成结果
  交还 owning Component；`Err(E)` 不升级为 `launch` fault。Component reducer 决定如何更新 state，
  以及随后 `wake()`、等待还是停止。completion callback 与 typed task handle 两种投递形状尚待下一项
  API 决定，不能在这里假定。
- submitted task 不允许把 panic 当作可恢复业务错误。task panic 会 poison 整个 AgentLoop：
  Runtime 先关闭 reaction token、取消 active reaction、abort 并 await sibling tasks，然后
  `resume_unwind` 原 panic payload。它不转换为 public error，因此也不能被应用当成正常 `Err`
  后继续运行。
- 只要 `launch()` future 仍在被驱动，回收期间发现的任何 task panic 都高于已 latch 的
  `ApplicationHostFault` 或显式 `stop()`。
- 多个 task panic 时，supervisor 第一个线性化的 panic payload 是 primary payload；其他
  panic 记入 observer diagnostic，不替换 primary payload，也不得中断对剩余 handles 的
  abort + await。回收全部完成后只 `resume_unwind` primary payload。
- task panic 后 AgentLoop 停止，主动 abort 并 await 其他 registered tasks，再恢复 panic。
  Tokio `JoinHandle` 在 drop 时会 detach；如果不 abort，task 会在 Component
  已 unmount 或 Loop 已返回后继续写 Signal、调用外部副作用或尝试 wake。`await`
  aborted handle 用于确认 Future 已经被 drop 并收集终止结果；它不暗示框架能够执行
  任意 async cleanup。
- 正常 `stop()` 和 reaction fault 也主动 abort 并 await tasks。直接 drop `launch()` future 时 Rust
  `Drop` 无法 async await，但 owner 必须同步 abort 所有 handles，不能 detach 它们。这一
  drop contract 只承诺发出同步 cancellation request，不承诺在 drop 返回前完成 async join，
  也不承诺传播与外部 cancellation 并发的 task panic。如果调用者需要 terminal fault/panic，
  必须继续 poll/await `launch()` 到终态。
- `wake()` 保持 unit-returning API。每次调用必须在 observer 上产生线性化后的
  `Applied | StaleMount | LoopStopping` diagnostic，使 correctness tests 可以区分有效 wake
  和 fenced no-op，而不把诊断结果加到 authoring control flow。observer sink 属于 internal/diagnostic
  execution 配置，不形成第二套应用编排入口。这项 observer delivery 承诺只在 AgentLoop 的
  diagnostic sink 仍存活时成立；
  Loop 已完全 drop 后才调用的 stale handle 仍必须 no-op，但无法再向已销毁的 observer
  delivery channel 发送事件。

## 8. Provider Event handler contract

- `use_provider_event_handler(...)` 写在 `view!` 外。
- selector 决定 callback 的 typed `Event`。
- callback 是 `FnMut(Event) -> Future<Output = Result<(), Error>>`，不是提前创建的 Future。
- Runtime 完整 await callback；returned error 和 panic 都属于当前 reaction fault。
- registration identity 是 `ComponentId + HookSite + MountGeneration`，应用不提供字符串 identity 或
  implementation version。
- mounted handler slot 跨 reactions 保留；每次 reaction 生成独立 dispatch binding。
- successful render 原子替换 callback capture；failed/abandoned render 保留上一版。
- 同一 Provider Event 的匹配 handlers 按 Component structure order 和 HookSite order 串行 await。
- Provider Event 是 typed multicast，不加入 DOM bubbling/capture。
- ToolCall lane 继续使用现有专门 consumer，不由普通 Provider Event handler 取代。

`#[component]` macro 需要为 `use_signal`、`use_provider_event_handler`、`use_loop` 和 `use_task`
分配稳定 HookSite。所有 hook kind 共用一个 component-wide lexical sequence，并在 topology 中保存
`(HookSite, HookKind)`；数量、顺序或 kind 改变都必须使 candidate render fail closed。

v1 macro 只承诺识别 direct prelude name 和 canonical full path。alias、用户 re-export 和 conditional
hook 明确不支持；trybuild tests 必须覆盖每种 hook、mixed ordering、canonical qualification、alias
misuse 和 conditional topology change。沿用当前 macro 实现时，任何包含 hook 的 `#[component]`
所有参数都必须实现 `Clone`；这项隐含约束必须进入 public rustdoc 和 compile tests。

一次 successful render 必须使用单一 transaction 处理所有 hook 状态：

```text
stage complete candidate tree
  -> validate runner capabilities
  -> validate (HookSite, HookKind) topology
  -> validate component/mount generations
  -> atomically commit:
       Signal topology and staged writes
       mounted Provider Event handlers
       task-scope ownership/reconciliation
       committed render-time Loop effects
  -> atomically fence removed mounts
  -> abort and await every retired-mount task
  -> only then expose new reaction bindings and enter Provider setup/Input Gate
```

不允许 Signal 先 commit、handler 后失败，也不允许 committed Loop effect 已更新但
task ownership 还属于旧
mount。任何 preflight 或 render 失败都丢弃整个 candidate，继续保留上一个 committed
snapshot。render transaction 只提交 task scope 和 mount reconciliation，不在 render 中 spawn 用户 task；
`submit()` 仍只能在 committed execution scope 中执行。

“原子 commit”不表示在锁内 async await。commit 先以一个线性化步骤安装新 snapshot
并 fence 旧 mount，再在不暴露新 reaction binding 的 transition phase 中 abort + await retired
tasks。旧 handle 在该 phase 已经 stale；新 Provider request 必须等回收完成。回收时发现 task
panic 按第 7 节优先级终止 Loop，不暴露新 binding。

## 9. 兼容性和迁移

候选迁移策略：

1. zero-argument autonomous root renderer 和 legacy two-argument root 都 lower 到一个 private root
   renderer abstraction，并由同一个 `ComponentHost` kernel 执行；不能复制第二套 Signal、mount 或
   render transaction。legacy explicit-reaction path 继续持有自己的 `Props`。
2. `ComponentReactionRuntime` 保留为 supported low-level explicit-reaction embedding API。
3. `EventInput` / `EventListener` 在 additive release 中保留原 module path 和原 prelude re-export，
   供 streaming XML、现有应用和低层代码使用。移除它们必须是另一次明确的 breaking change。
4. `use_provider_event_handler` 在 AgentLoop 和低层 explicit-reaction runner 中都可用。
5. 没有 AgentLoop orchestration capability 时，`use_loop` / `use_task` 必须在 render preflight 返回
   typed `ComponentHostFault::MissingOrchestratorCapability`；不能 panic、no-op 或运行到 handler 才失败。
6. 第一版 AgentLoop 使用一个新的小型 autonomous example 验证 high-level API。现有 Chess 继续作为
   `ComponentReactionRuntime` explicit-reaction example，因为它的 referee、retry、timeout、evidence、
   UCI lifecycle 和 `GameOutcome` 仍由外层业务代码拥有。
7. Chess 完整迁移是后续独立设计：要么把整场 game/referee ownership 移入 root Component，要么明确
   保留为低层 embedding；不能只机械删除 output publication。

Capability matrix：

| Capability | `AgentLoop` | Explicit-reaction runner |
|---|---:|---:|
| `use_signal` | yes | yes |
| `use_provider_event_handler` | yes | yes |
| `use_loop` | yes | typed preflight fault |
| `use_task` | yes | typed preflight fault |
| Legacy `EventInput` / `EventListener` root | compatibility adapter only | yes |

## 10. 已知实现陷阱

这些不是可选优化，而是评审 API 时必须考虑的 correctness constraints：

- 当前 `SignalRuntime::mark_dirty()` 同时推进 `wake_revision`。新 AgentLoop 不能复用这个 revision
  作为 explicit task wake epoch。
- 当前 `ComponentHost` root ABI 固定为
  `fn(Props, EventInput<ProviderEvent>) -> Component`；zero-argument autonomous root 需要共存策略。
- 当前 `RenderBindings` 只活一轮；mounted handler registry 是新的、更长生命周期。
- 当前 proc macro 只重写直接或固定全路径的 `use_signal` 调用；新 hooks 不能只加函数导出。
- retained handler capture 中的 `LoopControl` 必须依赖 runtime-installed reaction execution scope，
  不能保存或查找一个可被 detached task 操作的 mutable "current reaction" pointer。
- Tokio `JoinHandle` drop 会 detach。task registry 在 unmount/stop 时必须主动 abort 并 await cleanup。
- task submit 与 unmount、stale wake 与 remount、disposition write 与 reaction rollover 都需要 generation
  fence，不能用独立的“先检查、再执行”步骤。
- failed render 不能提交新的 handler/task/loop hook candidates，也不能取消上一版仍 mounted 的
  resources。
- waiter 必须在最后一次 epoch/fault load 之前 arm/enable，否则 wake 或 fault 可以落在
  read/subscribe 之间而永久丢失。
- task supervisor fault 必须有独立的 internal notification path，并且能打断 active dispatch、
  lane draining、transition 和 sleep；这个 path 不能冒充 Component-requested wake。
- 必须在 drop/cancel user futures 前 close reaction token，不能让 cancellation 路径继续写已结束
  reaction 的 disposition。
- Signal、handler、task mount reconciliation 和 committed Loop effects 必须在同一 render transaction
  中通过 capability/topology validation 后一起 commit。

## 11. 非目标

本次 API change 不定义：

- Skill/CLI subcommand declaration syntax；
- 通用 `use_hook<T>`；
- task key、supervision tree、retry/backoff 或 durable job queue；
- Component key authoring；
- native tool input schema；
- Provider history、diff baseline 或 compaction 的公共 API；
- frontend latest-rendering wire protocol；
- typed business completion facade。

## 12. 评审结论

两轮专项 review 已完成。已接受的 correctness 要求已直接合并到第 5–10 节；
第 13 节保留可执行的 review worklist，用于逐项确认，不复制原始 reviewer 长文。

当前结论可压缩为三点：

1. authoring surface 保持 `use_provider_event_handler`、`use_loop`、`use_task`、`submit`
   和 `wake.wake()`，`AgentLoop` 从 execution module 显式导入。
2. Component tree 拥有 loop policy；Continue 是每轮默认且没有 public method，任意
   Component 都只能按 `Continue < Sleep < Stop` 提升全局决定。
3. sticky wake、task supervision、mount fencing 和 render transaction 属于 Runtime correctness，
   不增加 Component authoring plumbing。

完成第 13 节中的 API decisions 后，才冻结 contract 并生成 implementation plan。

## 13. Review worklist

本节把 reviewer 问题分成三类。只有 A 组需要逐项做 public API/产品语义决定；B 组是
Runtime correctness，必须用测试证明；C 组是兼容性边界。

### A. 需要逐项确认

| ID | 问题 | 当前候选结论 | 状态 |
|---|---|---|---|
| A1 | Loop flow：Stop/Sleep/Continue 如何产生、合并和 reset | 单一全局决定；默认 Continue；无 `continue_now()`；`Stop > Sleep > Continue` | 已确认 |
| A2 | `wake()` 的调度保证与 stale 边界 | 有效 wake 是 sticky next-turn request，至少保证一个更晚 turn；stale/stopping 被 fence | 已确认 |
| A3 | `launch` 是否需要新的 fault taxonomy | 不新增 wrapper；直接返回现有 `ApplicationHostFault`，control misuse 仅 fence/diagnose | 已确认 |
| A4 | `launch()` future 被 drop 时承诺什么 | 正常终止 abort + await；drop 只同步 abort，不传播并发 panic | 待讨论 |
| A5 | `ProviderEventSelector<E>` 的 public 形式 | 作为现有 `EventSelector<ProviderEvent, E>` 的 type alias | 待确认 |
| A6 | submitted task 失败如何终止 Loop | typed `Result` 返回 owning Component；`Err` 不是 Loop fault；panic 扩张为 Loop panic | 已确认 |

### B. Runtime correctness

| ID | 必须保证的事情 |
|---|---|
| B1 | retained callback 每次 dispatch 安装新 reaction scope，旧 `LoopControl` 不能修改新 reaction |
| B2 | wake/fault waiter 必须先 arm，再做最终 epoch/fault load，不得 lost wake |
| B3 | supervisor 覆盖 render、Input Gate、Provider dispatch、lanes、cleanup 和 sleep；fault 优先于 progress |
| B4 | cancellation 先 close reaction token，再 drop/cancel user futures |
| B5 | stop/fault/unmount/remount 主动 abort + await owned tasks，新 Provider request 等旧 mount 回收完成 |
| B6 | Signal、handlers、task ownership 和 loop policy 在同一 render transaction 中验证并提交 |
| B7 | task submit/unmount、stale wake/remount 和 disposition/reaction rollover 都使用 generation fence |
| B8 | 所有 hook kind 共享 `(HookSite, HookKind)` topology，改变数量、顺序或 kind 必须 fail closed |

### C. 兼容性与迁移

| ID | 需要保留的边界 |
|---|---|
| C1 | zero-argument autonomous root 与 legacy two-argument root 共用 private renderer abstraction 和单一 `ComponentHost` kernel |
| C2 | `ComponentReactionRuntime` 保留为 explicit-reaction API；缺少 AgentLoop capability 的 hook 在 preflight typed fault |
| C3 | v1 保留现有 Chess 编排，用独立小型 autonomous example 验证 `launch` path |
| C4 | crate root 只提供 `launch` 高层入口；旧 `EventInput` / `EventListener` 在 additive migration 期保留 |
| C5 | proc macro v1 只支持 direct/canonical hook paths，hook-bearing Component 参数的 `Clone` 约束需要 rustdoc 和 compile tests |
