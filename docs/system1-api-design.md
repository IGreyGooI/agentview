# System 1 / Decision API 设计

本文记录已实现的原生判断接口。使用说明见 [system1.md](system1.md)，运行时契约以
[engine.md](engine.md) 为准，协议复核见 [system1-native-api-review.md](system1-native-api-review.md)。

## 组件是入口，Frame 是状态来源

原生协议是 `state + questions → answers`。AgentView 已经通过普通 Component 声明模型
视图，所以 state 统一来自实际提交 Frame 的有效观察。Decision 不另收一份 context，也
不要求业务构造 HTTP 请求。独立判断挂载独立 Application，仍然交付普通 Frame。

```rust,ignore
#[component]
fn gate_view(state: GateState, output: OutputStore) -> Component {
    view! {
        #[developer]
        "只根据当前展示的信息判断。"

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
}
```

每个 Decision 声明一个 name、一个原生问题和一个本地 handler。多个 Decision 在 Frame
内汇总成一个 questions map，共享 state、reaction 和模型请求，按题分发结果。重复问题名
以及空名称、非法 criteria 在 FramePrepare 阶段以 InvalidDecisionDeclaration 可恢复拒绝，
零 HTTP；可修改 Signal 中的声明再显式重试，不用最后一个声明覆盖前面的 handler。

## 问题决定结果类型

公开问题类型为 Noul、Choice 和 Score，分别关联 NoulAnswer、ChoiceAnswer、ScoreAnswer。
`DecisionQuestionSpec::Answer` 确定回调类型；业务不注册输出 JSON schema，也不从
`Deserialize + JsonSchema` 的任意 struct 反推问题含义。

```rust,ignore
fn owner_question() -> Choice {
    Choice::new(
        "应该由哪个团队处理当前工单？",
        BTreeMap::from([
            ("frontend".to_owned(), json!("页面渲染、布局问题")),
            ("payments".to_owned(), json!("支付、结算问题")),
        ]),
    )
}

Decision::new("owner", owner_question())
    .on_result(move |context, outcome| outcomes.store(context, outcome))
    .build()
```

factory 是普通 Rust 函数。builder 从 question 推导答案类型；view 的三个属性顺序不限，
也可显式标注 `QuestionOutcome<ChoiceAnswer>`。handler 支持同步 Result 和异步 Future。
业务 enum 映射、概率阈值和多个答案的业务结果组合都在本地执行，不增加模型调用。

Noul 保留原始概率；Choice 保留选项、可选分布与 confidence；Score 保留从零开始的连续
有序位置、可选分布、legend 与 confidence，不取整。公开 authoring 仅覆盖文本判断；
底层协议保留 Box，不代表已有端到端图像/assets 交付能力。

## Frame 和两条 Provider 协议

Frame 保持现有有序的 replay、staged inputs、当前 projection 与 ephemeral；System
snapshot 和历史沿用既有交付规则。原生 Provider 使用完整 Full Frame，不能把只含本轮
变化的 Delta 当成全部 state。

结构化 `DecisionDeclaration` 只包含 runtime 生成的 batch identity 与具名 questions。
callback、typed decoder 和业务函数留在 mounted runtime。当前 batch 的 questions 不
同时复制到 state；历史问题目录随历史答案保留其判据。
输入和问题参与 Component、Frame、wire 和 token 预算；不静默截断。

| 内容 | 原生 System1Provider | 普通 Responses Provider |
| --- | --- | --- |
| Frame 有效观察 | 编码为 state | 编码为普通模型输入 |
| 原生问题和判据 | 顶层 questions map | 生成问题提示与内部固定答案 schema |
| 完整答案 | answers map | 内部 `{ "answers": { ... } }` envelope |
| 校验和分发 | 共用 canonical admission、按题 decoder 和 handler | 相同路径 |

原生默认 `POST /api/alpha/decisions`，SystemOne 选项使用 OpenRouter 的
`POST /api/v1/systemone`。成功响应为完整 JSON，不使用 Responses SSE；实际完整 body
进入 canonical output。两种 Provider 都只做一次模型请求，不再调用模型转换答案或确认。
普通 Responses 的 structured-output schema 是 adapter 编码细节，不是用户定义问题的入口。

