# Runtime-Owned Compaction

本文记录运行时主动管理 compact 的设计方向，尚未实现，不改变
[`engine.md`](engine.md) 中当前 `CompleteTranscript` 契约。

## 1. 所有权

自动 compact 属于 `Application` 的 reaction 生命周期。运行时负责触发、选择覆盖范围、
验证结果及安装 replay checkpoint；Component 继续声明完整业务状态。

摘要执行与 checkpoint 管理解耦。`ContextCompactor` 作为具体 `Provider<C>` 的泛型策略，
定义与实现放在 provider 层；Application 仍只持有一个 P，通过统一的 compact 能力接口调用它。
`C` 借用 provider 管理的 session 执行压缩，不另行拥有一份主会话或强制创建独立摘要会话。
第一版采用 Codex 提示词生成文本摘要。原生压缩可以作为后续执行能力接入，但
provider-private opaque artifact 不能冒充可移植的 canonical 摘要；运行时仍管理覆盖范围和
checkpoint。

| Owner | 职责 |
| --- | --- |
| Application | 配置 policy，在 reaction 安全边界检查预算，通过 P 驱动 compact |
| FrameSession | 完整会话历史、有效 replay、覆盖关系、projection provenance、预算 |
| Provider<C>（拟扩展） | 持有 session，提供 compact 请求模式、continuation 和故障处理 |
| ContextCompactor（拟新增） | 借用 provider session，从选定输入产生候选；输出走 compact 专用处理 |
| 可选 port replay-replacement 能力 | 为新的 replay lineage 准备 wire continuation，在 handoff 时安装 |

旧 `ProviderPort` 保持单方法兼容接口。普通 `ReactionPort` 实现不因未启用 compact 被迫实现摘要。

### 第一版 HTTP 路径

自动 compact 的首个具体实现使用 Responses API，普通采样和摘要请求均走这一条 provider
session。公开 OpenAI provider 使用 `AsyncOpenAiResponsesProvider`；Chat Completions 保留为
私有兼容实现，不公开其 provider、options 或专用配置方法。

当前可复用的实现是 `AsyncOpenAiResponsesProvider`：它拥有 `reqwest::Client`、Responses
native Frame continuation 和 SSE 解析器，向配置的 base 下的 `/responses` 发请求。
`CodexHttpV1Encoder` 只负责协议编码，不单独持有 HTTP transport 或 session。
`ContextCompactor` 借用该 provider 的 session；端点、认证、请求头和连接配置归 provider。

沿用现有 Responses HTTP 通道、API base 和 API key 配置，普通采样与 compact 都请求
`/responses`。Chat Completions 的私有可见性与 HTTP 端点、认证方式无关。

启用运行时自动 compact 时，使用现有 `CodexHttpV1Options::without_context_management()`
省略请求中的服务端自动 compact 配置，避免两套触发策略并行。当前默认配置会请求服务端在
200,000 tokens 阈值自动 compact；这类 provider-private 产物本身不保证运行时全量重建 Frame。
省略该字段只控制客户端下发的配置，不能替未知服务端承诺额外行为。

### Session 复用

Codex 的逻辑 `Session` 和长生命周期 `ModelClient` 在 compact 前后保持不变。自动 Remote V2
把当前 `&mut ModelClientSession` 传入压缩流程，随后普通采样继续使用它。手动 compact 没有正在
执行的 turn，因此从同一个 ModelClient 创建 turn-scoped session；local summary 分支也会创建
这样的请求 session。新建 turn-scoped 对象不等于 fork 一条逻辑会话，也不一定新建 WebSocket。

AgentView 默认沿用 provider 当前 session。隔离的是请求模式及输出处理：compact 的输出不能
作为普通 assistant 回复 dispatch 给 Component，工具输出也不能未经对应协议验证就执行。
session 中的连接、routing 和 request/response 游标由 provider 继续管理。

同 session 不保证每次都能使用同一 `previous_response_id` 链。Codex 仅在请求是已知输入及输出
的兼容扩展时发送增量；压缩后的历史不匹配旧前缀时发送完整 input，仍可复用原连接。

