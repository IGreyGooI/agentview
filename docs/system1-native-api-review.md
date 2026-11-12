# System 1 共用 Frame API 复核

AgentView 的公共 API 由普通 Component 视图提供 state，每个具名 Decision 声明
一个原生问题及本地结果绑定。用法见 [system1.md](system1.md)，完整设计见
[system1-api-design.md](system1-api-design.md)，运行时以 [engine.md](engine.md) 为准。

协议依据：[OpenRouter Decisions](https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-request)、
[System One](https://openrouter.ai/docs/guides/community/typesafe-sdk)、
[Jev 教程](https://openrouter.ai/docs/guides/community/jev-tutorial)。

## 公开接口

```rust,ignore
view! {
    GateContext { state: state }
    Decision {
        name: "open_gate",
        question: Noul::new("用户是否明确要求打开门？")
            .criteria("明确要求开门", "没有明确要求开门"),
        on_result: move |context, outcome: QuestionOutcome<NoulAnswer>| {
            output.store(context, outcome)
        },
    }
}
```

builder 为 `Decision::new(name, question).on_result(handler).build()`，类型由 question
决定。Noul、Choice、Score 分别生成各自的 typed answer；factory 是返回 question 的
普通 Rust 函数，handler 的结果类型从 question 推导。业务阈值、选项映射和多个答案的
业务结果组合在本地执行。

## 已落实的边界

- state 来自实际 Frame 的有效观察，不由 Decision 再接收一份 context。
- 多个 Decision 汇总为一个 questions map，使用同一 state 与单次模型请求；重复名称
  和非法问题在 FramePrepare 阶段可恢复拒绝，零 HTTP；修改声明后可以显式重试。
- questions、判据、decoder 和 callback 绑定实际提交 Frame，新 render 不改写在途结果归属。
- 每题独立交付 typed outcome；refusal、缺失、非法答案不吞掉有效的其他题。
- 原生和 Responses adapter 共用相同的 Frame、admission 与事件；Responses 的内部答案
  schema 从原生问题生成，不反过来主导业务 API。
- 原生 Provider 只支持 decision，native tool calls、text deltas 和 XML action output
  在提交前被拒绝；普通 Responses 保留工具交互及下一 Frame 的真实反馈。
- 当前公开 authoring 仅 Noul、Choice、Score 文本判断；底层 Box enum 不代表已提供
  typed image/assets 输入或端到端多媒体支持。
- 单轮 runner 渲染普通根视图并挂载相同 Decision，没有另一条输入或结果路径；最终成功
  等待 reaction 和 consuming cleanup，取消也保留 cleanup owner。
- 一次 reaction 完成本批判断，不追加确认、修复、重试或辅助压缩模型调用。

## 配置、概率和缓存

原生配置仅使用 System1Options，代码复用位于共享 HTTP、认证、预算和 observer 内部。
原生请求为 Decisions/System One JSON，Responses 使用自身真实协议。普通语言模型报出的
confidence 不承诺与原生 Jev 概率具有同样校准。

Noul 保留数值概率，Score 保留从零开始的连续位置，Choice 保留声明的选项和可选概率。
业务阈值、enum 映射或结果 struct 组合在本地执行。

本轮没有本地结果缓存。Responses 保留 prompt cache，原生记录实际 cached_tokens；原生
接口不发送 Responses 的 cache key/TTL 参数。后续结果缓存仍需覆盖完整有效 Frame
state、questions、模型和执行配置，并保留真实的 reaction/event 路径。
