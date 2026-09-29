# 并行后台 Application

状态：编排方案已确定，本文记录设计选择；没有新增 runtime API。
[engine.md](engine.md) 继续是运行时的权威契约。

## 1. 决定

Popup / 后台应用使用一个独立的 `Application<P>`，由外层宿主与主 Application 并行驱动。
两个 Application 的 reaction 可以实际重叠；各自内部继续保持 single-flight。

这使用现有的 `Application::mount`、`react` / `run`、exit 和 `shutdown` 边界。
不为 popup 拆出共享 Component Runtime 的多份交互上下文，也不要求同一 Application
同时运行多个 reaction。

```text
Host
  ├─ Main Application
  │    ├─ Component tree / Signals
  │    ├─ FrameSession / canonical history / diff baseline
  │    └─ ReactionPort / target continuation
  └─ Background Application
       ├─ Component tree / Signals
       ├─ FrameSession / canonical history / diff baseline
       └─ ReactionPort / target continuation

应用之间：显式的业务服务、输入通道、进度与结果消息
```

## 2. 独立与共享的状态

每个 Application 拥有独立的 root、mount identity、Signals、preparation、provider bindings、
任务作用域、FrameSession、ToolCall/ToolOutput 记录和 port。两个 port 不共用一个可变的
Accepted cursor；相同模型或相同连接配置不代表相同 logical target session。

后台应用只得到显式提供的任务、规则和业务上下文。它不自动继承主应用完整 transcript，
其 assistant output 和工具记录也不自动并入主应用 transcript。

共享业务数据放在明确的业务服务、数据库或外层拥有的状态句柄中，各应用只获得任务所需的能力。
它们可以使用相同 Component 定义构建不同实例，但这不会共享 hook state。跨应用资源冲突、
版本检查和副作用幂等由该业务数据 owner 负责；应用隔离不等于共享数据库自动获得事务性。

共享服务变化不会自动使所有 Application 的 projection dirty。每个应用通过订阅、输入通道
或 preparation 读取变化，更新自己的 Signal，再由自身 driver 安排后续 reaction。
将主应用的 mount-scoped Signal 直接当成独立后台应用的长期数据 owner 会耦合二者生命周期，
因此共享数据优先由外层业务服务持有。

## 3. 启动、反馈与完成

宿主持有并监督后台 driver，发起后不要求主流程等待。driver 可以只执行一次 `react()`，
也可以用 `run()` 完成需要多次 observe -> act -> feedback 的应用交互；这是具体应用的选择。

输入和输出通过业务消息交换，使用业务 work identity 区分不同任务。进度、实际结果、失败
或取消状态由后台应用 / owner 发布给接收方。主应用收到消息后更新自己的 view，并在后续
Frame 中交付，模型才能据此继续行动。收到一条消息或写入共享 store 本身不构成模型观察。

普通业务结果与 runtime 完成分别表达。`react()` 返回 `Continue(())` 不代表业务任务成功，
`run()` 返回也不证明资源已经清理。主应用若需要向模型显示“后台工作已完成”，需要真实的
业务结果；若对外承诺 owner 已关闭，还要等待 shutdown 完成。

## 4. 生命周期保证

- 宿主保留后台 driver 的 owner 和完成句柄，不能仅丢弃 `JoinHandle` 使其无人监督。
- 每个 Application 的 mount、driver 和 Component tasks 遵守现有 Tokio runtime identity 约束。
- 软退出使用该应用的 exit handle；已经取得提交许可的 reaction 仍会结算。
- 需要主动取消在途 reaction 时，driver 在仍持有 Application 的条件下取消当前 operation，
  再按既有恢复、业务收尾和 consuming `shutdown()` 契约完成清理。
- 一个应用普通失败不隐式取消另一个应用；宿主明确决定后续调度。共享服务中的已发生效果仍然保留。
- 普通 Result 错误不能通过提前返回而跳过 owner cleanup；panic 保留现有传播及不可复用契约。
- 宿主关闭时收回所有 Application，完成各自必要的取消和 shutdown 后才确认关闭。

## 5. 与 Parallel Compaction 的关系

两个功能仍以 `ReactionPort` 为边界：

| 层 | 并发方式 |
| --- | --- |
| Port 上方 | 宿主并行驱动多个独立 Application |
| Port 下方 | Provider 并行运行普通生成和 compaction 请求 |

下层实现与配置见 [parallel-compaction-design.md](parallel-compaction-design.md)。
它不依赖新的 popup runtime、共享 Component tree 或 Application 内并行 reaction。

## 6. 实际集成的验证

后续接入具体后台应用时，验证一侧 reaction 保持 Pending 期间另一侧仍可完成交互，并覆盖：

- 两侧的 Frame、工具记录和 provider continuation 不串线。
- 后台进度或结果进入主应用后续 Frame，主模型可以据此执行下一步。
- 共享业务数据的更新通过接收应用自己的状态与 driver 交付。
- 一侧退出、失败或取消不遗留无人负责的工作。
- 宿主 shutdown acknowledgement 等待所有应用清理。
