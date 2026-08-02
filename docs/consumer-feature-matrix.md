# AgentView 消费者 Feature 支持矩阵

> 产品级 Feature、责任边界和验收语义见
> [AgentView 面向 LLM Application 的 Feature List](agentview-feature-list.md)。本文保留底层能力
> 映射、消费者缺口和具体 test receipt，不作为顶层产品目录。

## 1. 范围与状态定义

本文跟踪当前 AgentView 工作区能否支持三个真实消费者：

- Cube Stage Script Writer 与 Director
- Forgotten City player、NPC 与 GM agents
- Forgotten City semantic graph 摄取与查询 runtime

判定以消费者为中心。当前代码和可执行测试是事实来源；roadmap 只提供背景，不能单独
证明支持状态。

状态定义：

| 状态 | 含义 |
| --- | --- |
| `supported` | 已有 public API，并且消费者形状的可执行测试通过。 |
| `internal-proof` | 行为已在 AgentView 内实现和测试，但所需 owner 或 facade 仍为 crate-private。 |
| `adapter-required` | AgentView 已有语义原语，但仍需消费者专用 provider、persistence 或 effect wiring。 |
| `pending-verification` | 当前工作区正在修改该实现，必须通过具名测试后才能赋予其他状态。 |
| `missing` | 所需语义契约尚未实现。 |
| `not-applicable` | 该消费者不需要此 feature。 |

仅有 isolated unit test 不足以把 feature 标记为消费者 `supported`。它还必须能通过目标
public boundary 到达，并保留消费者可观察行为。

### 架构视图与测试口径

- [AgentView Consumer Readiness](consumer-support.html) 展示三个消费者的当前路径、
  mounted 目标路径和 release gaps；图源是
  [consumer-support.architecture.json](consumer-support.architecture.json)。
- Archify receipt：`standard` validation 9/9，0 error、0 warning；light/dark 浏览器像素
  检查通过；两轮局部布局修正。
- 本轮新增 `component_public_authoring` 的 public local-host lifecycle、factory-scope、
  `MountedFeature` composition/reopen 和 Live/cancel 证据，以及 mounted driver / direct harness render 的 compile-fail
  边界。下方结果只在具名命令退出为 0 后记为通过；命中另一个 agent 的半写快照时，
  该结果作废，文件稳定后重跑。

## 2. 核心 Feature Catalog