### 默认摘要提示词

generic `ContextCompactor` 默认复用 Codex 的
[本地摘要提示词](https://github.com/openai/codex/blob/02a8f038b87ad34d4a1dc5058eda26972ed7aa6c/codex-rs/prompts/templates/compact/prompt.md)。
它要求保留当前进展、关键决策、约束和用户偏好、待完成事项，以及继续任务所需的关键资料。
这是普通模型生成文本摘要的路径；Remote V2 的 `compaction_trigger` 是另一种协议能力。
提示词里的 `another LLM` 是交接表述，不要求创建新的 provider session。

摘要恢复时附加引导：随后会提供完整的当前状态；摘要用于延续历史，涉及当前状态时以最新
projection 为准。这样摘要保留历史进展，Component projection 重新提供当前状态与规则。
实际复制提示词资源入库时，附带上游 Apache-2.0 许可证与适用的来源署名；修改时标明修改。

## 2. 完整会话历史与有效回放

这里的完整会话历史指现有 `CanonicalTranscript` 中的消息和工具调用记录；Component 当前
业务状态仍由完整 projection 表达，不需要另建一份业务事实存储。

保留两个不同的视图：

```text
transcript:      A B C D E F G ...        // 完整会话历史与因果身份
effective replay: checkpoint(A..D) E F G  // 下一次提交的模型上下文
```

checkpoint 不把原始事实替换成一条伪造的 assistant output。摘要是带来源的派生上下文，
其覆盖记录与普通事实分开管理；保留的历史 item 必须引用原始 occurrence，而非仅靠文本相等匹配。

checkpoint 至少包含：

- checkpoint identity、版本及前一个 checkpoint identity；
- 来源 transcript revision、覆盖前缀边界及 digest；
- 摘要或受约束的 replacement 表示，以及每个保留 item 的 origin；
- 生成时的 policy/model 信息和 authority binding；
- 对应 replay generation 与恢复所需的交付 metadata。

再次压缩以当前有效 replay 为输入，并把来源覆盖关系衔接到原始事实；不能重新展开全部旧历史
作为每一次摘要请求。完整事实保存不等于已有持久化：当前 Application 没有接入 RecordLog，
跨进程恢复需要另行保存 checkpoint 与事实日志，不能声称第一版已有 crash recovery。

## 3. 触发与覆盖范围

检查点放在 Component preparation 完成后、`FrameSession::prepare()` 之前。此时上一条 fact
stream 和工具执行已结算，尚未提交本轮 Frame。计算预算时包含：

- 当前有效 replay 与 provider 必需上下文的估算；
- staged ToolOutput、本轮输入、当前完整 Component projection 与工具声明；
- 输出预留和安全余量。

provider usage 用于校准当前窗口估算，不把累计账单或压缩请求的 output_tokens 当成压缩后大小。
token 估算和 canonical JCS 字节预算分别检查；未知模型窗口需要显式配置。

默认触发规则为：

```text
configured_limit = explicit_limit or floor(context_window * 0.90)
trigger_limit = min(configured_limit, context_window - output_reserve - safety_margin)
should_compact = estimated_next_input_tokens >= trigger_limit
```

配置必须保证预留后仍有正的输入预算，且留得下摘要请求本身。估算针对下一次请求实际可见的
完整上下文，不能只计算 Delta payload。最近一次 `input_tokens` 用于校准本地估算，之后新
产生的模型输出、本轮输入和工具结果仍要计入；`cached_tokens` 是输入的子集，不从占用中扣除。
压缩后重新估算摘要、保留 tail、当前完整 projection 与工具声明，确认有足够余量再提交。
接近 next-Full 字节预算时也需提前处理，token 余量不能替代字节预算保证。

当前 `FrameConstraints` 已有窗口和输出预留字段，但默认 Responses 配置未填写；usage 目前
仅通过 observer 暴露 input/cached tokens。provider 内的用量记录、估算与运行时触发尚需实现。

只压缩已结算的历史前缀。最早未闭合 ToolCall 起的 tail、近期保留项和所有 staged ToolOutput
受保护；覆盖边界不能拆开 call/result 配对。持续调用工具的会话仍应能压缩较早的已结算前缀，
不能要求 staging 永远为空才能 compact。

摘要请求不包含待消费的 ToolOutput receipt，也不能消费它。受保护的 call 与后续结果继续通过
正常 Frame handoff 提交一次。没有可压缩前缀、摘要无有效缩减、或者当前 projection 本身超限时，
返回明确的预算故障，不能反复摘要或静默丢内容。

第一版在 single-flight reaction 内等待候选；后台预测压缩留作后续优化。

## 4. 候选与安装

```text
capture immutable source
  -> Provider<C> executes compact on its existing session
  -> validate and retain the compact result with its source binding
  -> validate coverage, origins, closure and budgets
  -> prepare a replacement Full and runtime commit candidate
  -> provider prepares a new wire lineage without mutation
  -> Frame handoff installs provider state
  -> infallibly commit runtime checkpoint in the same poll
```

候选绑定原始 revision、旧 checkpoint、cut boundary、authority 及受保护 tail/staging 的身份。
安装前重新校验；过期候选不能推进任何 cursor。结构验证只能证明来源、因果闭合和预算，不能证明
生成摘要与原文语义等价；当前规则、当前任务输入和明确要求保留的内容应保持原文。

生成阶段失败、取消或校验失败不安装 runtime checkpoint，摘要回复不直接作为普通业务事实追加。
但 compact 是真实的 provider 请求，有自己的发送和完成边界；同 session 请求可能已经推进
传输游标，或者 seal 了下一次 continuation 必需的私有 artifact，不能因此回滚或遗失它们。

provider 必须区分当前 active replay、待安装 compact 结果、传输层 continuity。结果绑定来源
revision/checkpoint；取消或失败后保留已 seal 的必要状态，并如实声明可继续、需要重新准备，
或者 terminal。不能无条件维持旧 Accepted 声明，也不能把 request scratch 与必要 artifact
一并清空。pending compact 结果和普通 Frame candidate 如何对应，需要作为 provider compact
能力契约实现并验证。

安装复用现有 Frame handoff 边界，不在发送前单独清除 provider continuity：

- prepare 和普通 Frame pre-handoff submit 失败保持旧的 active replay 和 receipt；
- provider 保留 compact 请求已产生的必要私有状态，对 declaration 和 continuation 作真实更新；
- port 在 crossing poll 安装新 wire candidate，返回 `Ok(stream)`；
- Application 随即提交已验证的 runtime candidate，中间不引入 await 或其他可失败操作；
- 实际或不确定的 handoff 之后，stream fault 或取消均保留新 checkpoint 与已提交事实；
- terminal 故障、panic 或违反 handoff 契约使 Application 停止，不能继续使用失配的两份状态。

未提交的候选可在来源仍匹配时复用；它不是当前模型已接收的 checkpoint。compact 失败后若旧请求
已经超限，则返回故障，由调用方处理。

这里不能调用 `Application::reset_model_context()`：它清空 canonical history 与 staging。
需要显式的 replay-replacement 契约，保留完整事实和待提交结果。port 在 mount-stable capability
中明确声明支持该契约；不支持的 port 不启用 runtime replay replacement。

## 5. Frame 与基线

当前 `FrameCheckpoint.replay_basis` 同时承担 append 检查、Delta tail 游标和新 provider output
的 provenance 游标。实现时必须拆分为原始事实 cursor 与有效 replay generation/cursor。

每次 compact 后，下一帧将所有 Component 的当前完整 projection 按首次交付全量渲染，
包括 system snapshot、当前声明内容和工具。已有交付记录不能使当前内容被省略或只发送 diff。
成功 handoff 后以本次完整 projection 建立新基线，后续恢复正常增量交付。
“按首次交付”只重置交付假设，Component 业务状态、已执行工具的结果及待交付结果继续保留。

这第一帧是特殊的 replay-replacement Full：

- wire-visible 内容包含 checkpoint、保留 tail，以及当前完整 Component projection；
- canonical append 仍只记录新交付事实，不能把重发的旧 ToolCall/ToolResult 再追加一次；
- diff 的交付基线失效，需要完整重建；但原始 occurrence ownership 不能一并丢弃；
- 当前 Component 引用的 provider output 必须通过明确 origin 完成交付，不能仅因曾被摘要覆盖而省略；
- 下一次成功 handoff 建立新的交付 checkpoint，之后才恢复 Delta。

`ReconciledProjectionPlan` 应明确输出 `delivery_projection` 与 `canonical_append_items`：
前者用于 Frame payload，后者用于完整事实历史提交。两者不能再共享同一个 submission 列表。

所以不能简单清空全部 reconciliation，也不能沿用旧 checkpoint 省略所有 unchanged items。
`ResponsesFrameRequestState::prepare_full_base()` 当前会复用旧 wire input 和 prefix proof，
同样必须显式识别新 lineage；只把 Frame 标成 Full 不足以完成 rebase。Frame 需要携带 opaque
`replay_lineage` 或 checkpoint fingerprint，provider 用它识别 replacement、从新的 wire base
构造候选，并在 handoff 时一并安装。新的 wire base 不要求销毁 provider session 或关闭连接；
旧增量基线不兼容时改发完整请求。普通 continuity-recovery Full 继续沿用现有恢复规则。

## 6. 预算闭合

`FrameSession::prepare()`、`FullReserveBudget`、流式 fact admission 和 ToolOutput staging 的
假想 next-Full 校验都必须基于有效 replay。仅在最终请求编码前换成摘要，仍会被完整事实历史的
admission budget 提前阻断。

完整会话历史继续负责全会话 call ID 唯一性、结果对应关系和审计；压缩不能让旧 call ID
重新可用。预算 tracker 绑定 replay generation，避免将旧 checkpoint 下的估算提交到新窗口。
流中已经接纳的 partial output 与取消 fallback 仍须满足原有可回放保证。

## 7. 实施与验证

建议按依赖顺序实现：

1. 拆分 canonical cursor / replay cursor，增加显式 checkpoint 与 origin，并接通全部预算校验。
2. 增加 Provider<C> 的 compact 请求模式、同 session 候选执行接口、可选 replay-replacement
   capability 和 handoff 事务，用 scripted 实现验证运行时。
3. 在现有 Responses HTTP/SSE 路径接入 Codex 提示词摘要、用量记录和窗口配置，并省略服务端
   自动 compact 配置；再把预算触发接入 `Application::react()`，保留手动入口用于验证与诊断。

必须覆盖的行为：阈值上下界、超大新增输入、连续多次压缩、无缩减、生成失败/取消、过期候选、
replacement 请求在 handoff 前后失败、Full 后恢复 Delta、当前 projection 完整重建、工具持续续接、
工具 call/result 只提交一次、历史 call ID 不复用，以及中断输出的预算闭合。
还需覆盖同 session 复用、compact 输出不触发普通 Component handler、压缩后完整 input 的
continuation、已 seal artifact 后的失败/取消，以及 pending 结果与 Frame candidate 的关联。

设计依据是当前仓库的 `frame.rs`、`admission.rs`、`projection_diff.rs`、Responses frame request
状态，以及 Codex [02a8f03 的 compaction 流程](https://github.com/openai/codex/tree/02a8f038b87ad34d4a1dc5058eda26972ed7aa6c/codex-rs/core/src)。
session 复用的直接证据在 `compact_remote_v2.rs` 的自动入口、`compact_remote_v2_attempt.rs`
对可选 client session 的处理，以及 `client.rs` 的 session 缓存与增量请求校验。
Codex 的 active-history/checkpoint 分离值得复用；其 provider-specific history 类型和普通摘要
失败路径不能直接套用到 AgentView 的 provider-neutral canonical contract。