原生 state 直接包含四个有效观察 section，普通 POM 渲染为给模型读的文本；Provider 不
解析 rendered prompt，不从中猜测 questions、工具或多媒体资源。业务代码和测试消费
结构化 POM、Frame 与协议数据，不把 POM 当 XML 序列化格式。

`System1Provider::try_new(transport, identity, System1Options::new(model))` 使用真实原生
options；共享 HTTP 连接、认证、限额、observability 发生在内部基础设施中，不依赖包装
Responses Provider 或跨协议 options 转换。概率只保留上游实际数值，不承诺不同模型具有
相同校准。

## 能力和恢复

保留三项独立能力：native tool calls、text delta output、decision。完整当前 Component
声明汇总 FrameRequirements，准备期检查是否为 Provider capability 的子集，不静态分析
闭包，不分析 prompt。原生 System 1 只声明 decision；普通 Responses 声明真实的三项支持。

NativeToolCall 要求 native capability；显式 TEXT_DELTA 订阅要求 delta。XML 可以消费
complete-only Provider 的 sealed text，因此 XML 声明不自动要求 delta；但原生 System1
没有最终文本协议，普通 XML action Frame 仍在 HTTP 前拒绝。

Decision 输出模式不与 active native/XML action 声明混用。被动观察者和普通 POM 可与
多个 Decision 共存。提交前拒绝不推进 baseline、history、revision 或消费 receipts；
调用方可以修改组件声明后显式重试。mode/缺问题拒绝不能将未提交的 target 标成 terminal。

## 输出 admission 和组件事件

Provider 将回包关联到实际提交 Frame 的 batch identity，不依赖模型回显关联字段。
`ProviderFact::Decision` 是独立封口输出，terminal 的 primary_text=None。共享 admission
先记录真实输出，再按提交时的问题逐题检查答案 key、类型、选项、概率和分值约束。

`QuestionOutcome<T>` 包含 `result: Result<T, DecisionFailure>` 与 source。单题 Refused、
MissingResult、InvalidAnswer 不吞掉其他有效题。未知答案 key 不获得组件权限。非法 JSON、
整包 envelope 或 transport/stream/observation 故障仍是 reaction 错误，不能伪造成普通
单题业务结果。

questions、typed decoder 和 callback 冻结于实际提交 Frame。流运行期间的新 render 可以
产生新问题和 handler，但迟到答案仍用原声明验证并调用原绑定。每个 handler 都携带实际
application/reaction/frame/event 的 EventContext，不让业务自行拼接请求 ID。

Decision 暂存已接纳事件，在正常 completion 时交付 typed outcome。handler 不具备整轮
事务保证；外部最终成功需要等待 ReactionOutcome::Completed 与 consuming cleanup。将
答案交给其他业务 Application 时保留来源，并在它的下一 Frame 中展示有用反馈。Signal
写入和日志本身不产生模型观察。

## 单轮 runner、缓存和验证

`system1::judge(provider, state, question)` 为单题建立普通根视图与相同 Decision，挂载
fresh 独立 Application，只驱动一次 reaction，等待 consuming shutdown 后返回
`Judgment<Q::Answer>`。JSON state 在根组件内渲染成普通 POM，不成为 Frame 之外的输入。
取消 waiter 仍移交唯一 Application owner 完成 cleanup。Decision 模式禁止辅助压缩、
重试、修复和确认调用；无法直接提交时明确失败。

Responses 复用自身 prompt cache；原生没有对应 key/TTL 请求字段，保留实际返回的
cached_tokens。本轮不做本地结果缓存。后续缓存需绑定模型、完整有效 state、questions
与执行配置，并保留当前 Frame、reaction 和事件的真实关联。

测试覆盖两种 Provider 使用同一普通视图和多个 Decision、一次 reaction/HTTP、逐题
refusal/missing/invalid 的隔离、重复名称准备拒绝、真实 Frame binding、普通 native/XML
交互反馈、HTTP/协议/observer 故障，以及 runner 取消和 consuming cleanup。断言检查
结构化数据与状态，不解析或快照 prompt。
