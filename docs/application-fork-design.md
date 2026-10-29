# 保留历史的 Application Fork

状态：已实现 `Application::fork()`、`fork_with_commands()` 和 Responses port 支持。
[engine.md](engine.md) 是权威 runtime contract；本文记录接口、所有权与验证场景。
可运行的[离线示例](../examples/application_fork.rs) 无需 API 凭证。

## 1. 目标与边界

从当前交互历史创建独立的 `Application`，让父应用与分支可以并行 reaction。
每个 Application 内继续 single-flight；各自独立退出、取消和 shutdown。

fork 保留分支点之前的 canonical history，以及使这些历史能够继续用于 provider 请求的私有状态。
应用显式提供业务状态 snapshot，并通过新的 root 恢复它。Component tree、Signals、handlers 和
任务作用域重新建立，不隐式复制任意 hook value、运行中的 future 或外部副作用。

这为[并行后台 Application](background-applications-design.md) 增加一种保留历史的启动方式。
现有 `mount()` 继续创建新模型会话；后台应用默认不继承父应用历史。

## 2. 公共 API

fork 是 provider 的可选能力，不给所有 `ReactionPort` 增加 `Clone` bound：

```rust,ignore
#[async_trait]
pub trait ForkableReactionPort: ReactionPort + Sized + 'static {
    async fn fork(&mut self) -> Result<Self, ReactionPortFault>;
    fn validate_fork_frame(&self, frame: &Frame) -> Result<(), ReactionPortFault>;
}

impl<P: ForkableReactionPort> Application<P> {
    pub async fn fork(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
    ) -> Result<Self, ApplicationFault>;

    pub async fn fork_with_commands(
        &mut self,
        root: impl Fn() -> Component + Send + Sync + 'static,
        input: CommandInput,
    ) -> Result<Self, ApplicationFault>;
}
```

root closure 捕获应用导出的 owned typed snapshot，作为新组件的初始化
输入；框架不暴露 mutable FrameSession 或 provider-private snapshot。命令分支接收新的输入通道，
不复制父应用的 inbox 或正在处理的 command。

`&mut Application` 保证 fork 不与该实例的 `prepare()`、`react()` 或其他 fork 重叠。
它不会冻结独立运行的业务任务或外部数据库。多字段业务 snapshot 的一致性、版本以及与历史分支点
的业务对应关系由业务 owner 保证；应传入拥有独立值的 snapshot，而不是父 mount 的 Signal handle。

## 3. 成对保留模型会话

模型会话的复制单位是 `FrameSession + port` 的一致快照。只复制 port 再调用普通 `mount()` 不够：
新 FrameSession 从空 canonical history 开始，而 port 保留的 wire coverage 仍指向旧 canonical prefix。
首个 Full 会在历史前缀校验处失败，不能通过跳过校验或清空历史来接续。

| 状态 | 分支的处理 |
| --- | --- |
| Canonical transcript | 保留分支点之前已接纳的结构化历史，随后独立追加。 |
| Provider wire history、reasoning 与已安装 compaction | 保留与同一 canonical prefix 一致的私有状态及证明。 |
| Target identity、FrameSession namespace 与 revision sequence | 创建新的身份和序列；不把父 Accepted revision 声明为子 target 的 revision。 |
| Projection checkpoint 与 reconciliation provenance | 保留可证明的历史归属信息；按新 mount scope 处理，不能直接当作子树已交付的 baseline。 |
| 子应用 diff baseline | 重新建立；首次 handoff 必须是 Full。 |
| Signals、handler bindings、preparation 与 Component tasks | 从显式业务 snapshot 重新 mount，使用新的生命周期与绑定。 |
| Exit、driver demand、command inbox、streaming supervisor | 创建独立实例，不继承父应用的待处理请求或 recovery。 |

子 port 声明新的 target 和 `FullRequired`，但保留可用于验证 Full 的父历史私有副本。子 FrameSession
保留对应 canonical prefix，并为新 target 编译 Full。第一次 successful handoff 才安装子 revision
与新的 delivery baseline；Pending、拒绝或取消不推进它们。之后各自沿用正常 Full/Delta 规则。

