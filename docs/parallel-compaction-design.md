# Provider 层 Parallel Compaction

状态：已实现于 native `AsyncOpenAiResponsesProvider`。policy-triggered background 默认启用，
可覆盖策略或关闭；forced compaction 默认关闭，须独立显式启用。
[engine.md](engine.md) 是运行时的权威契约。

## 1. 两层并发

两个独立功能以 `ReactionPort` 为边界：

| 层 | 功能 | 执行单位与结果 |
| --- | --- | --- |
| Port 上方 | popup / 后台应用 reaction | 宿主并行驱动独立 Application，各自拥有观察、动作与反馈 |
| Port 下方 | parallel compaction | 独立 provider 请求，生成候选压缩窗口，前台模型请求继续 |

本文说明下层 parallel compaction 的实现与配置。上层采用
[独立后台 Application](background-applications-design.md)，使用现有生命周期与显式业务消息连接。
两个功能均不要求先放开同一个 Application 的 single-flight gate，也不争用同一个 continuation。

## 2. 范围与所有权

采用 Responses 的独立 `/responses/compact` 接口压缩 provider-private wire window。
复用当前 provider 的客户端、认证、API base 和模型配置，请求拥有独立的输入快照、
HTTP 生命周期与结果。前台请求和压缩请求可以实际重叠，压缩输出走专用候选通道。

Provider 持有一个受监督的压缩操作，以及至多一个待安装候选。worker 不持有修改
前台 continuation、Component、canonical history、diff baseline 或 ToolOutput receipt 的能力。
它产生结果，由 provider 的前台交付路径决定能否安装。

这是 provider-private compaction。完整 canonical transcript、FrameSession replay 策略和
Component projection 的所有权保持现有契约。canonical hard budget 仍然独立生效；压缩 wire
window 不等于压缩 canonical Frame，也不提供无限历史。

[Runtime-owned compaction](runtime-compaction-design.md) 是另一个层面的 replay/checkpoint
设计。若后续需要改变 FrameSession 的 effective replay，下层负责计算，runtime 仍负责
覆盖关系、provenance、预算及 checkpoint 安装。不能把 opaque provider artifact 当成可移植摘要。

## 3. 启动与输入快照

在已有稳定输入时提前启动压缩，不等待压缩完成才发起前台请求。压缩依赖不可变快照，
不能读取一个随前台流不断修改的 Vec，也不能通过持有整个 port 的可变借用阻塞前台。

候选来源至少绑定：

- 逻辑 session / context generation；
- 当前 provider window lineage 和被覆盖前缀的身份或 digest；
- 压缩 cut boundary；
- 生成时的 normalized System instructions 与模型配置。

只选择已结算、因果闭合的前缀。最近保留项、open assistant output、尚未闭合的 call/result
及其后续 tail 受保护；cut 不能拆开工具调用和结果。前台新交付的输入与随后到达的输出
继续追加到 tail。压缩请求不能消费任何 staged ToolOutput receipt。

触发策略可配置。当前保留至少最新一个 wire item；工具配对或未完成输出可以使 tail 更长。
策略在 handoff 前同步判断，policy-triggered 后台 HTTP 只在普通 Frame 的真实 handoff 后启动。
forced compaction 可在超窗 Frame 的 handoff 前启动并等待。两者共用一个 coordinator；压缩期间不重新
复制 source 或启动第二个 attempt，同一个 source fingerprint 不无条件重试。

### 策略与计量

`AsyncOpenAiResponsesProvider::new(...)` 自动使用 `DefaultCompactionPolicy`，不需要调用
`with_parallel_compaction`。默认满足以下任一条件就允许尝试：

1. `provider_tokens >= threshold`。threshold 为
   `floor((context_window_tokens - reserved_output_tokens) * 0.9)`；reserve 未配置视为零。
2. `requested_tokens * 10 < provider_tokens`（严格小于），即此前约定的相对膨胀策略。

context window 与模型 ID 绑定在独立的 `ModelSpec` 中。用户必须在
`ModelSpec::new(model_id, context_window_tokens)` 中提供当前部署的容量；省略窗口无法编译，
零值返回 `ModelSpecError::ZeroContextWindowTokens`。所有型号都遵守这条规则，没有按型号填充的默认容量。
请求 options 接收已校验的模型描述，例如：

```rust
let transport = AsyncOpenAiTransportConfig::new(api_base, api_key)?;
let model = ModelSpec::new(model_id, 128_000)?;
let options = CodexHttpV1Options::new(model, None, None, None::<String>)?;
let provider = AsyncOpenAiResponsesProvider::try_new(
    transport,
    identity,
    CodexHttpV1Encoder::new(options),
)?;
```