| ID | Feature | 状态 | 当前证据与边界 |
| --- | --- | --- | --- |
| `AV-POM-01` | 支持 Markdown/XML 的 typed System/User POM | `supported` | 已有 public `Document` authoring、derive、按 role resolution、canonical rendering 和 golden tests。 |
| `AV-POM-02` | 显式 full/delta/delete view 与事务性 cursor commit | `supported` | 已有 public `DiffSlot`、`UserDocumentCursor`；被丢弃的 preparation 及失败的 provider/commit 不推进 cursor。mounted external proof 进一步锁定 consumer ack 才推进 cursor，presentation 不推进。 |
| `AV-COMP-01` | typed props 与 lifecycle-safe children 的函数式组件组合 | `adapter-required` | 已有 public `#[view(component)]`、`PomView`、`Component`、`DurableComponent`、纯 borrowed `.project_props(...)` 和 compile-fail coverage；external public-owner tests 已证明 projected `MountedFeature` 同时贡献 ordered System/runtime 与 retained per-turn User tree、通过 `map_channels` 把 feature-local streaming Live contract 提升到 harness root，并在 reopen 时不执行 feature 的 System/User renderer。`#[view(component)]` 现在会把 `MountedFeature` 的 name/optional key 保留到 durable System 和 fresh User projection；black-box tests 覆盖 positional sibling、duplicate key、keyed parent with multiple User children、跨 reopen stable `BindingId`，并拒绝同一 epoch 下的 key drift。capture/provider/store 仍不属于 feature。 |
| `AV-AGENT-01` | public transactional `Agent` `Wait`/`Continue` loop | `supported` | compatibility runtime 支持 history replacement、late sink binding、retry-safe draft、loop bound、observer 和 cancellation rollback。 |
| `AV-APP-01` | 外部 `observe` / `hook` / `act` application loop | `supported` | public `AgentViewApp` 提供 epoch fencing、full/partial view、stale-turn rejection 和 wake subscription。mounted external path 保持普通 `PromptComponent` authoring，并只在 advanced `MountedExternalHarnessDefinition::new(root, reply, epoch_id)` 边界绑定 grammar/pure decoder；Actionable/Passive disposition 与 props/revision 由 host 一次捕获，User publication/ack/CAS 仍属于 advanced host。 |
| `AV-MOUNT-01` | 单一 retained durable System definition 与 Create/reopen 分离 | `adapter-required` | Public local host 证明 Create/reopen 的 System-once lifecycle；advanced external controller test 进一步要求 Create 仅将 System 交给 host port，host 先持久化 `ExternalSystemDeliveryReceipt` 或有序 outbox identity，reopen 只复用同一 receipt 而不泄漏 raw System。两者仍不是 production store/provider/transport adapter。 |
| `AV-MOUNT-02` | 每次 preparation/continuation 重新渲染 User | `adapter-required` | Public local host 已证明两个新 call 分别 capture/render User，replay/reopen 不渲染 User；真实消费者尚未迁移。 |
| `AV-XML-01` | 增量 XML `open` / `stream` / `complete` / strict EOF reducers | `internal-proof` | public local host 已通过真实 provider wire 驱动 XML open；Forgotten City mounted SelectIntent 已用 consumer-shaped contract 覆盖 wire order、validation、fresh state 和 terminal behavior。opt-in SQLite/OpenAI host 已从该 wire path 走到 `PlayerRuntime`，但没有真实-provider 或完整 failure trace。 |
| `AV-CHAN-01` | typed Output、Live、Commit、Diagnostic lanes | `internal-proof` | public local host 已端到端传递 typed Live；其他 lanes 有 mapping 与 compile-fail coverage，但尚无真实消费者 channel schema。 |
| `AV-LIVE-01` | awaited Live effects 与显式 abort compensation | `internal-proof` | external local-host test 已证明 provider wire 等待 Live apply，cancel 会 join provider 并执行 Live abort；Forgotten City selector 已证明 ordered delivery、revocable fence、reverse compensation 和 runtime stale-delivery fencing。opt-in host 已有 recorded success delivery，runtime clear 会等待远端 cancellation，completed turn 可以经 replacement host reopen；indeterminate-turn recovery trace 仍缺。 |
| `AV-TOOL-01` | provider-native grouped tools 与即时 typed results | `local-proof` | 已实现 pure author capability contract、versioned host dispatcher registry、generic `MountedHostBindings` open、Create/reopen manifest preflight、ordered dispatch、shared dispatcher state、model-visible expected error、terminal infrastructure error 和 attempt-local replay/collision。 |
| `AV-PROVIDER-01` | stateful provider epoch attach、POM-free rehydrate 与 turn cursor | `adapter-required` | public provider traits 与内部 owner tests 已存在；ordinary attacher 已从 durable executor 拆开；AgentView coordinator 与 Forgotten City adapter 都证明 lost attach reply 后从同一 rendered artifact resume 且不重复物理 System install，另覆盖 cursor-only rehydrate、User-only turn 和 joined cancellation。SQLite/OpenAI Conversations/Responses adapter 已接到 opt-in host，但真实 provider/transport 验证仍缺。 |
| `AV-CANCEL-01` | call-scoped cancellation 与 joined provider/runtime cleanup | `adapter-required` | External local-host test 已证明 dropped waiter、start-future drop guard、typed Live compensation、provider join、`Indeterminate -> RecoveryRequired`、invalid `ResumeFrom` recovery、`Unchanged -> successor admitted`，以及 valid `ResumeFrom` cursor 经过 successor/reopen。owner tests 另覆盖 reason mismatch 和 timeout；真实 process-local store 的 7 个直接测试覆盖 revision、publication、epoch、stored/replacement-cursor、lease、foreign-fence faults 和 exact-fence recovery retry；Forgotten City runtime queue 证明 stale Live fencing。仍缺 public id-keyed control、production persistence 和真实 transport adapter。 |
| `AV-PUB-01` | pure Commit staging 与 atomic session/outbox publication | `internal-proof` | 已有稳定 request/item id、fingerprint、CAS、幂等 resolve 和 cancellation-safe publication actor；没有 production persistence。 |
| `AV-CALL-01` | durable call admission、lease、checkpoint、replay 与 recovery state | `internal-proof` | public local host 覆盖 admission、settled replay 与 input mismatch；private store/owner tests 继续覆盖 lease、checkpoint 和 `RecoveryRequired`。强 `NeverAccepted` provider status 现在可通过 opaque proof 驱动 exact-fence store recovery，并覆盖 New、continuation、legacy ambiguous origin 与 durable-backend CAS；跨进程 controller/supervisor 尚未接线。 |
| `AV-RECONF-01` | durable System epoch reconfiguration | `internal-proof` | private 实现覆盖 admission、rejected tombstone、crash recovery、activation、superseded status 和 POM-free retry。 |
| `AV-OBS-01` | turn lifecycle observation | `supported` | public compatibility loop 暴露 preparation、request、assistant、failure 和 flow event；消费者专用 durable event schema 仍需 adapter。 |
| `AV-FACADE-01` | public mounted `open` / `start` / `wait` / `cancel` / `reload` facade | `adapter-required` | `InMemoryMountedAgentFactory` 通过 external black-box lifecycle：两次执行、replay、input mismatch、reload、clone/drop/reopen、Live/cancel；该结论仅覆盖 process-local host。真实消费者仍需 provider、persistence 与 effect adapter。 |
| `AV-STORE-01` | production session store、lease renewal 与 recovery-scanned outbox worker | `missing` | 目前只有 trait 和 in-memory proof；未发布 mutation/outbox payload recovery 与外部 delivery policy 不完整。 |
| `AV-COMPAT-01` | 消费者 golden-trace compatibility harness | `missing` | 尚无测试同时比较 prompt bytes、streamed events、tool transcript、observer events 与 durable side effects。 |