新 mount 的 `execution_scope` 与父应用不同。导入过程必须沿用现有 scope transition 和 provenance
检查：历史记录不能在 remount 时变成新的 provider action，工具副作用也不能重新执行。新 root
声明的当前业务内容可以作为新输入交付；不保证它与继承历史中的 authored message 自动去重。
无法证明归属的 provider/tool 来源重声明，包括 assistant output、provider extensions 与工具记录，
继续明确拒绝；不能清空 reconciliation ledger 绕过歧义。

System 是独立 snapshot，子 root 必须显式声明当前 System；Full 中省略它表示 clear，不能指望从
普通 canonical transcript 自动恢复。历史 ephemeral 原始 prompt 不进入继承的 canonical/wire history；子 root
声明的当前 ephemeral 内容在后续请求按原规则交付。历史里已保留的模型输出和工具事实仍然继承。

## 4. 首版允许的分支点

fork 在 parent 的 outer driver boundary 检查之后进行。源应用须可用，并满足：

- 没有 active reaction、preparation、退出请求、未完成 mount retirement 或正在处理的 command；
- 没有 managed streaming settlement/recovery fence；
- canonical history 中的工具调用已闭合，ToolOutput staging 也为空；
- provider 能给出真实且可分支的 continuation，私有因果数据完整。

`react()` 已经返回不充分：工具 handler 的结果可能仍在 staging，等待下一 Frame handoff；取消时
生成的 unknown-outcome fallback 也可能尚未交付。这些状态在首版明确拒绝 fork，不隐式提交一帧、
消费 receipt 或重跑操作。父应用由自己的 driver 正常继续，满足条件后再显式 fork。

对 ready source 的检查不消费 reaction demand，不调用业务 preparation，也不自动开始 reaction。
调用需要当前 Tokio runtime；port fork await 之后和子任务启动之前重新仲裁源任务状态及退出请求。
fork 的普通失败或取消不改变父 history、delivery cursor、port continuation 或工具结果。
direct panic 和 supervised task panic 继续沿用现有传播与不可复用契约。

## 5. Port 能力与失败处理

具体 port 负责证明其私有 continuation 可分支。复制连接配置或实现 `P: Clone` 本身不提供这个证明。
无法实现独立分支的 Skill、Plugin 或远程 session 可以不实现 `ForkableReactionPort`。

Responses 的 `ResponsesFrameRequestState` 包含 wire input、instructions、canonical
prefix proofs、pending-call bookkeeping 与 compaction binding；reasoning 和 compaction artifact
也保留在该 wire state 中。fork 复制这组状态，并验证闭合条件；需要恢复的中断 continuation
和未完成的私有 artifact 会被拒绝。

- HTTP client 和只读配置可以共享，分支的可变 wire state、故障状态和 target identity 独立。
- 正在运行的请求、fact stream 和后台 compaction worker 不复制；父 worker 继续由父 port 拥有。
- 子 port 保留 compaction configuration，并通过 `Arc<dyn CompactionPolicy>` 共享显式的线程安全
  policy 对象，包括对象自身的内部状态。各自替换 policy、worker、monitor 和候选安装状态独立。
- 已安装 compaction 的 System binding 继续生效。新 root 的 System 与 opaque artifact 不兼容时，
  应明确拒绝或使用 provider 已证明可行的重建方式，不能仅清空 proof 后复用旧 artifact。
- 子 port 的普通构造失败或 fork 取消必须清理子资源；不能 shutdown 源 port 或遗留无人负责的工作。
  如果清理需要 await，它由独立的 cleanup owner 持有；取消 fork waiter 不得取消这项清理。