provider 从这份 `ModelSpec` 派生 `FrameConstraints.context_window = ContextWindow::Tokens(NonZeroU64)`；
options 和 transport 都没有独立的容量覆盖值。窗口仅用于本地声明、预算检查和压缩策略，不序列化到 HTTP body。
模型 provider 不能声明 `NotApplicable`。声明在 mount 期间稳定。
运行示例时用户必须设置 `AGENTVIEW_CONTEXT_WINDOW_TOKENS`，该值与 `AGENTVIEW_MODEL` 一起构造 `ModelSpec`。
两个 estimate 都用 `ceil(bytes / 4)` 计算，
保证非空输入不会估成零：

| 策略输入 | bytes 来源 |
| --- | --- |
| `requested_context` | 完整当前 Component envelope 的 canonical JSON，含 System 和工具定义，在 diff/omission 之前 |
| `provider_context` | 本次准备后的完整 Responses JSON request，含历史、私有 artifact、instructions 和工具定义 |
| `compactable_context` | 前一 accepted wire window 中可压缩前缀的 JSON array |

三者是估算，不是精确 tokenizer 结果，JSON 编码的开销也不同。生产 Frame 始终提供 requested
计量；内部合成 Frame 缺省时比例策略不触发，token 阈值仍生效。没有闭合 source、已有 attempt 或本次将安装候选时，
不再次调用 admission policy。

用户可以同时调整默认 token 阈值和比例（下面将比例改为稍小的 1/11）：

```rust
use std::num::NonZeroU64;
use agentview::provider::async_openai::{
    ContextRatioPolicy, DefaultCompactionPolicy, TokenThresholdPolicy,
};

let provider = provider.with_parallel_compaction(DefaultCompactionPolicy {
    token_threshold: TokenThresholdPolicy {
        token_limit: NonZeroU64::new(120_000),
    },
    context_ratio: ContextRatioPolicy {
        provider_context_multiple: NonZeroU64::new(11).unwrap(),
    },
});
let mut monitor = provider.parallel_compaction_monitor().unwrap();
```

显式 token 阈值最多取可用输入预算的 90%，避免把触发点设到硬上限之后。
仅想使用一种条件，可直接传入 `TokenThresholdPolicy` 或
`ContextRatioPolicy`。用户也可实现 `CompactionPolicy`，或传入快速、同步、无阻塞的策略闭包。
自定义策略会替换整个默认策略。例如保留默认判断，并在窗口估算占用超过 75% 时提前尝试：

```rust
use agentview::provider::async_openai::{
    CompactionContext, CompactionPolicy, DefaultCompactionPolicy,
};

let provider = provider.with_parallel_compaction(|ctx: &CompactionContext| {
    let used = u128::from(ctx.provider_context.tokens)
        + u128::from(ctx.reserved_output_tokens.unwrap_or(0));
    let near_window = used * 4 > u128::from(ctx.context_window_tokens.get()) * 3;
    DefaultCompactionPolicy::default().should_compact(ctx) || near_window
});
```

比例触发控制历史相对当前需求的膨胀，不能独自保证绝对 context 上限，也不保证后台压缩一定
赶在增长前完成。模型窗口在 `ModelSpec` 中必填；canonical Frame 字节预算和
output reserve 通过 `AsyncOpenAiTransportConfig::with_responses_frame_limits(max_frame_bytes,
max_component_bytes, reserved_output_tokens)` 配置。前台会检查完整 request 的 bytes/4 estimate
加 output reserve，超限则在 handoff 前返回 Limit/RequestPreparation fault。后台同样校验自身
请求 bytes/4、transport request/response byte limits 和超时；不会超限后静默截断或阻塞等待压缩。
若要对某模型的实际 token 数给出精确保证，仍需对应 tokenizer；bytes/4 是此处约定的估算。

编码器始终省略 server-side `context_management`，普通请求不依赖这一扩展。独立
`/responses/compact` 仍要求 endpoint 支持；不支持时可使用 `provider.without_parallel_compaction()`，
monitor 返回 `None`，请求上限继续生效。本实现尚无普通 `/responses` 文本摘要 fallback。
来自服务端的合法私有输出仍按现有协议处理；若它改写了候选依赖的前缀，旧候选失效。

### Forced compaction

`with_forced_compaction()` 独立开启一次有界的超窗恢复尝试，默认关闭。例如只使用 forced、关闭
policy-triggered background：

