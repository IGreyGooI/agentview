# 基于现有 AgentView 的实时输入与 Provider API 草案

本文记录通用媒体会话 API 的设计提案。当前 Qwen Realtime port 已支持同一次
`Application::react(false)` 内的多轮语音、Component 工具和视图回执。实际入口见
[Qwen Realtime 示例](../examples/README.md#qwen-realtime)，运行时契约见 [engine.md](engine.md)。

## 设计出发点

继续用 `Application::mount(root, provider)` 创建应用，用 Component 声明完整当前业务视图，
用 NativeToolCall 调用现有业务，用 Frame 提交模型真正看到的状态和工具结果。
在同一个 Application 内增加持续媒体输入和会话事件处理。

StepFun 是这个模型下的一个 provider 实现。应用侧的音频类型、输入入口、事件和工具处理
使用 AgentView 类型；StepFun 的 WebSocket、事件名称、item ID 和 base64 编码属于 provider。

## 现有接口与扩展位置

| 现有接口 | 拟扩展内容 |
| --- | --- |
| `Application<P>` | 拥有媒体输入通道、会话事件 pump 和相关清理；公开受生命周期约束的输入 handle |
| Component / Signal / preparation | 继续声明业务状态、动作、反馈与就绪条件；增加需要的 typed 媒体声明和事件 |
| `FrameSubmission` / private FrameSession | 提交媒体输入关系，跟踪交付和 canonical 历史；保留现有业务 diff 与工具回执 |
| `FrameCapabilities` / requirements | 声明音频输入、音频输出及会话行为；不支持时在交付前拒绝 |
| `ReactionPort` | 保留离散 `declare / submit / shutdown`，增加可选会话能力的发现入口 |
| `ProviderFact` / provider handlers | 增加 reaction 内音频输出；区分 reaction facts 和会话级输入事件 |
| `Application::run()` / `react()` | 同一个 driver 持续处理会话事件，并保留单次主动 reaction 的准入和结算 |

当前代码锚点：

- [公开 Application 与协议导出](../src/component/execution/mod.rs)。
- [Frame、能力与 ReactionPort](../src/component/execution/reaction.rs)。
- [canonical 输入事实](../src/transcript.rs)。
- [Application 的 Main / Temporary 所有权](../src/component/execution/application_parallel.rs)。

## 应用侧 API 的目标形状

以下为拟新增接口的伪代码，不是当前可编译示例：

```rust,ignore
let mut app = Application::mount(business_root, StepFunProvider::new(config))?;

// 注册输入源并取得 handle；此操作本身不发起模型生成。
let audio = app.open_audio_input(format)?;
let playback = app.subscribe_audio_output()?;

// Application owner 并发驱动：
// 1. 外部输入任务：audio.push(chunk).await
// 2. 输出任务：消费 playback 事件并交给实际播放端
// 3. app.run().await
// 退出时先停止外部输入，再完成 app.shutdown().await。
```

输入 handle 表达流身份、格式、序号和生命周期。`push()` 的成功表示框架接纳了该块，
不等于服务端已经接收，更不等于模型已经消费。队列必须有界，满时背压或返回明确错误。

输入源注册后，由框架建立对应的 typed 输入关系；Component 能使用该身份及其状态声明
业务视图。源的创建、关闭、完成和取消均经过 Application，业务不取得可变 provider。

音频块持续流入；Frame 关联音频流与输入范围。每块音频不单独创建 Frame。
输入的结束可以由明确事件或受框架管理的 VAD 产生，不能把关闭整个会话等同于结束一句话。

## Provider 入口：在原有边界上增加可选会话能力

建议为 `ReactionPort` 增加默认不支持的会话能力入口；能力的具体 Rust trait 签名随实现细化。
能力必须在 mount 时的 profile 中稳定声明，不能在已有 Application 中切换逻辑 target。

会话能力向 Application 提供：

| 方向 | 协议内容 |
| --- | --- |
| Runtime → provider | 有交付前置条件的 Frame 提交 |
| Runtime → provider | 已授权输入源的音频追加、输入结束和清理 |
| Runtime → provider | 请求生成、取消指定生成 |
| Provider → runtime | 会话状态、输入开始/结束、输入提交确认 |
| Provider → runtime | 生成开始、带身份的有序模型 facts、生成结束 |

该接口仍然只由 Application 使用。现有离散 provider 可以保持默认不支持；StepFun 实现
会话能力。不会向业务公开 `provider.send_json()` 或远端 WebSocket handle。

Provider 的会话任务独占 WebSocket 写入和读取，在同一条有序控制路径上处理 Frame 与媒体。
Runtime 使用一个入站 pump 分发会话事件和 reaction facts，不能由两个 reader 竞争同一连接。
会话连接和音频转发的启动必须有明确的 driver 与交付边界；`declare()` 保持幂等 snapshot read，
同步 mount 也不隐式调用模型。

## `react()` 需要如何演进

当前 `submit(Frame) -> ProviderFactStream` 表达一次有限响应；`ReactionCompleted` 是最后一个
fact，随后正常 EOF。这个流不能直接变成整条长期 WebSocket。

需要拆开三个身份：

| 身份 | 生命周期 |
| --- | --- |
| Session / target | 同一个 Application 的逻辑模型会话；物理连接重建受 continuity 约束 |
| InputTurn | 用户的一次输入，可在模型生成期间开始 |
| Reaction | 一次被 runtime 接纳的模型生成，关联实际交付的 Frame |

`react()` 保留应用主动请求一次生成的意义，其 preparation、Frame 准备、handoff、facts
接纳、工具执行及结算仍由 Application 拥有。会话模式的 driver 同时处理输入和会话事件，
不必等待一次 reaction 结束才能接收音频。

现有 Main / Temporary 含义保持明确。用户插话进入输入准入流程，按应用声明的策略排队或
中断指定 reaction；不能用 Main 活跃时再次 `react(false)` 创建 Temporary 来代替插话。
不支持独立 Temporary 的 provider 明确拒绝该能力。

会话模式下的 `run()` 必须继续尊重业务 preparation 与退出条件，同时在 preparation、工具
执行和空闲期间推进媒体与会话 pump。输入到达、业务 dirty 和生成请求是不同事件：媒体接收
不无条件触发生成，Signal 写入也不自动获得模型调用权。

## 不可绕过的因果与交付规则

1. Frame 的交付前置条件、handoff 与 private commit 仍然是业务输入提交的边界。
   会话型 Frame 交付也必须保留“交付前取消为零提交”的可证明语义。
2. 音频输入单独记录接纳、进入不可撤回传输路径、服务端 committed 等状态。
   首次发送媒体同样是外部交付，必须在协议中定义；不能用“只是发了一块数据”绕过 runtime。
3. Frame 授权的流和范围确定可发送的媒体。恢复和重连需要知道实际交付或结果未知的范围；
   StepFun append 没有逐块确认，不能据本地发送成功伪造服务端确认或无条件重放。
4. 一次模型生成绑定真实已交付的 Frame revision 和对应输入范围，不能在输出到达后给它
   贴上当时尚未交付的最新 projection revision。
5. 用户输入提交是输入事实，VAD 是会话事件，模型 text/audio/tool 是输出事实。
   各自先完成所需接纳，再投递 Component 事件；保留来源身份，不能伪造 ReactionId。
6. 工具调用先进入 canonical history 再执行。结果仍通过下一 Frame handoff 提交一次，
   pending 工具闭合规则不因连接持续存在而放宽。
7. `ResponseEnded` 仅终结对应 reaction；会话 EOF、传输断开、音频结束都不能冒充正常完成。

StepFun 服务端 VAD 可以自动触发推理。若 runtime 尚有未交付工具结果或业务 Frame，自动起答
可能违反上述规则。第一阶段支持持续流式上传、显式控制输入提交与起答；该模式需要
AgentView 管理本地 VAD 或输入结束事件。服务端自主起答作为单独能力验证，只有在能够证明
输入范围、Frame 因果关系和工具闭合顺序时开放。

## 音频事实、输出和取消

Canonical 输入应增加 provider-neutral 媒体事实，至少关联媒体身份、格式、话轮、输入范围
及完成/取消状态。身份属于 AgentView；远端 item ID 由 provider 映射。
框架管理的媒体对象或可追溯引用持有原始内容；引用的存活和可重放性必须有明确保证。
音频大小、时长和容量计量独立定义，不能用短引用绕过现有预算。

Reaction 内的音频增量、封存和中断要通过 typed 输出暴露。生成完成与播放完成是不同事件；
播放端报告消费进度，runtime 根据应用策略停止生成并通知清理指定输出的播放队列。
取消保留已执行业务效果，媒体也保留实际交付范围与未知状态。

音频及其转录不能编码进普通 POM 字符串来充当原生输入。应用和测试消费 typed 媒体、Frame
及结构化协议，不解析渲染 prompt。转录可以是明确标识的业务可见信息，不自动替代音频历史。

## 用现有业务闭环验证

先做可脚本化的 Debug 会话 provider，再接 StepFun，验证同一套 Application：

1. 初始业务视图和工具声明提交后，音频多块持续进入；模型输出期间仍能接收输入。
2. 用户音频驱动一个声明过的工具；工具参数由 schema 校验，调用既有业务服务。
3. handler 记录真实结果并更新 Component 状态；下一 Frame 交付结果和新视图。
4. 模型根据该反馈继续语音回答，或使用新视图选择下一个动作。
5. 用户插话能中断指定回复；其他任务及已成功业务结果按既有规则保留。
6. 背压、交付前取消、断线后的未知输入范围、pending 工具和 shutdown 都有确定结果。

这是协议行为验证，不以 prompt 字符串、渲染快照或仅能收发音频作为完成标准。

## 实施范围

实现先明确并更新 engine 中的会话事件、媒体交付和 reaction 绑定契约，再增加 typed ingress、
能力声明和同一 Application 的会话 pump，然后实现 StepFun provider。
业务新仓库使用这些公开 API，复用现有业务接口与 Component 交互。

本草案只确定扩展方向和必须保留的语义。输入 handle 的确切命名、会话能力的 Rust 签名、
媒体存储和预算实现需在对应实现变更中给出可运行的接口与行为验证。
