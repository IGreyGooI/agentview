# AgentView 设计约束

运行时契约以 [docs/engine.md](docs/engine.md) 为准。

## Compaction 统一使用 local compaction

**AgentView 的所有 compaction 都是 local compaction。摘要必须使用当前配置的模型进行普通推理。**

这里的 local 表示由 AgentView 在客户端管理触发策略、历史快照、摘要提示词、结果校验和
上下文替换，不表示模型必须运行在本机。摘要通过当前 provider 的普通推理接口执行，使用与
前台 reaction 相同的 `ModelSpec`、reasoning 配置、连接配置和认证；不另选或硬编码摘要模型。

- 后台 parallel compaction 和显式启用的 forced compaction 共用这一条本地摘要路径。
- 不调用 `/responses/compact` 或任何 provider 专用压缩接口。
- 不发送 `context_management`、`compaction_trigger` 或其他服务端压缩配置。
- 不把服务端压缩作为默认实现、可选分支或失败后的 fallback，也不依赖不透明的服务端压缩产物。
- 摘要请求只生成文本，不提供业务工具，不派发 Component action，不推进前台 continuation。

parallel 表示对稳定历史快照的摘要推理与前台 reaction 并行。后台只产生候选；完成后由前台在
下一次真实交付时安装，并保留摘要期间新产生的完整 tail。候选失效、失败或没有缩小上下文时，
保留原上下文。单个 provider 至多一个摘要任务；reset、取消和 shutdown 必须管理其生命周期。

forced compaction 仅在用户显式启用后，允许超窗的前台请求等待一次有界的同模型摘要尝试。
它不改变 compaction 的本地实现方式。摘要请求自身也遵守当前模型窗口、输出预留、字节预算和超时。

默认策略与自定义策略只决定何时启动 local compaction。具体计量、覆盖关系、安装及失败语义见
[parallel-compaction-design.md](docs/parallel-compaction-design.md)。未来 runtime checkpoint 的
[设计](docs/runtime-compaction-design.md) 也必须遵守上述约束。