```rust
let provider = provider
    .without_parallel_compaction()
    .with_forced_compaction();
let monitor = provider.parallel_compaction_monitor().unwrap();
```

它只处理 `ResponsesFrameRequestFault::ContextWindowLimit`。正常 under-limit Frame 沿用原 nonblocking
路径；编码、coverage、System binding 等其他错误原样返回。检测到超窗时，provider 不构造或发送该
foreground request：已有 worker 就等待它，没有 worker 才从当前 accepted wire window 的闭合前缀
启动一次 `/responses/compact`。background 和 forced 始终合计至多一个 worker，并共享同源去重。

等待上限是 transport `read_timeout`。失败、超时、没有闭合 source 或候选失效都返回原
`ContextWindowLimit`。成功候选只重新准备和预算一次；若仍然超窗，返回原 limit，monitor 保持
`Ready`，同一 source 不再启动 compact。准备和等待不会修改 accepted provider state、FrameSession、
canonical history 或 receipt；压缩后的 foreground 在首次 transport poll handoff 时才标记
`Installed` 并同步 commit。等待 future 被取消不会 detach worker，reset、shutdown 和 Drop 仍可取消，
其中正常 shutdown 收取 handle。

### Codex 源码对照

核对的本地 Codex revision 是 `02a8f038b87ad34d4a1dc5058eda26972ed7aa6c`（2026-09-11）：

- `codex-rs/models-manager/src/manager.rs` 从内置 `models.json`、provider 模型目录及其缓存取得
  模型 metadata；`model_info.rs::with_config_overrides` 允许配置 `model_context_window`，并受模型
  `max_context_window` 限制。该版本未知模型 metadata 的 fallback window 为 272,000。
- `protocol/src/openai_models.rs::ModelInfo::auto_compact_token_limit` 默认取 resolved context
  window 的 90%；显式阈值同样被限制在 90% 内。它与默认 95% 的 usable context window 是两个概念。
- `core/src/context_manager/history.rs::get_total_token_usage` 使用最近 provider usage 加未计入的
  新增 item estimate，而非只看最新 Delta；AgentView 当前统一使用约定的 bytes/4 estimate。
- `core/src/session/turn.rs` 在 pre-sampling 和需要继续时检查阈值，await compaction 后继续。
  `run_auto_compact` 按 provider capability 选择 remote V2 或普通模型摘要。

AgentView 采用 client-owned threshold，但保留后台异步计算与后续 handoff 安装；额外预留显式 output
reserve 后再计算 90%。所有型号都由用户显式配置容量，没有采用 Codex 的自动窗口解析。

## 4. 并行完成与安装

```text
启动时的窗口：     A B C | D E
压缩请求的输入：   A B C
前台继续后的窗口： A B C | D E F G
待安装的候选：     compacted(A B C)
下次交付的窗口：   compacted(A B C) | D E F G
```

`compacted(A B C)` 是接口返回的完整 `output` 数组，包括 retained items 和 opaque
compaction item。必须原样保留数组内容与顺序，不能只抽取其中的 compaction item。
这些输出不作为普通 assistant reply、ProviderFact 或 Component 工具调用派发。

来源之后仅追加 tail 不应使候选过期。安装时检查实际被覆盖前缀仍匹配、context generation
和 authority binding 仍有效；从当前窗口取得最新 tail，不能覆盖成启动时的旧 tail。
context reset、前缀替换、指令变化或另一轮不兼容 compaction 使候选失效。

安装分两步：

1. 准备：用候选完整窗口加最新 tail 构造 disposable wire candidate；重新验证调用闭合、
   canonical coverage、instructions binding 和精确请求预算。此时前台 active state 不变。
2. 交付：下一普通 Frame 的真实 handoff 安装 provider candidate；紧接现有 runtime 同步
   commit，中间不新增 await。handoff 后的断流不回滚已安装的窗口。

pre-handoff 失败或取消不消费候选、不推进 active continuation；来源仍兼容时可以再次准备。
worker 完成只代表候选 Ready，不代表模型已收到压缩后的上下文。进行中的前台请求继续
使用它发起时的窗口。

当前 provider 的 canonical-prefix coverage proof 必须继续成立。压缩改写 wire 表示，
不能使已接纳的原始事实重复提交、丢失或重新获得 call ID，也不能丢失 required private artifacts。

## 5. 生命周期、失败与观察

`ParallelCompactionMonitor::status()` / `changed().await` 提供以下可观察状态：

```text
Idle -> Running -> Ready -> Installed
              \-> Failed
                  Ready -> Discarded
Running / Ready -> Cancelled
```