Application 将 canonical snapshot 与 port snapshot 作为一组校验、构造，再发布子 owner。
历史导入与所有可预检的兼容性检查必须在启动子 Component tasks 前完成，避免因校验失败而先启动
业务工作。`validate_fork_frame()` 对初始 Full 检查 target/profile、exact canonical prefix、私有
artifact、System 与 wire budget，不提交或推进 continuation。初始 projection 可以含 preparation；
预检不运行 preparation，实际 reaction 仍重新校验当时的 Frame。
成功 mount 按现有规则启动新的 Component tasks；fork 不调用 provider `submit()`，
不启动子 reaction 或新的 compaction attempt。后续 driver 与 shutdown 均由宿主负责。

部分创建的子资源交由独立 cleanup task；父 owner 保存其完成句柄。取消 waiter 不取消清理；
父应用的正常 reaction 可以继续，后续 fork 和 consuming shutdown 等待清理；consuming shutdown
即使遇到 panic 也先等完其拥有的 fork cleanup，再传播原始 shutdown panic。未发布子应用先
fence mounts、join tasks、shutdown port，再传播尚未消费的子任务 panic，保证 panic 不跳过异步收尾。

错误沿现有 payload-free Application/port fault 边界返回。`ApplicationFaultStage::Fork` 和明确的
reason 区分源状态未结算、私有快照不可分支、快照不兼容与 cleanup 失败。`ForkNotReady` 是
retryable；direct panic 和 supervised panic 保留原 payload，并使源 Application 不可复用。

## 6. 结果、反馈与生命周期

分支继承的是过去的共同历史。fork 返回之后，两侧的 assistant output、工具记录、业务状态更新
和退出请求不自动合并。需要协作时，通过显式业务消息或外层拥有的服务交换进度与结果。

父应用接收分支的实际结果、失败或取消状态，更新自己的 typed view，在后续 submitted Frame 中交付，
模型才能根据结果选择下一步动作。写入共享 store、Signal 或日志本身不构成模型观察。
分支也通过自己的后续 Frame 看到动作结果和最新状态。

宿主持有两个 Application owner，监督各自的 driver，执行各自业务收尾并 consuming shutdown。
一侧退出、失败或取消不隐式终止另一侧；关闭宿主时要等待双方 active work 与资源清理完成。
fork 不复制或撤销已经发生的数据库、文件和其他外部副作用。

## 7. 验证

验证 typed POM、Frame、结构化历史、状态转移与真实交付，不解析或快照 rendered prompt。

1. 共同历史包含普通对话、reasoning 和合法工具调用/结果；fork 后首个 Full 保留对应上下文，
   exact prefix 校验通过，旧工具 handler 不再执行，后续 Delta 只沿各自 lineage 推进。
2. 一侧 reaction 保持 Pending 时，另一侧可以完成 observe -> act -> feedback -> next action；
   双方看到自己的动作结果，output identity、工具结果和 continuation 不串线。
3. 父子 target identity、namespace、mount、Signal、exit 和输入通道独立；关闭一侧不使另一侧失效。
4. 新 scope 的 diff baseline 重新建立；无法证明归属的 provider/tool 来源重声明被拒绝；System/compaction
   不兼容继续拒绝，不通过移除 proof 或 canonical prefix 校验放行。
5. 已结束 reaction 留下 staged output、取消 fallback、streaming recovery 或未闭合调用时拒绝 fork；
   父状态、receipt、demand 和提交次数不变。源正常完成后仍可继续或再次 fork。
6. Port fork、子 mount 和历史校验的普通失败，以及 fork future 取消，均清理子资源并保留父会话。
   panic 沿用现有原 payload 传播规则。
7. 父 compaction worker 不被复制或停止；子 worker 与 monitor 独立。宿主的 shutdown acknowledgement
   等待两侧清理完成。

覆盖位于 FrameSession、Application fork 和 Responses provider 单元测试中。
[application_fork](../examples/application_fork.rs) 使用真实 Responses port 与本地确定性 SSE fixture，
展示 owned snapshot、共同 assistant history、并行进度、结果回传、下一动作和后续反馈交付：

```sh
cargo run --no-default-features --example application_fork
cargo test --no-default-features --example application_fork
```
