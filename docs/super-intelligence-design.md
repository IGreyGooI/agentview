# SuperIntelligence 接入 AgentView 事件流

状态：事件原生的 SuperIntelligence 接入仍为设计草案，SuperIntelligence、判断需求 metadata 和
模型路由尚未实现。当前落地范围仅为无模型历史的 prompt 导出：
`RenderedProjection::render_prompt()` 和 `ProjectionSnapshot::render_prompt()` 返回当前完整声明的
模型输入 String，供调用方自己的模型 API 使用；它们不调用模型或执行动作。
示例见 [`export_prompt`](../examples/export_prompt.rs)，内容与快照新鲜度见
[HELP.md](../HELP.md#export-a-prompt-without-model-history)。以下事件流、模型选择和需求声明描述未来
设计。[engine.md](engine.md) 是现有 runtime contract。本文不改变已实现的 Application fork。

## 统一 reaction 链路

Component 持有业务状态，声明当前观察、可用动作、约束和实际结果。SuperIntelligence 是该
界面的逻辑操作者，内部可以使用一个模型、多个模型或规则，但通过现有 reaction 协议交互。

```text
业务输入 / Component task
          |
    更新状态、声明本轮需求
          |
显式 driver 推进 reaction
          |
preparation + reconcile -> Frame
          |
SuperIntelligence (ReactionPort)
          |
ProviderFactStream
TextDelta / TextSealed / ToolCall / ReactionCompleted
          |
admission -> 当前 RenderBindings -> Component handler
          |
Signal / ToolOutput / 实际反馈
          |
     后续提交的 Frame
```

不新增要求调用方串联 observe、decide、apply 的决策接口。模型结果通过事件流回到现有
Component handler。若使用 Decision 一词，它描述操作者在 reaction 中产生的操作提议，
不作为绕过 admission 的直接执行权限或另一套业务执行入口。

## 当前已有的接入点

`ReactionPort::submit(Frame)` 返回 `ProviderFactStream`。runtime 接纳事实后才 dispatch 到当前
Component bindings；native tool handler 产生实际结果，更新 Signal 并进入 ToolOutput staging。
下一次显式提交的 Frame 交付结果、进度或失败及可继续操作的状态。

SuperIntelligence 可以通过实现这个 port 接入，不需要改变 NativeToolCall 的 typed callback。
例如判断流程可以声明一个 `submit_verdict { request_id, value }` 动作；内部模型选择结果后，
适配器产生对应 ToolCall 及正常 reaction completion，现有 handler 验证业务请求并记录判断值。
这表示判断已返回，不表示其建议的外部业务操作已经执行。

多个内部模型可以提出候选，但只有被选择的输出进入对外的有序事实流。未选中的候选不调用
业务 handler。不能先把多个候选 ToolCall 发入 runtime 再撤销未选中的业务效果。

现有 `use_reaction_request()` 仅产生合并的 driver demand，不携带判断问题、速度要求或任务
payload，也不自动开始 reaction。`run()` 按原规则推进；其他 driver 显式决定何时处理 demand。
Signal 写入和任务完成本身不提交 Frame。

## Component 声明快速判断需求

缺少的契约是从 Component 声明，经 projection 和 Frame 编译，交给操作者的结构化判断需求。
该声明应至少表达请求身份、当前输入、允许的结果动作，以及本次决策的速度或质量约束。
具体 authoring 类型、字段和事件表示尚待确定，不在这里预设另一个公共决策接口。

声明仍属于当前 Component 输出，和相应的动作 binding 来自同一次准备完成的 render。
Component 不选择 endpoint、不持有模型 provider continuation，也不在 render/preparation 内
递归发起 reaction。必需业务输入沿现有 preparation 准备；执行仍由 driver 和 reaction 负责。

本轮请求的结构化结果通过事件进入 Component。handler 检查 request identity、业务版本和
前置条件，记录实际结果并结束或推进请求；失败、拒绝和重试条件也必须由应用声明。
新增 metadata 不能成为只写日志、不交付后续 Frame 的私有反馈通道。

普通无效模型动作应跳过并保留有用反馈，继续有效的独立动作；实现需要明确适配器如何交付
这些反馈。现有未知工具 binding 或 handler Err 仍可能是 runtime fault，不能仅改名为 Decision
就假定已有普通拒绝语义。取消、generation fence、工具调用身份和外部副作用规则保持有效。

## 多模型、完整观察与 history

不能假定本轮内部模型看过前几轮。没有历史时，新挂载的首次 Frame 本来就是 Full；业务应
在当前 Component 声明中提供完成判断需要的状态、上下文、动作和必要过去事实。

当前 Frame 是固定 target 的交付包，Full 也可能依靠 replay 与 reconciled projection 共同
满足当前声明，不等于只取 projection section 就得到独立完整观察。若内部模型每次无历史
调用，需要明确的完整 typed observation 编译或物化路径；不能把源 Delta 直接交给新模型，
也不能解析 rendered prompt 重建当前状态。

对外维持一个逻辑 target、稳定 profile 和一条权威事实流。内部各模型独立检查预算，并管理
自己的 history、continuation 和缓存；跨模型不能默认复用 reasoning 或 opaque compaction。
Runtime 仍保留已接纳动作、结果及因果记录。模型历史不是业务事实的唯一 owner。

## 验证

验证 Component 声明的请求进入 Frame，操作者产生有序事实，runtime 调用真实 handler，随后
Frame 交付结果并支持下一动作。覆盖无模型历史、内部模型切换、多候选只执行一次、流式输出、
普通拒绝后继续、过期请求、业务版本冲突、取消和 shutdown。测试验证 typed POM、结构化
Frame/协议、状态和交付行为，不解析或快照 rendered prompt。