## 3. 消费者需求矩阵

### Cube Stage

| 需求 | AgentView features | 当前状态 | 消费者证据或缺口 |
| --- | --- | --- | --- |
| 不依赖 raw template fragment 编写 Director prompt | `AV-POM-01`, `AV-COMP-01` | `partial` | `src/director/rig_agent.rs` 已通过 current `build_system_document`/`build_user_document` 和 `AgentViewValue` 生成 prompt；`cargo check --all-targets` 与 80-pass library suite 通过。mounted runtime 仍未接入。 |
| Director policy/tool schema 只 mount 一次，每步重渲染 board | `AV-MOUNT-01`, `AV-MOUNT-02` | `adapter-required` | public local-host lifecycle 已通过 external test；Director 已完成 POM migration，但尚无 Cube-shaped mounted/provider test。 |
| 顺序执行 provider-native tool call 并立即返回 correlated result | `AV-TOOL-01`, `AV-LIVE-01` | `internal-proof` | isolated dispatcher 保留顺序、correlation、expected error 和 awaited Live；Cube 仍从 legacy `TurnSink` 调用 `ToolServer`。 |
| 持续执行 model turn，直到 `complete_task` / `finalize_dialogues` 或 step limit | `AV-AGENT-01`, `AV-MOUNT-02` | `adapter-required` | legacy loop 当前支持；迁移前 public mounted call 必须拥有完整 `Wait`/`Continue` loop。 |
| retry provider completion 时不重复已接受的 tool I/O | `AV-PROVIDER-01`, `AV-CALL-01` | `adapter-required` | Cube 对 transient completion 最多重试三次；adapter 必须区分 tool 前 transport retry 与 durable tool invocation replay。 |
| 一致地持久化 transcript、observer event、timeline mutation 与 external effect | `AV-PUB-01`, `AV-OBS-01`, `AV-STORE-01` | `missing` | 已有 attempt-local replay，但 Cube event-store idempotency、durable ToolServer mutation 和 outbox delivery 尚未定义。 |
| 保留现有 prompt、tool-result、observer 与 SQL-visible 行为 | `AV-COMPAT-01` | `missing` | 尚无 migration golden trace。 |

### Forgotten City Agents

