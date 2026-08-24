# ProviderPort / ApplicationHost 开放问题

日期：2026-08-08

本文只记录会改变公共语义、且尚未由
[`provider-port-application-host-boundary.md`](provider-port-application-host-boundary.md)
确定的问题。确定性实现问题直接修复，不在这里展开新的架构。

问题确认后，应把结论写回边界文档或看板，并从本文移除。

## Q1：native ToolCall consumer 还缺什么 public authoring？

状态：name-only declaration、completed call handler 和 result return 已确认；input schema
authoring deferred。

当前 public authoring 是 `NativeToolCall::named(name).on_call(handler)`。它只声明 tool name，
不声明 input schema。匹配的 completed `ToolCall` 直接进入这个专门 consumer，不经过普通
`EventInput` / `EventListener`，也不能用消费 XML text stream 的 `XmlStreamingToolCall` 代替。

当前已冻结的 result contract 是：

- handler 接收 completed `ToolCall` 并返回 `Result<ToolOutput, E>`；
- `call.output(content)` 生成绑定原 `call_id` 的 ToolOutput；
- ApplicationHost 在 reaction-local async lane 中等待 handler，把唯一 output 发布到
  Provider-owned sink；
- Responses Port 为下一次 Input Gate 暂存并按 call 顺序提交 output；本地 Gate 失败不会消费
  staged output。

仍开放的问题只有 input schema 的 public 表示与 lowering。当前实现不能把 name-only API
解释成已经存在 schema contract，也不能在没有新设计决定时扩展 API。

## Q2：ParallelToolCallComponent 如何 author 和 lower？

状态：deferred；与已经实现的 reaction-local tool lanes 无关。

当前不存在 public `ParallelToolCallComponent` 或其他 request-level policy authoring，Responses
request 把 `parallel_tool_calls` 保持为 `false`。未来是否允许 Provider 在同一个 response 中
产生多个 call、它的 public authoring 以及 projection lowering 仍未冻结；native tool capability
是否开启仍由匹配的 `NativeToolCall` Component 决定。已经实现的 reaction-local async lanes
只调度 Provider 已经完成并发出的 ToolCall handlers，不能被解释为这项 Provider request policy，
也不是通用 Event 并发 scheduler。

## Q3：Component key 的 public authoring 语法是什么？

看板要求 keyed sibling reorder 后保留各自的 Signal state，并要求 duplicate key fail closed；但
当前 public Component authoring 尚未定义 key 的写法。这个选择会新增公开 API，不能由实现层
自行猜测。

语法冻结前，positional sibling 仍按位置识别；keyed reorder、removed-key stale handle 和
duplicate-key 测试保持为 B1 未完成项。

当前 projection tree 只需要 runtime `ComponentId` 做 ProviderPort provenance，不依赖 Q3。
如果 reorder 导致 `ComponentId` 变化，当前语义允许 Port 将其视为新 node；Q3 只决定未来
是否需要 identity 跨 reorder 保持。

## 不作为开放问题

- reaction-local consumers 的 Rust owner、normal EOF 和 supersede 的清理顺序属于内部生命
  周期实现，不扩大公共边界；当前实现名 `RenderBindings` 不构成公共概念；
- 具体 Port 如何保存 continuation、instructions/artifact representation、compaction 和 recovery
  context 属于私有实现，不建立通用 `ArtifactBinding` 或其他公共模块；
- Component-owned history 只表示业务 POM；assistant/provider conversation 属于可丢弃的 Port
  私有 context，不新增 `#[assistant]` 或 raw `CanonicalInputItem` 注入 API；
- `RecordLog`、外层 AgentLoop、checkpoint/accept、revision/CAS 不在当前目标中。
- External wrapper 持有当前 reaction：`observe` 结束当前 reaction 后开启下一条并返回新
  observation；`act` 先注入外界模型输出，再结束当前 reaction、开启下一条并返回新
  observation。它不使用公开 `observation_id` envelope。