每个 attempt 有独立身份。观察者可以区分正在计算、候选可用和已实际安装；错误分类
不携带 provider 文本、认证信息或原始请求内容。需要向模型展示压缩进展的应用可将状态
投影到自己的 view，再安排后续 Frame；状态通知本身不产生模型观察。

通知使用 watch，允许合并中间状态。Ready 表示后台已经算出候选；后续 submit 非阻塞地收取
已结束的 worker，再尝试安装。`changed()` 在 provider 和 worker 都释放后返回 `None`。

- 单个 provider 的压缩操作有界；已有运行中或 Ready 候选时不再启动重复操作。
- 对同一个失败来源不做无条件紧密重试；重试策略与前台 reaction 调度分别定义。
- 压缩失败、无有效缩减、取消或候选过期，不破坏仍可用的前台窗口。
- request / response 字节上限、超时、协议验证均独立执行；后台任务不能绕过预算。
- reset 立即撤销旧 attempt 的安装资格并 abort，保留 handle 到下次收取或 shutdown join。
- 正常 shutdown acknowledgement 必须等待 worker 清理；普通 Drop 只提供 best-effort abort。
- worker panic 在下次 submit 或 shutdown 收取时原样传播，不能变成普通 compaction 失败。

后台请求只计算候选。共享客户端不意味着共享请求游标；后台请求的返回不能直接覆盖
前台 Accepted revision、remote continuation 或当前 Frame state。

## 6. 实现落点

1. [`parallel_compaction.rs`](../src/provider/async_openai/parallel_compaction.rs)：策略、计量、controller、
   不可变 source、owned worker、完整 output 校验与状态观察。
2. [`codex_http_v1.rs`](../src/provider/codex_http_v1.rs)：独立、受 byte limit 限制的 compact 请求编码。
3. [`frame_request.rs`](../src/provider/async_openai/frame_request.rs)：闭合前缀选择、来源验证、
   候选加最新 tail、canonical proofs 和请求窗口 estimate 检查。
4. [`native_reaction.rs`](../src/provider/async_openai/native_reaction.rs)：handoff 时启动或安装候选，
   reset 失效处理与后台清理。
5. `Frame::requested_context_bytes()`：沿用已有完整 Component meter 提供 advisory metadata；
   `ReactionPort::shutdown()` 默认空实现，Application 在正常 consuming shutdown 时 await 后台清理。

不在这一功能中引入上层 popup API，也不通过删除 canonical history 或调用
`reset_model_context()` 来模拟 compaction。

## 7. 验证要求

使用有显式 barrier 的本地 HTTP 服务控制完成顺序，避免依靠 sleep 判断并发。
交互覆盖见 [`component_api_parallel_compaction.rs`](../tests/component_api_parallel_compaction.rs)，
比例、协议、coverage 与生命周期边界另有单元测试。测试不需要真实 API key。

- compact 请求保持 Pending 时，前台请求仍能送达并完成。
- forced 未启用或 Frame 未超窗时不等待 compact；forced 超窗时已知 oversized foreground 不送达。
- forced 与 background 共享一个 worker；等待取消后同一候选可重试，timeout/reset/shutdown 仍能收取任务。
- forced 候选仍超窗时保持 Ready、不安装、不重复 compact，accepted revision 和 receipt 不提前推进。
- compact 完成前后，前台均可追加新输入；安装结果精确保留最新 tail。
- compact `output` 中 retained items 的内容与顺序完整保留，且不会触发 Component handler。
- 完整 observe -> action -> feedback -> next action 在压缩前后继续成立。
- tool call/result 配对完整，pending calls 受保护，ToolOutput receipt 只交付一次。
- System change、context reset、前缀替换、过期候选均无法安装错误窗口。
- pre-handoff 失败可保留候选；post-handoff 失败保留已安装窗口，并遵守既有恢复契约。
- 超时、HTTP 错误、格式错误、超限、无缩减、取消、panic 与 shutdown 的状态和清理可验证。
- 连续 compaction 基于有效 wire window 工作；Full recovery 与后续 Delta 均保持合法。

## 8. 接口依据

已核对官方文档：

- [Compaction guide](https://developers.openai.com/api/docs/guides/compaction)
- [Compact a response](https://developers.openai.com/api/reference/resources/responses/methods/compact)

独立 compact 接口是 stateless 操作，返回完整的压缩窗口；官方要求后续请求原样使用
完整返回窗口。具体部署的模型及 API endpoint 仍需支持该能力；不支持时显式关闭后台 compaction。