| 需求 | AgentView features | 当前状态 | 消费者证据或缺口 |
| --- | --- | --- | --- |
| 保持当前 POM-first compatibility Agent 可编译 | `AV-POM-01`, `AV-POM-02`, `AV-AGENT-01` | `supported` | `cargo check -p engine --all-targets` 已对当前 AgentView 工作区通过。 |
| System policy/tool contract 固定，同时 User context、artifact、feedback、task 可变 | `AV-MOUNT-01`, `AV-MOUNT-02` | `adapter-required` | `GameEngineConfig::mounted_openai` 已可显式把 Player SelectIntent 路由到 session-scoped SQLite/OpenAI mounted host；默认 engine、NPC 和 GM 仍使用 compatibility path，且缺完整 game-loop golden trace。 |
| 从 XML open 按 wire order 立即发出 `SelectIntent` 工作 | `AV-XML-01`, `AV-CHAN-01`, `AV-LIVE-01` | `adapter-required` | mounted selector 已通过真实 wire path 发出 typed Live，并由 `PlayerRuntime` sink 做 ordered delivery/revocation；显式 mounted host 已接线，仍需 authoritative revalidation 与完整 failure/cancellation golden trace。 |
| provider/parser/turn 失败后取消全部 tentative SelectIntent phraser/option | `AV-LIVE-01`, `AV-CANCEL-01` | `adapter-required` | selector adapter 与 runtime-level test 已证明 reverse compensation、joined cancel 和 stale-delivery fence；parent/child phraser scope、durable cancel settlement 与 trace-equivalence test 仍缺。 |
| 流式处理 Phrase open/append/close，并在 publication 前保持 option tentative | `AV-XML-01`, `AV-LIVE-01`, `AV-PUB-01` | `adapter-required` | primitive 已存在；stable phrase slot、显式 compensation 与 ready-after-publication 语义尚未集成。 |
| 复用一个 stateful provider epoch，物理上不重发 System | `AV-PROVIDER-01` | `adapter-required` | SQLite-backed `GameEngineConfig::mounted_openai` 使用 OpenAI Conversations/Responses adapter；recording-server tests 证明一次 System attach、普通 User-only request 与 replacement-host POM-free reopen。默认 Rig 仍按 request 重建 agent，真实 provider 和 production transport 尚未验证。 |
| 支持 NPC/GM native tools、scheduling 与 multi-turn behavior | `AV-TOOL-01`, `AV-MOUNT-02` | `adapter-required` | capability model 适配，但尚无 NPC/GM harness 完成迁移。 |
| restart 后恢复 session、call、publication 与 reconfiguration state | `AV-CALL-01`, `AV-RECONF-01`, `AV-STORE-01` | `missing` | mounted host 已把 AgentView state 与 OpenAI operation ledger 持久化到 SQLite，并有 replacement-host rehydrate proof；selector `Commit = Never`，尚无 Forgotten City domain transaction/outbox worker、crash supervisor 或 public recovery facade。 |
| 保留 prompt、parser、world-event、text-stream 与 cancellation trace | `AV-COMPAT-01` | `missing` | SelectIntent 已比较 System、initial/retry User 与双方均接受的 XML callback trace；仍缺 invalid-input migration policy、world/text/cancellation trace 与 Phrase comparison。 |

### Forgotten City Semantic Graph

| 需求 | AgentView features | 当前状态 | 消费者证据或缺口 |
| --- | --- | --- | --- |
| 把 graph/source state 投影为 typed full/delta view | `AV-POM-01`, `AV-POM-02`, `AV-APP-01` | `supported` | `agent_runtime --all-targets` 与 210 项 database-free tests 通过，包含 full/delta graph/source workspace view。 |
| 用 epoch fencing 驱动外部 `observe` / `act` session | `AV-APP-01` | `supported` | `AV-T04` 的 stale-turn/epoch/cursor tests 与 semantic graph 当前 runtime 编译、local suite 均通过。 |
| 在 bounded multi-turn loop 中执行 graph read/query/mutation/commit tools | `AV-AGENT-01`, `AV-TOOL-01` | `supported` | 当前 compatibility sink/Rig `ToolServer` 的 read、query、mutation、commit、finish 与 bounded retry tests 通过；不包含 mounted migration。 |
| replace/compact history 时不提交被丢弃的 view cursor | `AV-POM-02`, `AV-AGENT-01` | `supported` | AgentView replacement rollback 与 graph context-compaction/working-set tests 均通过。 |
| 通过 PostgreSQL commit/resume graph snapshot | consumer-owned persistence | `pending-verification` | `postgres_persistent_loop` 是 authoritative integration test，需要 `DATABASE_URL`。 |
| process restart 后不 replay indeterminate provider/tool turn | `AV-CALL-01`, `AV-PUB-01`, `AV-STORE-01` | `missing` | 已有 graph snapshot resume，但当前 mounted runtime 不提供完整 provider-turn mutation/outbox recovery。 |
| 保留 graph mutation、provenance、observer 与 snapshot semantics | `AV-COMPAT-01` | `missing` | 尚无 cross-runtime golden trace。 |

