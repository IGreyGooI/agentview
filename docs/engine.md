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

## 7. Unsupported ToolCall

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

## 8. 取消、失败与 remount

取消 reaction 时：

- drop Provider stream；
- 取消 Runtime 拥有的 pending lane futures；
- 保留已经提交的 input、已经 item.done 的模型 items 和已经组装的 ToolOutputs；
- 将尚未 item.done 的 inflight items 标记为 aborted；
- 保留取消前已经成功的 Signal 写入；
- 不自动发起重试或下一次模型调用。

Provider fault、Component handler fault、lane panic 和 terminal handler fault 都必须带明确 stage 和
reason。清理完成是终止条件的一部分，不能把仍在运行的 task 留给下一次 reaction。

Component remount 会创建新的 runtime identity。旧 node 的 Provider history 不撤回；新 identity
作为新的 projection node 参与后续 reconciliation。旧 mount 的 handler、lane 和 Signal handle
不得写入新 mount。

## 9. Observability

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

## 10. 必须保持的不变量

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

## 11. 非目标

本文不规定：

- Chess、裁判或其他业务领域规则；
- 某个 ToolCall 的业务 schema 和执行实现；
- LiteLLM 的内部 cache/session 命名；
- UI 展示格式、JSONL 字段全集或测试计划；
- 实现顺序、迁移进度和发布里程碑。