## 4. 可执行测试清单

按以下顺序执行。当前置 tier 的必要测试失败时，后续 tier 不能把 feature 升级为
`supported`。

### Tier A：AgentView contracts

| Test ID | Features | 命令 | 当前结果 |
| --- | --- | --- | --- |
| `AV-T01` | `AV-POM-01`, `AV-POM-02` | `cargo test --test pom_ast --test pom_user_document --test context_preparation` | 通过：62 项 |
| `AV-T02` | `AV-COMP-01` | `cargo test --test component --test component_compile_fail --test component_public_compile_fail --test component_public_authoring` | 通过：22 项 runtime、31 个 component compile-fail case、9 个 public-boundary case（raw-IR default gating、durable-provider attach split、stateful rehydrate System exclusion，以及 component/root prelude 不导出 legacy turn bridge、mounted host integration、external control 或 async provider-dispatcher binding）、38 项 public authoring（含 prompt-only facade、fallible User preparation、ordinary/durable child props projection、pure provider registry preflight、`MountedFeature` composition/channel mapping/reopen、positional/duplicate/keyed-parent identity 与 durable harness-owned loop bound） |
| `AV-T03` | `AV-AGENT-01` | `cargo test --test component_agent --test agent_session --test context_preparation` | 通过：22 项 |
| `AV-T04` | `AV-APP-01` | `cargo test --test agent_view_app --test control_view_state` | 通过：10 项 |
| `AV-T05` | `AV-MOUNT-01`, `AV-MOUNT-02` | `cargo test --test component_lifecycle --test component_public_authoring` | 通过：9 项 lifecycle；public local-host 38/38，包含 fallible User preparation、同 `EpochContractId` 的 System preservation、durable props projection、pure provider registry preflight、`MountedFeature` channel mapping、POM-free reopen、positional/duplicate/keyed-parent identity、keyed feature contract rejection、durable harness-owned loop bound、call-side tightening、safe-cancel successor release、invalid resume-cursor recovery 与有效 resume-cursor 的 successor/reopen preservation。 |
| `AV-T06` | `AV-XML-01`, `AV-CHAN-01`, `AV-LIVE-01` | `cargo test --test component_mounted_streaming --test component_streaming` | 通过：29 项 |
| `AV-T07` | `AV-TOOL-01` | `cargo test --test component_provider_dispatch --test component_provision` | 通过：24 项 |
| `AV-T08` | `AV-PROVIDER-01` | `cargo test component::durable_epoch::coordinator_tests --lib` | 通过：10 项 |
| `AV-T09` | `AV-CANCEL-01` | `cargo test component::managed_attempt::tests --lib` | 通过：13 项 |
| `AV-T10a` | `AV-PUB-01` | `cargo test component::publication::tests --lib` | 通过：11 项 |
| `AV-T10b` | `AV-PUB-01` | `cargo test component::managed_publication::tests --lib` | 通过：10 项 |
| `AV-T11` | `AV-CALL-01`, `AV-RECONF-01` | `cargo test component::mounted_agent::tests --lib` | 通过：127 项 |
| `AV-T12a` | `AV-CANCEL-01` | `cargo test component::local_mounted::tests --lib` | 通过，20/20；直接验证真实 process-local store 的 cancellation、recovery、provider-registry 与 fault/retry parity。 |
| `AV-T12b` | `AV-FACADE-01` | `cargo test --lib public_` | 通过，5/5 |
| `AV-T12c` | `AV-FACADE-01` | `cargo test --test component_public_authoring` | 通过，38/38 |
| `AV-T12d` | `AV-APP-01`, `AV-MOUNT-01`, `AV-MOUNT-02` | `cargo test --test component_external` | 通过，8/8；覆盖 System-once、host-owned immutable User outbox、receipt/bytes exact replay、ack-before-act、first-full/ack/delta、passive full/non-advancing、安全取消后的 full、typed decode、source stale、recovery/race 与 generation fencing；schema-v4 state 在 v5 contract 下 fail closed。该 fake port 不证明 production transport。 |
| `AV-T13a` | 全部 | `cargo fmt --all -- --check` | 通过（2026-07-31） |
| `AV-T13b` | 全部 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 通过（2026-07-31） |
| `AV-T13c` | 默认 surface | `cargo test --workspace --all-targets` | 通过（2026-08-01；lib 299 项及全部默认 integration、compile-fail、example targets） |
| `AV-T13d` | raw-IR compatibility | `cargo test --all-targets --features raw-component-ir --quiet` | 通过（2026-07-31；legacy raw IR tests/examples 与 default contracts 同时编译） |

### Tier B：消费者编译与本地行为

| Test ID | 消费者 | 命令 | 当前结果 |
| --- | --- | --- | --- |
| `CUBE-T01` | Cube Stage | `cargo check --all-targets` | 通过（2026-07-31）；Director current POM path 和 test cfg 均已编译。 |
| `CUBE-T02a` | Cube Stage | `cargo test --lib director::tests` | 包含在 `cargo test --lib --quiet`：80 passed、18 ignored。 |
| `CUBE-T02b` | Cube Stage | `cargo test --lib script_writer::tests` | 包含在 `cargo test --lib --quiet`：80 passed、18 ignored。 |
| `FC-T01` | Forgotten City agents | `cargo check -p engine --all-targets` | 无 warning 通过；mounted OpenAI adapter 仍显式允许 dead code，因为默认 engine 不会隐式安装它。 |
| `FC-T02` | Forgotten City agents | `cargo test -p engine --lib` | 通过：49 项 |
| `FC-T02a` | Forgotten City mounted SelectIntent | `cargo test -p engine mounted_select_intent --quiet` | 通过：20 项；包含 runtime queue cancel/join/stale-delivery fence、`Indeterminate` cancel 进入 recovery 后对 different call-id successor 的 admission fence（无第二次 capture/User/provider work，失败 admission 不留下空 selection）、durable definition 的三轮 loop policy、drop/reopen no-second-System，以及 legacy/mounted System、initial/retry User POM 和 accepted-stream callback trace golden comparison。 |
| `FC-T02b` | Forgotten City stateful provider scaffold | `cargo test -p engine mounted_provider --quiet` | 通过：8 项；fake remote 证明 System-once/cursor rehydrate、User-only resync after rehydrate、`Unchanged` joined cancel、lost attach reply 后 POM-free `ResumeAttachment`、Running operation inspection、Completed operation 不被 cancel 改写，以及同一 adapter 驱动真实 mounted selector composition path。 |
| `FC-T02c` | Forgotten City mounted OpenAI host | `cargo test -p engine production_host_routes_a_streamed_selection_into_player_runtime --quiet`、`cargo test -p engine production_host_runtime_cancellation_joins_the_remote_response --quiet`、`cargo test -p engine production_host_replacement_rehydrates_runtime_without_a_second_system --quiet` | 通过：3 项；recording OpenAI server 经 SQLite state/operation ledger host 把一次 System attach、User-only response 和 typed streamed `Leave` selection 送入可见 `PlayerRuntime` option，并证明 runtime clear 会等到远端 response cancel、completed turn 的 replacement host 不二次 attach System。真实 provider 与 indeterminate-turn recovery 仍未验证。 |
| `GRAPH-T01` | Semantic graph | `cargo check -p agent_runtime --all-targets` | 重跑通过；首次半写快照结果已作废 |
| `GRAPH-T02a` | Semantic graph | `cargo test -p agent_runtime --lib --bins --test embedding_provider_client --test node_card_contract --test node_card_provider_client --test query_application --test query_tools` | 通过：210 项 database-free tests |
| `GRAPH-T02b` | Semantic graph | `cargo test -p agent_runtime --lib --tests` | 环境门控：先通过 196 项，随后 9 项 Node Card worker tests 因缺少数据库 URL 失败；不计为通过 |

### Tier C：环境门控集成测试

| Test ID | 消费者 | 命令或前置条件 | 必须断言 | 当前结果 |
| --- | --- | --- | --- | --- |
| `CUBE-T03` | Cube Stage | MySQL-backed Director integration tests | Tool transcript、observer events、timeline rows 与 final cursor 符合已接受行为。 | 未验证：需要 MySQL harness；POM compilation 不替代行为对比。 |
| `CUBE-T04` | Cube Stage | 带显式 credentials 的 real provider quality tests | Provider tool calls 有界，retry 不重复已接受的 ToolServer I/O。 | 未验证：需要明确 credentials；POM compilation 不替代 provider/tool 证明。 |
| `GRAPH-T03` | Semantic graph | `TEST_DATABASE_URL=... cargo test -p agent_runtime --test postgres_persistent_loop` | 已提交 graph snapshot reopen/resume 时没有 identity 或 cursor drift。 | 未验证：1 项测试因未设置数据库 URL 在 setup 阶段失败 |
| `GRAPH-T04` | Semantic graph | `TEST_DATABASE_URL=... cargo test -p agent_runtime --test node_card_worker` | Lease expiry、retry、shutdown drain、card promotion 和 embeddings 保持幂等。 | 未验证：9 项测试均因未设置数据库 URL 在 setup 阶段失败 |
| `FC-T03` | Forgotten City agents | provider/world-runtime test harness | SelectIntent/Phrase golden trace 覆盖 success、provider/parser failure、cancellation、timeout 和 Live host failure。 | harness 尚未实现 |

### Tier D：待新增的迁移等价性测试

| Test ID | 消费者 | 必须比较 |
| --- | --- | --- |
| `CUBE-G01` | Cube Stage | Legacy/mounted System/User prompt bytes、ordered tool calls/results、`TurnFlow`、observer events 与 SQL-visible mutations。 |
| `FC-G01` | Forgotten City SelectIntent | Legacy/mounted parser callbacks、selected-intent events、phraser start order，以及每个 failure point 后的完整 compensation。 |
| `FC-G02` | Forgotten City Phrase | Legacy/mounted visible text deltas、parsed close、publication-ready transition、cancellation 与 resource cleanup。 |
| `GRAPH-G01` | Semantic graph | Compatibility/mounted view deltas、tool transcript、graph mutations、provenance、observer rows 与 resumed snapshot identity。 |

## 5. 发布门槛

只有当某消费者的全部必要行均为 `supported`，且 build、local behavior 与 golden-trace
tests 全部通过时，AgentView 才能声明支持该消费者。

Cube Stage 支持要求：

- 完成 POM migration，不恢复 raw prompt compatibility shortcut
- 整个 Director/Script Writer loop 使用一个 durable System epoch
- 真实 provider-native ToolServer dispatch 能返回 correlated results
- externally visible tool mutation 具备 durable idempotency
- prompt/transcript/observer/SQL golden traces 全部通过

Forgotten City agent 支持要求：

- 至少一个 production player harness 使用 public mounted facade
- stateful provider adapter 证明 reopen 不重发 System
- provider-started call 的取消具有 exact-lease durable settlement/recovery，后续 call 不会被遗留 `Running` 永久阻塞
- SelectIntent scope cancellation 后没有残留 tentative option 或 phraser
- Phrase 具备显式 compensation 和 publication-gated readiness
- prompt/parser/world-event/text/cancellation golden traces 全部通过

现有 compatibility runtime 上的 semantic graph 支持要求：

- `agent_runtime` all-target compilation 与 local tests 通过
- `AgentViewApp` epoch/cursor/compaction tests 通过
- integration environment 中的 PostgreSQL commit/resume tests 通过

mounted runtime 上的 semantic graph 支持还要求完整 provider-turn recovery 和
`GRAPH-G01`。只有 graph snapshot persistence，不能证明 indeterminate model/tool turn
可以安全 replay。

Repository-wide release 要求：

- 所有必要 Tier A、Tier B 测试通过
- strict Clippy 无 warning
- environment-gated failure 必须显式报告，不能当作 skip 后宣称通过
- production consumer 使用的每个 `internal-proof` 都通过 public facade 暴露，并至少有
  一个 consumer-shaped executable test
