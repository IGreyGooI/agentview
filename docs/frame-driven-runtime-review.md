# Frame-driven Runtime Review

日期：2026-08-28；最后更新：2026-09-01

状态：**FDR-001至FDR-020均已解决，FDR-021的v1风险已fail closed且future provenance extension保持deferred；
FDR-022至FDR-052均已解决。Phase 0至Phase 9 gates及Phase 9 Tasks 1至Task 7均已独立签收。
FDR-051 post-closure reopen由final CLI-only provisional-response/commit correction关闭；FDR-052 package boundary保持通过。
当前review无open release blocker。**

## 1. Review 范围

本 review 针对当前 dirty worktree，按以下顺序检查：

1. `docs/engine.md`，作为当前权威目标 contract；
2. `docs/frame-driven-runtime-plan.md`，重点检查 Phase 1 及 phase dependency；
3. `docs/frame-driven-runtime.architecture.html`；
4. `docs/frame-driven-runtime.architecture.json`；
5. 审查期间已经出现的 Phase 1 至 Phase 9 Rust 实现及测试。

本文是 reviewer 与实现方之间唯一的固定交接面：reviewer在第 3 节记录finding并拥有finding status与phase sign-off；
主实现方在第 8 节记录逐项回复、源码锚点和验证证据。

## 2. 总体结论

当前协议与实现方向已经一致：

- `Application<P>` 固定持有 Component runtime、private `FrameSession` 和一个不可替换的 `ReactionPort`；
- canonical history 与 target delivery state 已分离；
- `Frame` 是 move-only exact submission；
- borrowed `ProviderFactStream` 的 lifetime 可以由 port 持有；
- handoff crossing poll 后的 FrameSession commit 是同步、不可失败且没有 `.await`；
- dirty Component 保留上一版 committed complete projection；
- normal reaction 增加 post-reconcile；
- Phase 3 admission 已覆盖 text lifecycle、terminal grammar、ToolCall ordering 和 ToolOutput staging；
- Phase 4 已闭合 next-Full budget；shared projection reconciliation会对value-only跨scope collision
  typed fail closed；
- Phase 5 已接通 private `Application::react()` pipeline，并发推进 fact stream 与已启动 ToolCall lane。
- Phase 6 built-in targets均有native `ReactionPort` correctness路径；Phase 7 External、driver demand、Skill和
  Plugin integrations共用同一个private `Application + FrameSession` compiler。
- Phase 8 Component demand hook、mount-scoped task primitives、bounded/runtime-fenced supervisor、直接panic
  unwind和production consuming shutdown均已实现并通过独立review；user code不再由Runtime catch/分类/吞掉；
  FDR-035至FDR-041及完整authoring/lifecycle surface均已关闭。

原 review、Phase 5 独立 review和第二轮FDR-017、FDR-018、FDR-020 blockers已按第8节逐项解决。
FDR-019已修复旧authored ledger跨scope的问题；FDR-022进一步禁止把旧scope provider canonical value当成
新scope provenance，并覆盖scope切换前尚未reconcile的canonical tail。FDR-021没有被错误地标成future
replacement support：v1 `CompleteTranscript`之外的non-append replay会在reconciliation前typed fail
closed；真正的replacement支持等到HistoryPolicy携带occurrence provenance后再实现。FDR-007的
architecture语义主链、showcase validation、Archify 2.12 delivery和像素检查均已闭合。FDR-014
已冻结为shared Frame compiler管理的replaceable System snapshot。Responses现在原生实现
`ReactionPort`，FDR-012、FDR-013、FDR-015和FDR-016均有production路径与conformance；第二轮native
review新增的FDR-023至FDR-027也已解决。Chat与Debug随后完成原生迁移，FDR-028至FDR-032均有direct
regression；Phase 6 built-in target gate可以关闭。External migration和FDR-033 disconnect fence已通过
独立复核；Agent、Skill、Plugin canonical golden及各自lifecycle gate均通过，Phase 7也可以关闭。
Phase 8的production teardown、有界retirement bookkeeping、runtime-affinity、task-panic arbiter、shutdown
migration和External panic precedence均已独立签收；完整authoring/lifecycle surface与全量门禁未发现新增blocker。
因此Phase 8 gate关闭，可以进入Phase 9。Phase 9 Tasks 1至Task 7现已逐slice独立签收，FDR-042至
FDR-052全部resolved。FDR-051在旧candidate的final gate上reopen后，最终CLI-only correction把已严格验证但尚未公开的
provisional Full传输/验证移出ack critical path，并只在exact candidate、operation precedence及old-owner consuming
shutdown全部成功后提交小型ticketed Commit；任何mismatch/error/loss均fail closed或走Replace且不输出provisional bytes。
final-source双配置full、CLI、strict gates及重复deadline证据稳定通过。因此Task 7和Phase 9总gate关闭。

## 3. Findings

以下保留 reviewer 提出 finding 时的原始证据、影响与要求；当前 resolution 以第 7、8 节为准。

### FDR-001 High: Frame handoff precondition 没有绑定 FrameProfile

证据：

- `src/component/execution/reaction.rs` 的 `Frame` 只携带 `revision`、`target`、`epoch`、`prepared_against`、`basis` 和 `submission`；
- `docs/engine.md` 要求 `TargetIdentity` 与 `FrameProfile` 在 mount 内稳定；
- `docs/frame-driven-runtime-plan.md` 要求 profile 改变时 fail closed，但 `SubmitFault` 没有 profile-specific variant。

影响：

`declare()` 后、crossing poll 前，如果 target 只改变 limits 或 capabilities，Frame 的 identity、epoch 和 continuity 仍可能全部匹配。除非每个 port 额外保存一份未写入 trait contract 的 declaration profile，否则 port 无法从 Frame 得知它是按哪个 profile prepare 的。这使 profile-only race fence 依赖隐含实现约定，而不是 public protocol。

要求：

- 在 declaration 和 Frame precondition 中加入 mount-stable profile revision/fingerprint，或直接携带可比较的 profile snapshot；
- crossing poll 前同时比较 identity、continuity 和 profile precondition；
- 增加 typed `ProfileChanged` 或等价 fault；
- 增加 profile-only declare/submit race conformance test。

### FDR-002 High: Fault contract 仍是自由文本，并可能泄漏模型内容

证据：

- `src/component/execution/reaction.rs::ReactionPortFault` 只有 `message: String`；
- `docs/engine.md` 要求 port、handler、lane 和 terminal fault 带明确 stage/reason；
- `src/component/execution/admission.rs::ReactionAdmissionFault::TextSealMismatch` 保存并格式化完整 `accumulated` 与 `sealed` assistant text；
- 现有 legacy `ProviderFault` 已经提供 `ProviderFaultKind + ProviderFaultCode` 的 payload-free precedent。

影响：

- 下游只能解析错误字符串，后续增加 machine-readable code 会再次破坏 experimental public API；
- provider/model 输出可能进入 `Display`、`Debug`、日志或 observer；
- 大文本 mismatch 还会扩大 fault 对象和错误格式化成本。

要求：

- 为 `ReactionPortFault` 和 admission/application faults 定义 sanitized kind/code/reason；
- stage 可以由 runtime call site 补充，但 reason 必须 machine-readable；
- fault detail 只保留长度、closed enum 或 digest 等无 payload 信息；
- 增加 sentinel 测试，证明 authored/provider content 不进入 error、Debug 或 observation。

### FDR-003 Medium: TargetDeclaration::resume 可以构造 scope 矛盾的 snapshot

证据：

- `TargetDeclaration::resume(identity, epoch, revision, profile)` 接受三个彼此独立的 scope 字段；
- constructor 不验证 `revision` 内嵌的 target/epoch；
- `Application::mount()` 当前只验证 profile，随后就执行 bootstrap root；
- 真正的 scope 校验直到 crate-private `Frame::from_compiled()` 才发生。

影响：

一个错误 port 可以让 structurally invalid declaration 通过 mount，并建立部分 Component state，之后才以 internal Frame invariant fault 失败。这与 mount 文档中“invalid target contract 不能建立 partial Component state”的承诺不一致。

要求：

- 首选 `TargetDeclaration::resume(revision, profile)`，在 constructor 内从 opaque revision 派生 identity/epoch；
- 或让 `resume()` fallible，并在 mount reconcile 前验证完整 declaration；
- 增加 wrong-target、wrong-epoch、wrong-namespace declaration tests，验证 root 未执行。

### FDR-004 Medium: Phase 3 streaming admission 是二次复杂度

证据：

`ReactionAdmissionGuard::admit()` 对每个 fact 都会：

- clone 整个 `AdmissionState`，包括累计 text；
- clone base transcript items 并重新 materialize 所有本轮 outputs；
- 对完整 candidate 再运行 `CanonicalTranscript::try_from_items()`；
- clone `abort_candidate` 后写入 transcript。

影响：

大量小 `TextDelta` 会产生接近 `O(delta_count * history_size + output_size^2)` 的 CPU 和内存复制。在较长 canonical history 或高频 SSE delta 下，这会在 wire/output hard limit 之前形成可观的资源放大。

要求：

- 让 authoritative transcript 自身始终保持 cancellation-safe candidate，避免同时保留等价 clone；
- 增量维护当前 output tail，而不是每个 delta 重建完整 base history；
- 在 Phase 5 接入 production pump 前增加 construction-work 或大流量测试，验证总工作量近似线性。

### FDR-005 Medium: ToolCatalog 没有实现冻结的 canonical grammar

证据：

- `ToolCatalog::new()` 仅调用 Rust `String::sort_unstable()`；
- constructor 不拒绝空 name 或重复 name；
- `engine.md` 和 plan 规定非空、重复非法，并按 JCS string comparator 排序；
- Rust String ordering 与 JCS/UTF-16 ordering 在 supplementary Unicode 上不同，例如 `U+10000` 与 `U+E000`。

影响：

不同实现可能对同一个 ToolCatalog 产生不同的 canonical bytes，破坏 `max_component_bytes`、`max_frame_bytes` 和 golden vector 的确定性。重复工具名也会让 target capability lookup 产生歧义。

要求：

- 使用明确、测试固定的 comparator；
- constructor 返回 `Result` 并验证 empty/duplicate；
- 增加 ASCII、Unicode supplementary、duplicate 和 empty golden vectors。

### FDR-006 Medium: required private state 的 proof 被放在无法完成证明的 declare 边界

证据：

- `ReactionPort::declare()` 明确不能读取 canonical history；
- `engine.md` 又要求 port 在声明 `FullRequired` 前证明“canonical Full + required private state”可以形成合法请求。

影响：

如果“证明”指 exact candidate legality，则 port 在 `declare()` 时没有必要输入，contract 不可实现；如果只指 recovery capability，当前文字没有区分 capability proof 与 actual Frame proof。

要求：

- `declare()` 只证明存在 content-independent、provider-defined recovery strategy；
- `submit(frame)` 在 handoff 前验证 actual Full、required private artifacts、wire bytes 和 token limit；
- 无 recovery strategy 时 declaration 直接 fail closed；
- actual candidate 不合法时返回 typed pre-handoff fault。

另有一个 budget 用词需要统一：`engine.md` 前文明确 `max_frame_bytes` 只计量 canonical `FrameSubmission` payload，并排除 control metadata；invariant 11 的 “total canonical Frame” 应改为 “total canonical FrameSubmission payload”。

### FDR-007 Medium: 架构图没有表达核心 commit/admission 路径，且交付产物未过 showcase gate

语义问题：

- JSON 将 `Ready Ok(stream)` 从 `ReactionPort` 直接连到 `ProviderFactStream`，没有表示 stream 返回 Application；
- 图中缺少 crossing poll 后 `Application -> FrameSession` 的 synchronous outbound commit；
- `ProviderFactStream -> Application` 标记为 `history first`，但没有画出 `FactStream -> FrameSession validate/commit -> Component Runtime dispatch`；
- 这会弱化图中最重要的 ownership 和因果顺序。

产物问题：

- Archify 2.12 standard validation：0 errors，1 warning；
- warning 为 `crossing poll` label 与 `declaration before reconcile` route 0px clearance；
- showcase validation 因该问题失败；
- checked-in HTML 的 generator 是 Archify 2.10.0，缺少 2.12 的 semantic navigation 和 route metadata；
- legacy HTML checker 因缺少这些 metadata 给出零 route metrics，不能证明 JSON 与 HTML 当前一致。

要求：

修正 JSON 的 flow 和 route 后，使用当前 Archify `deliver` 重新生成 HTML，并要求 showcase validation 通过。

### FDR-008 Low: Phase 1 test closure 与计划状态仍不完整

证据：

- handoff harness 分别测试了“第一次 poll Pending 后 drop”和“第一次 poll 立即 Ready(Ok)”；
- 尚未测试同一个 future 的 `Pending -> Ready(Ok)` crossing transition；
- 尚未覆盖多次 Pending、Pending 后 Ready(Err)、重复完全相同 declaration 不使 prepared Frame 失效；
- `docs/frame-driven-runtime-plan.md` 首页仍写着“Rust implementation 尚未开始”，但工作树已经进入 Phase 3。

要求：

- 增加逐 poll transition matrix；
- 对每一个 Pending cancellation point 建立独立 case；
- 增加 idempotent declaration 和 profile-only change cases；
- 更新 plan status，明确当前已完成和未完成的 phase gate。

### FDR-009 High: Full 的 raw-complete projection 与 CompleteTranscript replay 会重复提交 provider output

证据：

- `engine.md` 与 plan 当前要求 Full 的 Component section 携带 raw complete projection；
- v1 Full replay 同时固定为 CompleteTranscript；
- provider output 已先进入 canonical transcript，Component 下一轮又可能把同一 item 渲染进 complete projection；
- 当前 `FrameSession::prepare()` 会把两份都放入 Full，并再次把 projection item append 到 canonical history；
- legacy Responses / Chat 即使 force-full，也会先用 unclaimed provider-output ledger claim 相同 occurrence。

影响：

同一 assistant output 会在一次 Full 中同时出现在 replay 与 Component section，既改变模型输入语义，
又使 canonical history永久重复。要求 port 二次去重会重新把 shared reconciliation 泄漏回 integration boundary。

Resolution：

- `Full` 表示不依赖旧 delivery checkpoint、单独即可解释的 self-contained submission；
  不要求它的 Component section逐项等于 raw complete projection；
- private checkpoint和`max_component_bytes` meter仍使用 raw complete projection；
- Frame compiler用 CompleteTranscript replay occurrence、unclaimed provider outputs和当前 complete projection
  做 shared reconciliation；已由 replay 表示的 occurrence 从 Component section omit，未表示的 authored item保留；
- replay + staged inputs + reconciled Component section的组合必须保持完整且有序；port不得再次 dedup；
- 增加 Full continuity reset、provider output被Component吸收、重复 occurrence和`#[diff]` coexistence tests，
  并同步修正`engine.md`与plan中“Full放完整projection items”的表述。

### FDR-010 High: post-handoff `react()` cancellation 会遗留无 owner 的 pending ToolCall lane

> **Historical finding:** 以下证据与要求记录引入cancellation fallback之前的缺陷和当时可接受的
> terminal/resumable二选一。terminal mitigation后来曾落地，但现已由admission-time fallback reserve和
> reusable `react()` cancellation取代；当前结论见本页FDR-010 response与implementation ledger。

证据：

- ToolCall admission 会在 session-owned staging 中创建 `output: None` slot，但 lane future 仅由
  reaction-local `FuturesUnordered` 持有；
- drop `react()` 会 drop pending lane，RAII admission guard 会保留已接纳 ToolCall，却不能为被取消的
  lane 生成 ToolOutput；
- 下一次 `FrameSession::prepare()` 会因 `UnresolvedOutput` fail closed，但 `Application` 没有显式
  terminal/poisoned state；
- 现有 cancellation coverage 只证明 lane 已完成并 staged 后的 drop，不覆盖 pending lane 被取消。

影响：

调用方无法从 API contract 判断 post-handoff cancellation 后 `Application` 是否仍可复用。继续调用
`react()` 虽会被 pending call 间接挡住，但 terminality 依赖 ToolOutput staging 的偶然状态，而不是
明确生命周期状态；没有 ToolCall 的 post-handoff cancellation 又可能走另一条路径。

要求：

- 明确并强制 post-handoff `react()` future drop 的 terminal contract，或把 started lanes 提升为
  Application-owned resumable state；
- 当前 v1 若选择 terminal contract，应在 cancellation guard 中 poison Application，并保证之后的
  `react()` 在 declare/render/submit 前返回 typed terminal fault；
- 增加 pending lane 被取消后第二次 `react()` 零 submit 的测试，并同步 `engine.md` 与 plan。

### FDR-011 Medium: nested ToolOutput fault 的结构化分类被 admission wrapper 丢失

证据（独立 review snapshot）：

`ReactionAdmissionFault::ToolOutput(source)` 曾先被压成通用 `ReactionAdmissionReason::ToolOutput`，再统一
映射为 `Protocol`；这会丢掉 `RegistrationIdentityExhausted` 等已经存在的精确 code/reason，并使
Admission、FramePrepare 和 lane staging 对同一底层 fault 的分类不一致。

要求：

`ApplicationFault::from_admission()` 应先解包 nested ToolOutput fault，并复用
`from_tool_output(ApplicationFaultStage::Admission, source)`；增加精确 classification test。

### Phase 6 preflight: Responses migration findings (historical)

以下 finding 来自对现有 Responses implementation 的迁移审查。它们不重新打开已经完成的 Phase 0-4
shared kernel gate，但在 Responses `ReactionPort` 被称为 correctness-complete 前必须解决。

#### FDR-012 High: private compaction 缺少 canonical coverage proof

当前 Responses continuation 只保留被裁剪后的 wire input，不能证明 private compaction artifact 覆盖了
Full replay 的哪个 canonical prefix。直接附加完整 replay 会重复上下文，直接跳过 replay 又违反 exact
Frame。Phase 6 必须保存并校验 coverage count + prefix digest（或等价 proof），digest 不匹配时在
handoff 前 fail closed；不能用完整 `submitted_items` 镜像重新把 shared history 放回 provider。

#### FDR-013 High: ProviderFact 缺少 output-order release barrier

当前 Responses public text/tool events 基本按 wire arrival / `item.done` 直接返回，而 private continuation
只对 retained wire items 按 output index 重排。新协议要求 first fact 的 yield 顺序就是 canonical output
顺序。Phase 6 需要 reaction-local ready queue：只有所有更低 output index 已被证明为 private，或已经形成
可发布 public fact 后，才能释放后续 fact。

#### FDR-014 High: System instruction replacement 语义未冻结

CompleteTranscript 可以积累多个 System instruction，当前 Codex lowering 却拒绝多个；legacy Responses
实际依赖“当前 projection instruction 替换旧 sidecar”的行为。迁移前必须明确 canonical
latest-system-wins，或引入显式 replacement fact。不能由 port 根据隐藏 history 猜测。

#### FDR-015 Medium: production FrameProfile 没有配置来源

当前 Responses transport config 只有 serialized request limit，没有稳定的 `max_frame_bytes`、
`max_component_bytes` 和 token hints。Phase 6 必须增加 mount-stable production config 或明确安全默认值；
不得用 `usize::MAX` 绕过 budget contract。Responses 是否声明 semantic delta capability也必须显式固定。

#### FDR-016 Medium: legacy OpenAI faults 需要唯一 sanitized mapping

legacy `ProviderFault` 包含 payload-bearing diagnostics，新 `ReactionPortFault` 只允许 closed
kind/code/reason。Phase 6 应只有一个 `map_openai_fault()`（或等价边界）：详细诊断留在 provider-private
observability，公开 fault 不在各 SSE branch 临时拼装，也不泄漏 wire/model payload。

### FDR-017 High: authoritative Phase 0 contract 与当前实现仍有互相冲突的规则

证据：

- `engine.md` 的 public `Frame` shape仍未列出实现已经依赖的 `prepared_profile`，plan也没有冻结
  `ProfileChanged` 和 structured fault algebra；
- `engine.md:195` 与 plan `:344` 仍要求 Full 从 Accepted prepare时保持 accepted revision的
  FrameSession namespace和sequence递增；`reaction.rs:374-391` 只对 `DeltaFrom`执行该检查，并由
  `compiled_full_can_rebase_a_foreign_accepted_namespace` 明确允许 foreign Accepted revision；
- plan `:536` 又规定 unknown namespace回退Full，因此同一份plan同时要求“回退Full”和“Full不得跨
  namespace”；
- `engine.md:142` 已把 `FullRequired` 正确定义为 content-independent capability proof，但
  `engine.md:468` 仍保留“declare前证明canonical Full合法”的旧表述。

影响：

`engine.md` 仍是权威 contract。实现、candidate API和恢复语义在上述位置给出不同答案时，Phase 0不能以
源码测试通过为理由关闭；built-in port迁移也无法确定应实现哪一个 Full/recovery precondition。

要求：

- 明确选择 Full revision语义。若接受当前实现，应规定 namespace/sequence约束只适用于
  `DeltaFrom(base)`；Full仍携带exact Accepted precondition，但successful handoff可安装新的本地
  FrameSession revision并让port rebase；
- 将 `prepared_profile`、`ProfileChanged`、structured port/application fault shape原子回写
  `engine.md`与plan candidate API；
- 删除 required-private-state 的旧冲突段落，而不是同时保留新旧两种proof定义；
- 文档更新前重新打开 Phase 0 gate，不能把 FDR-001、FDR-002、FDR-006标成最终resolved。

### FDR-018 High: 较高 TargetEpoch 只在 Frame handoff commit 后记住，pre-handoff retry会遗忘已观察 epoch

证据：

- `FrameSession::validate_declaration()` 只读比较 `target_delivery.highest_epoch`；
- 较高 epoch 只写进 `FrameCommitCandidate`，successful handoff后才安装；
- `Application::refresh_declaration()` 虽把较高 declaration放进 `self.declaration`，下一次调用却不和该
  已观察 snapshot比较；
- `SubmitFault::Rejected` 可以是 retryable pre-handoff fault，此时 FrameSession state不commit。

影响：

序列 `epoch 1 Accepted -> epoch 2 FullRequired -> retryable pre-handoff reject -> epoch 1 Accepted` 会在第二次
`react()` 重新通过验证。Runtime因此接受已经观察过的epoch回退/复用，并可能对旧head生成Delta，违反
“同一TargetIdentity内epoch单调且不得复用”的核心continuity fence。

要求：

- 单独保存 `highest_observed_epoch`，在成功读取并验证 declaration 时同步、不可回滚地推进；它不是
  Frame handoff candidate，也不推进canonical history/revision；
- 后续 declaration必须同时对照 highest observed和highest committed epoch；
- 增加“higher epoch + retryable pre-handoff fault + lower epoch”测试，证明第二次调用在render/submit前
  terminal fail closed。

### FDR-019 High: shared projection reconciliation 没有 execution-scope fence，remount 的新 occurrence会被旧 ledger吞掉

证据：

- `FrameSession::prepare()` 只用 execution scope筛选 `delta_checkpoint`，但仍把未筛选的
  `reconciliation_checkpoint` 传给 Full reconciliation；
- `ProjectionReconciliationState` 的 node key只有 `node.identity: String`，不包含
  `ProjectionExecutionScope`或其他mount incarnation；
- `ComponentHost::remount()` 只推进 `mount_generation`，root/node结构identity可以保持相同；
- legacy conformance `missing_or_remounted_diff_memo_resends_full_projection_without_losing_history` 明确要求
  scope变化后重发完整projection occurrence。

影响：

同一host从mount generation 1切到2、并渲染相同node/item时，basis会正确回退Full，但ordinary item可能
被旧node ledger claim并从Component section省略。新mount identity没有进入canonical history，旧mount
事实被错误当成新mount occurrence，违反mount fence并造成built-in迁移行为回归。

要求：

- 将 authored projection ledger绑定到 execution scope，scope变化时重置node occurrence ledger和diff
  baseline；
- provider-output unclaimed ledger若需要跨scope保留，应与scope-bound authored ledger分开处理；
- 增加相同item跨mount-generation测试，要求Full submission仍包含新mount occurrence，同时不得重复已经由
  replay表示且被当前mount显式claim的provider output。

### FDR-020 Medium: ReactionAdmissionGuard::Drop 在内部materialization失败时静默丢弃已发布tail

证据：

- event返回后，本轮canonical tail只存在 `AdmissionState`，stable transcript直到normal finish或Drop才
  flatten；
- `Drop` 调用仍会分配、重建并执行 `CanonicalTranscript::try_from_items()`；
- `materialize(Abort)` 返回Err时，`if let Ok(...)` 直接保留旧stable prefix，没有panic、terminal marker或
  其他fail-stop处理。

影响：

只要新增canonical invariant、未来item variant或内部bug使Drop materialization失败，已经对Component可见的
facts就会从canonical history消失。该fallback正好违反“visible fact先commit且后续fault/cancel不回滚”的
最高优先级不变量；静默保留prefix比显式fail-stop更危险。

要求：

- 让abort flatten在类型/结构上真正不可失败，例如保存已经canonical-validated的tail item并提供private
  infallible append/install操作；
- 在完成该结构前，内部不变量失败至少必须 fail-stop，不能静默继续一个缺失causal facts的session；
- 增加test-only invariant injection，证明Drop不会无声丢弃已release event对应的tail。

### FDR-021 Medium: FDR-009 只覆盖append-compatible checkpoint，replay-view replacement仍会回到重复提交

证据：

- `newly_unclaimed_outputs` 只在 `replay_view.starts_with(checkpoint.replay_basis)` 时从canonical tail建立；
- replay replacement导致 `reconciliation_checkpoint == None`，此时Full reconciliation既没有旧ledger，也
  没有从新完整replay建立claim index；
- 当前 replacement test只断言basis为Full，没有构造“replacement replay与current projection含相同
  occurrence”的重复检测。

影响：

v1 `CompleteTranscript` production policy暂不替换view，但plan已经把未来replacement fallback语义作为
Frame compiler规则冻结。当前实现只能证明continuity reset路径修复了FDR-009，不能把finding整体标为
resolved。

要求：

- 要么将FDR-009 resolution明确限定为v1 append-only policy，并把replacement支持保持deferred；
- 要么为replacement replay建立可验证的occurrence/provenance basis，并增加相同occurrence测试，证明Full的
  replay与Component section不重复且不误吞新的authored occurrence。

### FDR-022 High: scope reset把canonical value当成provider provenance，会吞掉同值的新 authored occurrence

证据：

- `ProjectionReconciliationState::reset_authored_for_scope()` 清空node ledger后，把累计
  `provider_outputs` 全部复制回 `unclaimed_provider_outputs`；
- `ItemOccurrenceIndex` 的唯一key是 `submission_item_key(item)`，即canonical item value；ledger没有
  provider output key、origin token或projection occurrence provenance；
- `RenderedProjectionNode` 也只携带 `Vec<CanonicalInputItem>`，无法标记当前item是旧provider output的
  projection，还是新mount独立 authored item；
- FDR-019 regression使用不同值的 `"authored"` 与 `"provider"`，没有覆盖二者canonical value相同的
  collision。

可复现序列：

1. mount generation 1先提交 authored `"base"`；
2. canonical history随后接纳provider output `"collision"`，下一Frame保持projection为 `"base"`，使该
   provider occurrence进入累计ledger；
3. mount generation 2的complete projection只包含一个独立 authored `"collision"`；
4. scope reset把旧provider occurrence重新开放，value-only claim将新authored item省略。实际Full的
   Component section为 `[]`，而正确结果应包含 authored `"collision"`。

reviewer在隔离的 `/tmp/agentview-fdr019-review` 副本加入
`execution_scope_reset_preserves_equal_valued_new_authored_occurrence`；该test稳定失败：

```text
left:  []
right: [AssistantText { text: "collision", phase: None, status: Sealed }]
```

影响：

这不是future replay replacement问题；v1 `CompleteTranscript`加正常remount即可触发。新的authored
occurrence不会进入Frame或canonical history，直接违反mount fence和FDR-009“未被replay表示的authored
item必须保留”的resolution。因此FDR-019只能证明旧node-authored ledger已重置，不能证明跨scope
provider reconciliation正确，Phase 4 v1 gate仍需打开。

要求：

- 跨scope保留provider claim前必须有能关联当前projection occurrence的明确origin/provenance；仅存
  canonical value不构成proof；
- 若v1暂不增加provenance，遇到可能碰撞时必须选择并冻结保守行为，例如typed fail closed；不能静默吞
  authored occurrence；
- 把上述同值序列加入repository regression，并同时覆盖重复同值occurrence的计数；
- FDR-022关闭前，ledger保持open，FDR-019 resolution只描述已经完成的authored-ledger scope reset。

### FDR-023 High: terminal Responses fault 不能降级成新 epoch Full retry

Native stream原实现对所有post-handoff fault和Drop统一调用`lose_continuity()`。因此401、确定的protocol
violation、output/body hard limit等terminal fault也只推进epoch，并在下一次`declare()`重新给出
`FullRequired`。这违反`ReactionPortFaultKind::Terminal`“当前logical target session不能继续”的定义。
要求target持久保存terminal fault；只有retryable fault或无terminal proof的cancellation才能推进epoch。

### FDR-024 High: Responses completion不能强制存在primary text

Native adapter曾在过滤function call后复用legacy `OpenAiOutputLedger::complete()`；该API强制恰好一个
non-commentary final text。reasoning+tool、commentary-only等合法reaction因此被误报
`MissingFinalMessage`。要求terminal validator独立完成lifecycle/identity校验，并允许
`ReactionCompleted { primary_text: None }`。

### FDR-025 High: legacy ProviderPort 与 Frame-native ReactionPort 缺少实例级mode fence

同一个`AsyncOpenAiResponsesProvider`同时保存legacy continuation/tool staging和native reaction Frame state，
且同时实现两个port trait。若交替调用，两套owner会从同一HTTP target分叉。要求实例在真实handoff poll
才claim一种mode：确定的pre-handoff local failure不claim；handoff后另一种mode必须typed fail closed。

### FDR-026 High: opaque compaction coverage没有绑定System snapshot

canonical prefix proof故意排除replaceable System，但Full recovery曾无条件复用旧compaction wire state并
替换`instructions`。除非provider保证compaction不覆盖instructions，否则System change/clear可能把旧语义
藏在opaque artifact中。要求compaction seal绑定normalized System digest；Full reuse必须exact match，
无compaction的普通System replace/clear继续允许。

### FDR-027 Medium: Accepted declaration没有验证required native state仍存在

target可声明`Accepted(revision)`，即使`reaction_frame`缺失或revision不匹配；错误会延迟到submit prepare。
这违反coherent declaration要求。`declare()`和crossing poll都必须验证Accepted revision对应exact native
state；已知丢失时返回sticky terminal Declaration fault。

### FDR-028 High: Chat response不能同时进入wire baseline和下一Frame replay

Chat native草案曾在`[DONE]`时把assistant output追加到provider-private `wire_messages`；下一次
`FrameSession::prepare()`又会把同一sealed output放进Delta replay，导致request重复。Chat upstream没有
server-side continuation，accepted wire baseline只表示上一份request input；本轮output必须由下一Frame
replay首次加入。要求删除stream-time baseline mutation，并用连续两轮exact request body回归证明只出现一次。

### FDR-029 High: Chat empty completion不能伪造空text lifecycle

Chat native草案对只有stop/`[DONE]`、没有非空content的合法empty completion发布
`TextSealed { text: "" }`和`primary_text: Some(0)`，违反shared terminal grammar。要求直接发布
`ReactionCompleted { primary_text: None }`，不创建output key。

### FDR-030 High: Debug legacy/native入口缺少first-handoff mode fence

同一`DebugProviderPort`可以先`ProviderPort::execute()`再`ReactionPort::submit()`，或反向交替，两套capture
owner会分叉。要求加入`Unclaimed | Legacy | FrameNative`，只在真实capture handoff同步claim；unpolled或
precondition failure保持Unclaimed，另一入口随后typed fail closed。

### FDR-031 High: Debug TargetIdentity exhaustion会panic且与其他adapter共享低位namespace

Debug原allocator在`AtomicU64`耗尽时panic，且identity从1开始，与Responses低位domain碰撞。要求耗尽时
保存payload-free terminal Declaration fault，不panic；Debug使用独立u128 domain并覆盖instance uniqueness、
domain separation和local exhaustion regression。

### FDR-032 Medium: Debug FrameProfile没有operational budget

Debug原profile使用`usize::MAX`和`usize::MAX / 2`，shared Frame meter形式通过但不能形成实际内存边界。
要求改为有限、mount-stable默认值，并验证declaration幂等和Accepted后profile不变。

### FDR-033 High: External ingress断开不保证成为stream fault

证据：

- `ExternalControl::act()`在读取protocol前通过`claim_ingress()`把唯一fact sender从shared ingress中取走；
  如果act producer随后被取消，sender直接Drop（`external.rs:251`, `external.rs:352`）。
- `external_fact_stream()`把没有terminal的channel EOF直接返回为normal stream EOF；它只推进continuity，
  不yield `ReactionPortFault`（`external.rs:573`, `external.rs:584`）。shared admission随后把它分类为missing
  terminal protocol fault，而不是计划要求的retryable stream fault。
- `submit()`先异步取得queue permit，之后才安装shared ingress（`external.rs:469`, `external.rs:476`）。若
  last `ExternalControlInner`恰好在这两个动作之间Drop，Drop观察不到active sender（`external.rs:210`）；
  submit仍会安装sender、send并返回`Ok(stream)`，但已经没有control能够注入act或关闭该sender，stream可
  永久Pending。

影响：

这两个路径都已经跨过Frame handoff，却不能满足`engine.md`和plan 5.3规定的“caller断开、读取失败或act
timeout成为stream fault”。前者产生错误的terminal admission classification，后者可能让一次structured
reaction永久挂起。现有`receiver_failure_after_queue_acceptance_is_a_stream_fault`只覆盖submit返回后再Drop
control，没有覆盖claimed act cancellation或reserve/install竞态。

要求：

- 在shared state中记录control receiver liveness，并与ingress安装使用同一把锁线性化；reserve后若control
  已关闭必须确定pre-handoff reject，安装后关闭则必须使返回stream可终止；
- nonterminal fact-channel EOF必须yield一次retryable `StreamTransport` fault，不能作为normal EOF交给
  admission；即使fault queue已满、Drop中的`try_send`失败也必须成立；
- 增加claimed act future cancellation回归，以及test barrier固定`reserve acquired -> last control Drop ->
  ingress install/send`竞态；两者都必须证明不hang、continuity推进且下一次显式reaction为Full。

Resolution：已解决。control liveness与ingress安装现在由同一shared mutex线性化（`external.rs:176`,
`external.rs:211`）；last control Drop在尚无ingress时直接推进continuity，submit在reserve后、安装ingress前
检查liveness并作pre-handoff reject（`external.rs:521`）。nonterminal fact-channel EOF现在固定yield一次
retryable `StreamTransport`（`external.rs:634`）。claimed-act cancellation与reserve/control-Drop barrier
回归分别位于`external/tests.rs:224`和`external/tests.rs:259`。

### FDR-034 High: last External control永久消失后仍声明可恢复的 FullRequired

证据：

- 最后一个`ExternalControlInner` Drop会销毁唯一frame receiver并把`control_alive`设为false；同一个port没有
  API可以重新创建control或receiver（`external.rs:211-230`）；
- 无active ingress时，Drop只调用`reset_continuity()`推进epoch并清空accepted revision，没有安装terminal
  target fault；
- `ExternalProviderPort::declare()`只调用`target.declaration()`，完全不检查`control_alive`
  （`external.rs:494-501`）；
- `submit()`直到异步`reserve_owned()`失败或reserve后的liveness检查才返回retryable Transport fault
  （`external.rs:503-525`）；该port实例上任何重试都不可能成功；
- 现有`last_control_drop_after_reserve_rejects_before_handoff`反而把随后仍能`declare().unwrap()`并得到新epoch
  当作恢复证据，没有检查这个declaration是否有真实handoff strategy（`external/tests.rs:259`）。

reviewer在隔离的`/tmp/fdr034-repro`外部crate执行：创建port/control、drop唯一control、随后调用
`ReactionPort::declare()`。当前输出为`unexpected recoverable declaration: epoch=2`，证明该target对外宣称
`FullRequired`，但其queue receiver已经永久关闭。

影响：

这违反`engine.md:258-263`和plan `:136-141`：`FullRequired`必须证明content-independent recovery strategy
仍存在；否则要在declaration阶段fail closed。当前Application会完成bootstrap/render和Full prepare，直到
submit才得到一个永远不可恢复的retryable fault；Skill与Plugin薄wrapper继承同一问题。FDR-033解决了
reserve/install竞态和post-handoff EOF，却没有解决永久transport owner丢失后的target terminality。

要求：

- last control Drop必须安装sticky terminal target fault，或让`declare()`在`control_alive == false`时返回
  payload-free terminal Declaration/Unavailable；不能只推进epoch；
- 已prepare Frame在control Drop后也必须得到确定pre-handoff terminal rejection，而不是retryable Transport；
- 增加“mount前drop control”“accepted reaction结束后drop control”“reserve后drop control”三条回归，证明
  declaration/submit分类一致、零handoff且不会继续render；
- FDR-034关闭前Phase 7 gate保持打开；FDR-033的stream/race resolution可以继续保持resolved。

### Phase 8 reviewer policy note: Component task panic直接stack unwind

owner澄清这里不是Cargo `panic = abort`或process abort，而是`panic=unwind`语义：Component task panic不转成
`ApplicationFault`、不恢复当前operation，并在runtime观察到后从当前可用的outer driver boundary直接
`resume_unwind`。由于`use_future`/`spawn`运行在独立Tokio task，Rust不能跨task直接展开另一条调用栈；如果
此时没有`react()`、driver demand wait或integration driver正在poll，只能保留原panic到下一个此类边界。
caller若主动`catch_unwind`，Application不承诺可复用。

正常Rust Drop会随unwind执行，但额外的sibling drain不能成为开始unwind的前置条件；否则一个阻塞或失效的
sibling cleanup会把“直接stack unwind”退化为永久hang。typed handler error、normal cancellation、unmount和
显式shutdown仍必须完整cleanup。`engine.md`、plan和Phase 8 checkpoint中“drain siblings后才恢复unwind”的
旧表述需要按该owner决定同步；无需增加Cargo abort profile。

并发实现快照随后误加了`[profile.release] panic = "abort"`（`Cargo.toml:47-50`），并把engine/plan改成
process-fatal。该改动与owner本次澄清直接相反，必须撤回；release build也应保持stack unwinding。这里的
“当前/下一driver boundary”是异步Tokio task向owner stack传播panic的必要桥接，不是process termination。

reviewer独立执行`cargo test --all-features --quiet`时，library得到450/451：
`latched_task_panic_overrides_driver_wait_after_bootstrap_task_start`失败，`starts`为0而测试断言1。该测试假设
bootstrap task一定先于后来注入的panicking task被poll，但runtime只承诺registration不inline poll；单测单独
重复10次通过也不能消除全量并行下的调度flakiness。它并入FDR-038的panic contract测试修订；修正前实现方
checkpoint中的“451/451稳定通过”不能作为Phase 8 gate证据。

### FDR-035 High: Application teardown的abort + await没有接入任何production owner

状态：**resolved；production External owner与CLI acknowledgement已获独立签收。**

证据：

- private `Application::shutdown(self)`确实先fence tree并await supervisor（`application.rs:261-276`），但全仓
  production代码没有调用点；唯一调用均在`application.rs`单元测试；
- `MountTaskSupervisor::Drop`只发送Shutdown、`join.abort()`后立即返回，并明确不能await
  （`task.rs:150-160`）；它是best-effort abort，不是cleanup completion proof；
- public `ExternalApplication`持有实际`Application` owner或reaction `JoinHandle`，但没有`shutdown`或Drop
  lifecycle（`external.rs:894-1211`）。普通Drop还会detach active Tokio reaction task；
- CLI收到`DaemonRequest::Shutdown`后先写`Ok`并退出loop（`src/bin/agentview.rs:785-788`），随后直接Drop
  `DaemonState`，没有取消/取回active reaction、fence mounts或await Component tasks。

影响：

这不满足`engine.md:838`和Phase 8 plan的“Application teardown必须cancel并await”。显式daemon shutdown已经
向caller证明完成时，Component sidecar、future destructor或外部副作用仍可在后台继续；active reaction owner
还可能在state被丢弃后detached运行。SignalRuntime Drop会使capability最终stale，但不能替代task join。当前
`shutdown_fences_mount_capabilities_and_awaits_task_drop`只证明一个没有production caller的private helper。

要求：

- 每个持有`Application`的production integration必须提供并调用一个consuming async shutdown；External路径要
  先取消并join active reaction、恢复owner，再调用`Application::shutdown()`；
- CLI只能在上述cleanup完成后回复Shutdown成功；Agent、Skill和Plugin owner也要有同一lifecycle proof；
- 冻结shutdown future自身的cancellation语义：cleanup必须可恢复或在future Drop后继续被明确owner监督，不能
  因`shutdown(self)`被取消而退化成unawaited Drop；
- 增加task Drop barrier回归，证明shutdown acknowledgement前capability已fence、active reaction已join且全部
  mount task destructor完成。普通Rust Drop可以保留emergency best-effort abort，但不能用于gate证明。

独立closure复核：

- production `Application` owner只有`ExternalApplication`；Debug、Skill和Plugin中的其余mount均为test-only或
  crate-private role，Phase 9形成owner时仍受同一contract约束；
- public shutdown在调用时立即spawn并转移唯一owner，waiter Drop只detach；cleanup先cancel/join active reaction，
  恢复Application后fence tree并abort + join mount tasks；CLI只在该future完成后写response；
- task Drop barrier同时证明Signal capability先stale、active ingress已回收、阻塞task destructor释放前waiter与CLI
  response都不会完成；
- reviewer独立执行shutdown filter 7/7、detached-cleanup回归30/30、CLI ACK barrier 30/30；此前FDR-040的
  runtime-migration + detached-owner liveness签收继续成立。

因此FDR-035接受resolved；普通Drop仍仅为emergency best-effort cleanup。

### FDR-036 Medium: task supervisor永久积累每次remount的retirement tombstone

状态：**resolved；transient retirement claims与current-cardinality收敛已获独立签收。**

证据：

- 每个有actor的`retire()`都向`SupervisorState.retirements`追加一个`Weak<RetirementState>`
  （`task.rs:383-384`）；normal completion只更新`RetirementState`，从不从该Vec删除；只有shutdown/actor failure
  的`close_retirements()`执行一次`retain`（`task.rs:451-459`）；
- actor对每次retire执行`retired_scopes.extend(scopes)`（`task.rs:591,628`），exact
  `MountTaskScope(component,generation)`在cleanup完成后也从不删除；
- core侧`retired_generations`按ComponentId保留high watermark是有界stale fence，但前两份结构分别按
  retirement次数和mount generation增长，重复hide/show同一task-bearing child即可持续扩张。

影响：

Phase 8引入的是long-lived Component sidecar。一个正常运行、反复remount动态child的Application会产生
O(total remounts)的Weak allocation和hash tombstone，即使所有task已经abort/join完成；这不是业务state或
stale-correlation所必需的保留。长期Agent/Plugin进程因此存在确定的runtime-owned内存增长。

要求：

- completed/abandoned retirement必须从core waiter bookkeeping中prune；
- actor stale fence改为per-Component generation high watermark，或在能够证明channel ordering后删除已完成
  exact scope，不能永久保留每个generation；
- 增加高次数same-component remount stress regression和test-only bookkeeping counters，证明live task为零后
  supervisor state由当前Component cardinality约束，而不是由历史remount次数约束。

reviewer closure复核：

- completed weak waiter已经prune，exact generation tombstone也已压成per-Component high watermark；这两项方向正确；
- 但core与actor的`retired_generations: HashMap<ComponentId, u64>`都只insert/update、从不删除
  （`task.rs:433-436`, `task.rs:702`, `task.rs:804`, `task.rs:841-846`）；
- `ComponentId`包含结构位置（`identity.rs:14-22`），render中的scope cursor会产生`root/item#0...#N`
  （`attempt.rs:462-474`）。因此一个动态列表历史上扩到N、随后缩回1时，两张map仍保留N个ID；
- 当前512-generation回归只使用`ComponentId::root()`（`task.rs:1020-1022`, `task.rs:1127-1159`），只能证明同一ID
  不按generation增长，不能证明由**当前**Component cardinality约束。

此前candidate因此保持in progress。最终实现改用per-retirement transient claims：core在`Retire` enqueue前安装，
actor消费同一command后接管；matching tasks全部abort/join后，两侧claim与weak waiter一并释放。永久stale authority
由`MountFence`提供，registration持有该fence完成`Start` enqueue，unmount先invalidate再发送`Retire`，core mutex
和actor FIFO保留线性化顺序。

独立closure复核：task supervisor 14/14、async-task context 5/5通过；512个distinct `ComponentId`从512 live缩到1
再到0时core/actor claim均归零，原distinct-ID回归独立重复50/50；阻塞Drop期间claim拒绝late start，cleanup后旧
context仍由MountFence返回`StaleMount`。源码的install/release、queued retirement、panic/shutdown clear路径均成对。
因此FDR-036接受resolved。

### FDR-037 High: owning Tokio runtime关闭会让supervisor永久伪装成 Healthy

状态：**resolved；supervisor与Application三个driver boundary的runtime closure均获独立签收。**

证据：

- actor只在`run_actor_inner()`返回或unwind被`run_actor()`捕获时更新core；actor future因Tokio runtime
  shutdown被直接Drop时，没有RAII finalizer调用`close_after_actor_failure()`（`task.rs:572-582`）；
- `SupervisorState`仍保留`accepting=true`、`closed=false`和旧`ActorControl`，因此
  `TaskPanicMonitor::status()`继续返回`Healthy`，`wait()`也没有任何后续Notify来源；
- `Application<P>`和supervisor没有runtime-affinity fence，也没有`!Send`约束；owner可以合法移出创建actor的
  runtime，或比该runtime活得更久；
- reviewer在`/tmp/agentview-phase8-review`增加隔离回归：runtime A内start一个pending mount task并取回
  supervisor，Drop runtime A后在runtime B检查monitor。当前稳定失败为
  `left: Healthy, right: Closed`；若继续等待monitor则永久Pending。

影响：

这是普通runtime lifecycle，不依赖panic。driver可能在task actor和全部sidecar已经消失后仍认为supervisor
健康；`wait_for_driver_demand()`可永久hang，`react()`在没有new task start/retirement触发channel send时还可
继续提交Frame。直到某次send或显式shutdown碰到closed channel，状态才被动修正，违反task runtime fail-closed
和driver wait可终止性。

要求：

- actor future必须有覆盖normal return、unwind和cancellation Drop的RAII lifecycle finalizer，在任何非owner
  shutdown退出时同步把core设为Closed、关闭retirement waiters并Notify monitor；
- 或明确冻结并强制Application runtime affinity，使owner不可能比创建actor的runtime活得更久；仅靠文档约定
  不足以证明当前`Send`类型安全；
- 增加跨runtime回归，证明runtime A关闭后monitor在runtime B确定返回Closed、pending retirement不hang、
  后续start/react在handoff前fail closed。

reviewer closure复核：

- `ActorLifecycle` RAII、monitor/retirement逐pollaffinity fence和shutdown迁移路径方向正确；supervisor完整12项通过，
  owning-runtime shutdown与foreign-runtime filters各独立重复50轮通过；
- 但`begin_outer_driver_boundary()`只仲裁`Panicked`，不会把`TaskSupervisorStatus::Closed`映射成driver failure
  （`application.rs:93-110`）；
- `react()`和blocking demand随后poll `monitor.wait()`，所以仍会看到Closed；nonblocking
  `take_driver_demand()`在preflight后只再次检查Panicked，随后直接返回`driver_demand.take()`
  （`application.rs:332-341`）。owning runtime关闭后，该“下一次driver observation”因此可返回`Ok(false/true)`，
  而不是`StaleMount`；
- 现有runtime lifecycle回归都停在supervisor API（`task.rs:1337-1524`），没有覆盖Application的三个driver入口与
  zero-handoff fence。

此前candidate因此保持in progress。最终outer-boundary arbiter显式区分Application terminal state与supervisor
`Closed`；`react()`在创建reaction future前返回typed Component-runtime terminal，blocking/nonblocking demand均返回
`StaleMount`，nonblocking take在消费sticky demand前后各仲裁一次，fresh panic precedence保持不变。

独立closure复核：supervisor 14/14通过；owning-runtime与foreign-runtime filters此前各重复50/50；Application级
runtime A关闭后在B调用react/wait/take的回归独立重复50/50，预置`Ok(true)` demand仍被拒，port计数保持
`declare/render/submit/handoff = 1/1/0/0`。因此FDR-037接受resolved。

### FDR-038 High: task panic被sibling drain阻塞且可能被framework panic覆盖

状态：**resolved；独立review接受当前implementation candidate，Phase 8仍受后续finding阻塞。**

证据（finding提出时的历史实现；当前实现见本节“独立closure复核”）：

- `resume_supervised_task_panic()`调用`take_after_drain()`，只有`panic_drained=true`才取payload并
  `resume_unwind`（`application.rs:50-56`, `task.rs:289-295,508-523`）；
- actor锁存第一项panic后abort全部siblings并逐个await JoinSet，最后才`mark_panic_drained()`
  （`task.rs:650-716`）。任一task destructor或cancellation cleanup永久阻塞，outer driver便永远不能unwind；
- reconcile内部把`MountTaskSupervisorError::Panicked`传给`from_task_supervisor()`，后者使用
  `assert_ne!(Panicked)`（`application.rs:419-424,880-888`）。panic在outer monitor本poll检查之后、
  `tasks.retire()`之前锁存时，该assert直接用framework payload覆盖原task panic；
- reviewer在`/tmp/agentview-phase8-review`构造确定交错：slow synchronous rerender唤醒panicking task，等待
  monitor已为Panicked，再移除一个task-bearing child。当前得到原task panic后，实际逃逸payload来自
  `a Component task panic must resume its original payload` assertion；original `&str`无法downcast；
- 全量451测试中的bootstrap panic断言还存在调度flakiness，详见本节policy note。

影响：

这与owner刚冻结的“panic后直接stack unwind”相反。一个普通阻塞Drop可以把panic传播变成永久Pending；另一个
合法same-poll时序会传播framework assertion而不是原panic。两者都发生在outer driver API内部，不能由caller
修复，也不能降级成typed Application fault。

要求：

- 第一项task panic被supervisor观察后，应立即使当前/下一outer driver boundary取得原payload并
  `resume_unwind`；sibling abort可以同步发起，但drain completion不能阻塞unwind；
- 撤回`Cargo.toml`的`panic = "abort"`和engine/plan中的process-fatal contract；workspace release与下游
  embedding都保留Rust stack unwinding；
- 移除`from_task_supervisor(Panicked)` assertion路径。所有可能同步观察Panicked的preflight、retire、start和
  wait边界必须回到同一个panic arbiter，不能制造第二个panic或structured fault；
- unwind开始前把Application标为不可复用；caller主动catch后，后续API要确定terminal fail closed，不能再次
  `take()`已经消费的payload；
- 增加blocking sibling Drop、panic-during-reconcile、bootstrap registration race和无active driver时下一边界
  unwind回归；测试不能假设异步registered task的首次poll顺序。

独立closure复核：

- Application arbiter先把共享state原子置为`APPLICATION_TERMINATED_AFTER_TASK_PANIC`，再立即take原payload并
  `resume_unwind`（`application.rs:50-63`）；catch后的`react`、driver demand和shutdown均返回terminal fault，
  不会第二次take payload；
- supervisor在锁存第一项payload后、任何await之前执行`abort_all()`，而payload publication不再依赖drain
  （`task.rs:586-595`, `task.rs:869-906`）；secondary Drop panic不会覆盖first payload；
- reconcile的host preflight、retire、retirement wait与task start所观察到的`Panicked`均调用同一Application
  arbiter（`application.rs:526-595`），旧`assert_ne!(Panicked)`路径已删除；outer react还在drop provider
  stream时抑制secondary destructor panic并优先传播task payload（`application.rs:93-99`, `:420-449`）；
- reviewer复跑全部13项`task_panic` focused tests、49项Application tests和12项supervisor tests均通过；另在
  `/tmp/agentview-fdr38-review.O37v0r`连续执行500次retirement-panic交错，未出现额外handoff、payload替换或
  framework panic。因此FDR-038本身可以接受resolved。

### FDR-039 High: pending supervisor shutdown可迁移到非owning runtime并永久挂起

状态：**resolved；独立review接受supervisor-level implementation candidate，production wrapper缺口另见
FDR-040。**

证据：

- finding复现时，`MountTaskSupervisor::shutdown()`只在进入future时检查一次runtime affinity；随后
  `begin_shutdown(false)`从core移走`ActorControl`，并直接await其Tokio `JoinHandle`。boxed shutdown future可先在
  current-thread runtime A上poll到Pending，再被移动到runtime B；若A仍存活但idle，actor仍绑定A，B上的join
  不会自行完成；
- `ActorControl`被取走后，core不再保留该actor的`runtime_id`，因此只通过core执行的后续affinity检查无法识别
  已经pending的shutdown future发生迁移。这是FDR-037 monitor/retirement pending-wait迁移问题在consuming
  shutdown路径上的同类production缺口；
- 当前并发implementation candidate已经把提取出的`runtime_id`保留在shutdown future中，并用逐poll
  affinity fence驱动join；foreign-runtime poll会abort旧actor、调用`close_after_actor_failure()`并返回
  `Closed`（`task.rs:122-157`）；
- repository中也已加入确定性候选回归：先在runtime A把shutdown poll到Pending，再在A保持存活但idle时移到
  runtime B，要求shutdown返回`Closed`，pending retirement关闭且monitor变为Closed
  （`task.rs:1479-1522`）。这些构成下方独立closure复核的直接证据。

影响：

consuming shutdown是Phase 8 production teardown的完成证明。若future迁移后永久Pending，caller无法取得
shutdown acknowledgement，Component task actor和retirement waiters也没有确定终态；仅在future入口检查
runtime affinity不足以约束一个可移动、可多次poll的async operation。

要求：

- shutdown必须在提取`ActorControl`后继续持有其`runtime_id`，并在每次join poll时检查runtime affinity；或者
  从类型上证明shutdown/Application future不可跨runtime迁移；
- mismatch必须fail closed：abort旧actor、关闭retirement waiters、Notify monitors，并确定返回`Closed`，不得
  在foreign runtime等待仍绑定owning runtime的JoinHandle；
- 独立复跑上述pending-migration regression，并核对normal owner-runtime shutdown、actor panic和重复terminal
  observation没有退化；closure结论记录在下方复核段。

独立closure复核：

- extracted `ActorControl.runtime_id`由shutdown future继续持有；join的每次poll都先比较当前runtime identity，
  mismatch时abort actor、执行`close_after_actor_failure()`并返回`Closed`（`task.rs:122-164`）；
- repository的A到B pending migration回归通过；全部12项supervisor tests也通过，normal shutdown、actor panic、
  retirement close和terminal observation未见退化。因此supervisor-level FDR-039可以接受resolved；公开
  `ExternalApplication`在该inner fence之外仍有独立迁移窗口，记录为FDR-040。

### FDR-040 High: production External shutdown waiter迁移后绕过inner runtime fence并永久挂起

状态：**resolved；独立review接受当前implementation candidate，Focused closure不等于Phase 8整阶段sign-off。**

证据：

- `ExternalApplication::shutdown()`在同步调用时直接把`shutdown_owned()`用`tokio::spawn`固定到当时runtime，随后
  返回一个明确`Send + 'static`、只等待该`JoinHandle`的future（`external.rs:922-932`）；
- FDR-039候选修复的逐poll runtime fence位于更内层的`MountTaskSupervisor::shutdown()`。如果outer cleanup task
  尚未在owning current-thread runtime A获得poll，调用方就把公开shutdown waiter移到runtime B，B只会poll
  cleanup `JoinHandle`；inner Application/supervisor future从未获得poll，因此没有机会观察runtime mismatch并返回
  `Closed`；
- reviewer在`/tmp/agentview-fdr38-review.O37v0r`增加隔离回归：runtime A内构造无active reaction的
  `ExternalApplication`并调用`shutdown()`，立即把返回waiter带出`block_on`；A保持存活但idle，在runtime B用
  100ms timeout等待。回归稳定timeout，证明即使没有Provider stream或Component task cleanup阻塞，production
  shutdown仍不能完成或fail closed。

影响：

`ExternalApplication::shutdown()`是FDR-035用于证明production owner teardown完成的唯一公开路径，它又在类型上
承诺waiter可跨线程/runtime移动。当前outer spawn把Application ownership困在不再驱动的runtime中，导致caller
既得不到cleanup acknowledgement，也得不到typed terminal failure；FDR-039的supervisor修复因此不能单独闭合
production shutdown的同类迁移窗口。

要求：

- public shutdown waiter必须逐poll校验cleanup runtime identity，并在foreign runtime确定fail closed，或者从类型上
  移除`Send`并证明它不能跨runtime迁移；不能在一个不可观察迁移的runtime-bound task外只await `JoinHandle`；
- 如果保留“调用时立即取得唯一owner、丢弃waiter不取消cleanup”的contract，修复必须同时保留该cancellation
  guarantee；runtime mismatch路径至少要确定返回typed failure，不能为了可迁移而把ownership交回可取消waiter；
- 增加production `ExternalApplication::shutdown()`的A到B迁移回归，覆盖cleanup首次poll前迁移，以及cleanup已经
  Pending后的迁移；同时保留正常shutdown、waiter Drop、active reaction join和task destructor barrier测试。

独立closure复核：

- public waiter保存创建cleanup task的Tokio runtime identity；每次poll先比较当前runtime，foreign/no-runtime
  poll立即返回typed `ExternalApplicationFault::ShutdownTaskFailed`，不会poll一个绑定到A的`JoinHandle`
  （`external.rs:924-944`）；
- 返回foreign-runtime failure后，poll future持有的`JoinHandle`被drop/detach而不是abort，独立cleanup task继续
  持有唯一`ExternalApplication` owner。repository tests分别覆盖cleanup首次poll前和已经Pending后的A到B迁移
  （`external/tests.rs:171-264`），两项均在独立target通过；
- reviewer另在`/tmp/agentview-fdr40-closure.XXiXzH`加入Application ownership barrier：B取得typed failure后
  root factory仍存活；恢复驱动A后，detached cleanup在1秒内释放该owner。该回归通过，证明修复没有以typed
  failure为代价取消cleanup；既有waiter Drop、active reaction join和task destructor barrier仍保留。

因此FDR-040可以接受resolved；本次只签收038至040的focused closure，不替代Phase 8其余authoring/lifecycle
surface的独立review。

### FDR-041 High: 已有terminal state会遮蔽随后锁存且尚未传播的Component task panic

状态：**resolved；direct Application与External production closure均已获独立focused签收。**

> **Historical trigger:** 以下cancellation-created terminal交错描述的是已被取代的实现。FDR-041仍保留的
> 通用结论是outer driver必须优先传播尚未消费的task panic；当前cancellation recovery还必须在monitor已经
> 锁存panic时保持fallback hidden，而不是创建cancellation terminal。

证据：

- `Application::react()`先把`APPLICATION_TERMINATED_AFTER_CANCELLATION`映射为typed
  `CancelledAfterHandoff`并return，之后才创建/check task panic monitor（`application.rs:407-419`）；
- `wait_for_driver_demand()`和`take_driver_demand()`具有相同顺序：先因非`APPLICATION_READY`返回
  `StaleMount`，之后的monitor check不可达（`application.rs:282-290`, `application.rs:319-326`）；
- post-handoff reaction cancellation只terminalize Application，不会同步fence/abort mount-scoped tasks。
  因此合法交错是：handoff后drop `react()` future -> state变为cancelled -> 仍存活的`use_future`随后panic ->
  supervisor锁存原payload -> 下一次`react()`或demand boundary先返回typed cancellation/stale。原payload只有
  caller改走`shutdown()`时才会被观察；现有“no active driver”回归只覆盖`APPLICATION_READY`状态
  （`application.rs:3440-3484`），没有覆盖该交错；
- `engine.md:820-825`明确要求没有active driver时保留payload到下一次`react()`、driver demand wait或integration
  driver调用；invariant 26（`engine.md:928-930`）也不允许把这项尚未传播的user panic降级成typed fault。

影响：

post-handoff cancellation、runtime terminal classification与稍后发生的Component task panic之间存在稳定顺序，不是
仅同poll race。driver会看到typed cancellation而不是原user payload，违反本轮刚冻结的panic transparency owner
决定。External等integration如果在取得terminal Application后再次驱动reaction，也会继承该遮蔽。

要求：

- 每个outer driver boundary必须在返回既有terminal classification前先仲裁monitor中**尚未消费**的panic；已经
  `resume_unwind`且Application state为`APPLICATION_TERMINATED_AFTER_TASK_PANIC`时仍只返回typed terminal fault，
  不能重复take或重复unwind；
- 保持post-handoff cancellation的declare/render/submit fence，不得为了传播panic重新进入terminated operation；
- 增加确定性交错回归：先形成post-handoff cancellation terminal，再触发仍存活mount task panic，分别证明下一次
  `react()`、pending/immediate demand boundary和至少一条production integration driver boundary传播exact原payload；
  同时证明caller catch后后续API仍fail closed且不会第二次传播。

reviewer复核结果：

- `begin_outer_driver_boundary()`的双检查与最终state重读闭合了三个direct Application terminal快路径；direct
  regressions独立单次3/3通过，随后重复20/20轮通过；既有`post_handoff_cancellation` filter 4/4和完整
  `task_panic` surface 17/17通过；
- External candidate不是稳定证明。独立先执行完整External 27/27通过，但随后把
  `late_component_task_panic_after_cancellation_crosses_external_boundary`单独重复50轮，结果为**37 passed / 13
  failed**。失败时原task payload已经在Tokio worker panic，public `observe()`却返回
  `Err(Control(ObservationChannelClosed))`；
- 根因窗口位于`ExternalApplication::await_next_observation()`：Application因`resume_unwind`被drop后先关闭
  observation sender，`control.next_observation()`可在reaction `JoinHandle`发布Ready前一个调度步返回closed。
  biased `select!`只能解决同一poll两者都Ready，不能解决channel closure先Ready；该分支在未join reaction task的
  情况下立即把typed control fault返回caller（`external.rs:1117-1165`）。

此前candidate因此保持open。follow-up随后把`ObservationChannelClosed`作为sender ownership已经销毁的证明，
先join已安装的reaction task，再与正常JoinHandle-ready路径共用terminal classifier；panic `JoinError`恢复原payload，
abort仍typed fail closed。

独立closure复核：

- reviewer检查了persistent sender与`OwnedPermit`的完整ownership路径；receiver没有独立close入口，closure后不会再有
  producer工作，只剩Tokio completion publication；
- deterministic close-before-completion回归证明channel先关闭时首次poll保持Pending，释放reaction后传播exact panic；
  permit sender-count回归固定`1 -> 2 -> 1`，abort回归在1秒内返回`ReactionTaskFailed`；
- reviewer独立执行External完整surface 30/30，并把原
  `late_component_task_panic_after_cancellation_crosses_external_boundary`重复100轮，得到**100 passed / 0 failed**；
  `cargo fmt --all -- --check`与`git diff --check`通过。实现方另报告完整门禁library 471/471、CLI 20 passed / 1
  existing ignored、check与strict Clippy通过。

因此FDR-041接受resolved。该focused签收不替代FDR-035至FDR-037与Phase 8完整surface review。

### FDR-042 High: public `use_reaction_request()`没有public Application demand-consumer

状态：**resolved；public blocking/nonblocking consumer与真实Agent scheduling已获独立签收。**

证据：

- Phase 9 Task 1 candidate公开`Application<P>`、`mount/current_projection/react/shutdown`，并继续公开
  `use_reaction_request()`；但唯一receiver入口`wait_for_driver_demand()`与`take_driver_demand()`仍是
  `pub(crate)`（`application.rs:320`, `application.rs:352`），execution public module甚至把driver demand
  plumbing明确描述为private；
- downstream Component因此可以成功调用`ReactionRequest::request()`并向该Application的private sticky channel
  写入demand，但持有public `Application<P>`的external driver既不能等待也不能nonblocking消费它；除了调用
  `react()`猜测轮次外，没有public scheduling observation；
- `engine.md:700-720`要求Autonomous Agent用mount-fenced Component-to-driver demand表达“至少一个更晚turn”；
  `engine.md:819-832`把blocking wait和nonblocking take列为与`react()`共享panic/Closed arbiter的三个outer
  boundaries，invariant 21（`engine.md:934`）要求external driver拥有when-to-react policy；计划
  `:816-826`明确说这个capability不能排到public examples之后才验证，Phase 9 gate还要求真实Agent scheduling
  executable而非trait-only compile。

影响：

Task 1 candidate的public hook是一个下游无法观测的单向capability。crate-private integration测试可以通过，但外部
crate无法实现权威contract中的Autonomous Agent driver，也无法确定地证明request-before-wait、request-during-wait、
coalescing或task-panic precedence。用timer或无条件循环调用`react()`会把scheduling policy猜测成polling，不能作为
Phase 9 Agent example的验收证据。

要求：

- 在不暴露`DriverDemandHandle`、mutable port或FrameSession的前提下，为public `Application<P>`提供curated
  blocking和nonblocking demand-consumer；可以复用现有实现，但不要仅为了visibility冻结不必要的internal类型；
- 两个入口必须继续经过与`react()`相同的outer-boundary arbiter，保持fresh task panic precedence、supervisor
  `Closed` fail-closed、mount fence、sticky/coalescing和consume-once语义；consuming shutdown后由Rust move阻止复用；
- 增加真正downstream public compile/runtime tests：Component在driver wait前和wait期间分别request、多个request
  coalesce、nonblocking take只消费一次；至少一项test必须通过public API观察task-panic/Closed precedence；
- Phase 9 Agent executable必须用该public consumer驱动下一轮，而不是crate-private wrapper、timer polling或无条件
  `react()` loop。

`mount_with_events()`保持crate-private不是本finding：`engine.md:740-757`明确要求目标
`use_provider_event_handler(selector, handler)`不暴露`EventInput`/legacy listener API。

独立closure复核：

- `wait_for_reaction_request(&mut self)`与`take_reaction_request(&self)`只返回payload-free
  `ApplicationFault`（`application.rs:338`, `application.rs:379`）；两者在消费前复用shared outer-boundary
  arbiter，blocking completion与nonblocking take后再次仲裁，internal `DriverDemand*`没有public export；
- downstream 3/3覆盖request-before-wait、wait-Pending后request、三次coalesce、consume-once、dirty Signal先写、
  projection revision不变及零submit；panic回归先把public waiter poll到Pending，再通过`Notify`释放Component task，
  在有界时间内捕获exact `String` payload，不含`yield_now`或调度猜测；
- `frame_agent`用explicit `Notify`把task Signal write + request固定在第一轮之后，driver通过public blocking consumer
  决定第二轮；独立检查得到first Full、second Delta且payload包含published state；成功/错误路径都执行consuming
  shutdown；
- reviewer独立执行public demand 3/3、Application 52/52、downstream trybuild lifecycle、no-default lib check、example
  test/binary、format和diff gates；完整public demand与Agent example分别重复50/50，全部通过；
- `engine.md:77-95`已同步两个public签名、sticky/coalescing、no-render/no-declare/no-submit与panic/Closed arbiter。

因此FDR-042接受resolved；这不关闭FDR-043或Phase 9整体gate。

### FDR-043 Medium: native curated surface仍公开superseded EventInput/EventListener contract

状态：**resolved；default legacy compatibility与no-default curated native surface均获独立签收。**

证据：

- `engine.md:685-698`冻结高层root为普通无runtime props Component，`engine.md:740-757`进一步明确目标authoring
  surface是`use_provider_event_handler(selector, handler)`，公共API不暴露`EventInput`、
  `EventListener::observe`、listener identity/version或DOM event routing；
- Task 1 candidate仍从`component::authoring`和`component::prelude`无条件公开`EventInput`与`EventListener`
  （`authoring.rs:28-29`, `component/mod.rs:22-23`），prelude还无条件导出legacy `ComponentEvents` derive；
- public `ComponentHost<Props>`继续把`fn(Props, EventInput<ProviderEvent>)`放在类型字段和`new()`签名中
  （`host.rs:47-62`）。Task 2 feature计划只gate `ProviderPort`、`ApplicationHost`与
  `ComponentReactionRuntime`，并明确preserve event types；因此`default-features = false` downstream仍可编译
  superseded event contract。

影响：

`legacy-provider-port` feature即使隐藏旧provider trait，也没有形成curated native boundary。下游仍会从推荐prelude
看到两套互斥authoring模型，继续编写event-input root与listener identity/version；下一minor删除legacy feature时这些
API没有清晰归属，README和native examples也无法证明迁移完成。

要求：

- internal admission/dispatch可以继续使用EventInput实现细节；public `ProviderEvent` selector和
  `use_provider_event_handler`必须在`--no-default-features`可用；
- default-enabled `legacy-provider-port`窗口可以公开旧`EventInput`、`EventListener`、`ComponentEvents`和依赖它们的
  event-taking Host/runtime入口，但必须与legacy provider surface一起deprecated；无默认feature的curated prelude/
  authoring path不得导入这些名字；
- 若低层`ComponentHost<Props>`作为provider-neutral rendering API继续公开，应提供不要求EventInput的root constructor；
  event-taking constructor应属于compatibility surface。不要为了保留旧Host签名把EventInput重新提升为native contract；
- 扩展真实downstream feature fixtures：native consumer用`Application::mount`和
  `use_provider_event_handler`成功；legacy consumer在no-default下导入`EventInput`/`EventListener`及event-taking Host
  必须失败，在default legacy feature下编译并产生deprecation；迁移后的新examples不得使用这些名字；
- 每个legacy surface必须有独立或不可互相遮蔽的probe，至少分别覆盖provider ownership、prelude与direct
  `component::authoring` event imports、ComponentHost event constructor和ExternalApplication event constructor；一个
  聚合crate的任意nonzero exit或一条泛化`deprecated` warning不能证明其余符号已隐藏/标记；
- root crate在default/all-features与no-default两种配置下都必须通过all-target strict Clippy `-D warnings`；下游
  fixture刻意捕获的deprecation diagnostics不算root warning。不得使用crate-wide `allow`或命令行waiver；真正
  feature-private实现应cfg掉，必须共享的内部machinery只允许局部、可解释的`cfg_attr`，legacy tests/examples应有
  窄compatibility allowance。

独立closure复核：

- `Cargo.toml:12-14`定义default-enabled `legacy-provider-port`；legacy ownership modules/exports与built-in
  `ProviderPort` impl按feature编译，native `ReactionPort` impl、`ProviderEvent` selector、
  `use_provider_event_handler`和internal dispatch保持无条件；没有增加legacy/native协议adapter；
- no-default下`component::authoring`不公开`EventInput`/`EventListener`，prelude不公开二者或
  `ComponentEvents`；`ComponentHost::new_root`与`ExternalApplication::new_root`不带EventInput。default feature
  保留原event-taking `new`名字并在associated function本身发出deprecation，保持一个minor源码兼容窗口；
- downstream matrix 4/4通过：一个native no-default consumer，十个互不遮蔽的legacy symbol/constructor probes分别
  证明no-default不可用、显式feature可用且逐项deprecated、普通default dependency自动启用compatibility；constructor
  mutation test还拒绝仅签名不兼容的public `new`，真实negative均为E0599 missing-`new`；
- reviewer独立执行all-features完整suite：library 475/475、CLI integration 20 passed / 1 existing ignored，其他
  integration、trybuild与doc suites无失败；no-default library 430/430，native Application/async authoring/
  projection/signal为1/4/5/14；
- default/all-features与no-default均通过workspace all-target check和不带waiver的strict Clippy；两种feature配置的
  rustdoc `-D warnings`、root与三个nested fixture format、`git diff --check`全部通过；matrix生成的三个Cargo.lock已
  清理；
- warning修复以真实cfg ownership为主：Responses/Chat legacy continuation/history/stream/tool state按feature隔离；
  compatibility tests/examples仅使用带reason的窄allowance。`event_input.rs`的file-level dead-code allowance来自
  Task 2 baseline，不是本finding引入的掩盖。

因此FDR-043接受resolved。这里只关闭Phase 9 public compatibility slice，不代表native examples、Chess或README已
迁移，也不关闭Phase 9总gate。

### FDR-044 High: no-default compile harness仍无条件执行legacy-only UI fixtures

状态：**resolved；default与no-default compile topology及完整no-default suite已获独立签收。**

证据：

- `cargo test --no-default-features --test component_api_component_compile -- --nocapture`独立结果为0/2；public pass
  harness中`pass_static_component`、`pass_event_select`、`pass_event_selector_names`和
  `pass_provider_port_one_method`分别因EventInput/ComponentEvents/ProviderPort不可见失败；
- compile-fail harness用`fail_*.rs`通配符无条件执行九个legacy EventInput/ComponentEvents fixtures；其expected
  diagnostics是legacy derive/type contract，no-default actual则先在public import处失败，导致九个snapshot mismatch；
- FDR-043 matrix已正确证明no-default下这些API不可见；当前harness却把“不可见”当成自身失败，所以
  `cargo test --no-default-features --no-fail-fast`整体exit 101。Task 3报告把它列为out-of-scope concern，但Task 3 brief
  和Phase 9 gate均明确要求完整no-default regression，不允许跳过。

影响：

curated no-default library、examples和strict Clippy可以通过，但完整test topology不能通过。release或CI一旦关闭default
feature就得到红色suite；更严重的是compile harness无法区分native public contract与default-enabled legacy
compatibility contract，后续删除compatibility feature时没有可靠迁移基线。

要求：

- 保持default/all-features下现有legacy pass/fail fixtures全部执行并匹配；
- no-default下继续执行所有native pass/fail fixtures，只条件排除真正依赖EventInput/EventListener/
  ComponentEvents/ProviderPort的legacy fixtures；不得跳过整个compile harness，也不得bless“找不到legacy API”为原本
  derive语义的snapshot；
- 把`pass_static_component`迁移成无EventInput的`ComponentHost::new_root` fixture，使静态Component/prelude coverage
  在no-default仍存在；`pass_event_select`、`pass_event_selector_names`和`pass_provider_port_one_method`属于legacy
  compatibility，可按feature注册；
- compile-fail fixture discovery必须仍覆盖未来新增的native `fail_*.rs`，不能用一份容易漏项的冻结总清单；用明确
  legacy classification过滤动态发现，或提供等价的complete-registration证明；
- 独立通过该harness的default与no-default两种配置，以及完整`cargo test --no-default-features --no-fail-fast`、双配置
  strict Clippy、format和diff；不得削弱FDR-043逐symbol negative matrix。

独立closure复核：

- `pass_static_component`已改为event-free root并使用`ComponentHost::new_root`（
  `pass_static_component.rs:43`）；no-default继续保留static Component/prelude/derive coverage；
- harness动态读取、排序全部`fail_*.rs`（`component_api_component_compile.rs:17`），no-default只过滤明确九项
  legacy fixtures；三个legacy pass只按feature注册。default/all-features实际执行16 pass + 43 fail，no-default执行
  13 pass + 34 fail，两种配置均2/2通过且原legacy snapshots未重写；
- reviewer独立完整no-default suite通过：library 430/430、compile harness 2/2、matrix 4/4、CLI 20 passed /
  1 existing ignored，其余enabled integration/trybuild/doc targets无失败；all-features完整suite同时保持library
  475/475及其他所有target通过；
- Task 3 native examples亦获独立签收：signal 8/8并stress 50/50，provider acceptance 4/4并stress 30/30；
  `signal_reaction`严格证明mount零submit、Full→DeltaFrom、delta期间state仍Pending、terminal handler已完成、第二Frame
  含published state、typed mismatch cleanup和task destructor；
- scripted target使用独立domain 4、typed exhaustion、private accepted revision与sticky terminal；retryable capture
  regression证明pre-handoff Err不消费script/capture/revision，所有fallible capture完成后才在same-poll success path
  pop script并commit（`scripted_provider.rs:205`, `:354`, `:391`）；
- Responses acceptance只用真实public `Application<AsyncOpenAiResponsesProvider>`与loopback request bodies；旧owner
  shutdown后才从explicit phase B mount distinct fresh target且first Frame为Full。old/fresh/error三类owner均在task-start
  barrier后执行，consuming shutdown完成后Drop probe才为true；success/error server均join，operation panic在cleanup后按
  原payload恢复（`component_runtime_visual_acceptance.rs:601`, `:753`, `:807`, `:949`）；
- 双配置workspace all-target check、无waiverstrict Clippy、format和diff均独立通过；matrix生成lockfiles已清理。

因此FDR-044接受resolved，Task 3 native signal/provider acceptance slice关闭。该closure本身不提前签收Task 4；
Task 4当前状态由下方FDR-045独立closure更新。Chess和README仍未签收，Phase 9总gate保持打开。

### FDR-045 High: public External exchange没有curated act/empty-completion constructor

状态：**resolved；public typed/empty completion与Task 4 executable workflows已获独立签收。**

证据：

- `ExternalProviderPort::new()`、`ExternalControl::next_observation()`和`ExternalControl::act()`公开，所以下游可以把port
  移入public `Application<P>`并收到accepted Frame；但`ExternalAct`唯一public constructor是
  `#[doc(hidden)] __from_cli_json_lines()`（`external.rs:700`），绑定CLI私有JSON-lines wire语法；
- normal typed text constructor `from_text_protocol`仅在`cfg(test)`且`pub(crate)`（`external.rs:729`）；empty
  `ReactionCompleted { primary_text: None }`路径`complete_without_output`也是`pub(crate)`（`external.rs:326`）；
- `Application::react()`在handoff后等待该reaction-local fact stream。公共Skill/Plugin driver若不调用隐藏CLI方法，既
  无法提交文本act，也无法合法结束empty reaction，因此Task 4只能永久Pending、复制External state machine或依赖不稳定
  hidden API；三者都不能作为curated public workflow证据；
- `engine.md:713-715`要求cloneable control handle按active ingress generation注入act，late act必须明确stale；Phase 9
  要求真实Skill/Plugin executable而非trait-only proof。

影响：

当前公开surface是半个协议：handoff可见但completion authority不可用。它会诱导下游使用CLI wire decoder作为库API，
并使Skill frontend/exchange separation、Plugin stale-ingress和consuming shutdown无法通过稳定public path证明。

要求：

- 增加最小provider-neutral public typed入口，例如`ExternalAct::text(text)`与
  `ExternalControl::complete(generation)`；命名可按local style调整，但不得要求下游构造CLI JSON或ProviderFact；
- text act必须复用既有size bound、TextSealed + ReactionCompleted grammar与one-shot ingress claim；empty completion只
  产生`ReactionCompleted { primary_text: None }`；两者均受active generation fence，重复/late调用返回
  `ExternalControlFault::StaleIngress`；
- 保持`__from_cli_json_lines`仅为CLI compatibility实现，不能把它重新包装成推荐public core；不得暴露fact sender、
  FrameSession或mutable target state；
- 增加真实downstream no-default compile/runtime gates，分别证明public text、empty completion、late stale、ordered facts
  与cleanup；现有internal Skill/Plugin wrappers应复用同一public primitive而非保留第二套completion逻辑；
- Task 4 Agent/Skill/Plugin examples必须只使用可见public API，且Skill/Plugin first Frame与Agent canonical payload的
  byte-for-byte golden不能借助hidden constructor。

独立closure复核：

- `ExternalAct::text`（`external.rs:706`）构造normal typed completion；`ExternalControl::complete`
  （`external.rs:345`）构造empty completion。两者分别在`external.rs:267`和`:349`进入同一
  `claim_ingress` fence；text继续复用4 MiB bound与`TextSealed`→`ReactionCompleted(Some)` grammar，empty只发送
  `ReactionCompleted(None)`。`__from_cli_json_lines`仍为`#[doc(hidden)]`（`:717`）；Task 4 examples与downstream
  test未引用hidden decoder、fact sender、FrameSession、mutable port或event-taking mount API；
- no-default downstream test（`component_api_external_completion.rs:85`）只经public imports运行text与empty两轮
  exchange，证明handler完成顺序、admitted text与handler state进入下一轮Delta、text/empty重复及late generation均
  返回`StaleIngress`，且consuming shutdown完成Component task destructor。direct External regressions
  （`external/tests.rs:1231`, `:1265`, `:1290`）锁定exact fact grammar、one-shot claim与output limit；internal
  Skill/Plugin controls复用相同public `complete` primitive；
- Agent、Skill和Plugin分别经自己的真实public driver path断言同一checked-in exact canonical first-Frame bytes
  （`frame_agent.rs:77`, `frame_skill.rs:223`, `frame_plugin.rs:430`, golden `frame_workflow_golden.rs:10`）。Agent保留
  demand-driven Full→Delta；Skill test（`frame_skill.rs:240`）证明latest和typed command不render/submit，explicit
  exchange才看到dirty state；Plugin test（`frame_plugin.rs:559`）证明one Application per parent、distinct target、
  old ingress stale、same-parent second Delta/canonical history、other parent idle与全部owner consuming shutdown；
- Plugin partial setup会清理已mount owner（`frame_plugin.rs:191`, test `:447`）；final drain即使遇到error/panic也尝试
  每个owner（`:137`, test `:489`），并在cleanup完成后恢复原始operation panic payload且保持其优先级（`:224`,
  test `:529`）。这些catch只存在于example ownership harness，runtime panic contract未改变；
- reviewer focused结果为External 33/33、internal roles 3/3、downstream completion 1/1、Agent 2/2、Skill 2/2、
  Plugin 5/5，三个no-default binaries均成功；独立stress为completion 40/40、Skill 40/40、Plugin 40/40、Agent
  30/30；
- frozen九文件SHA-256与`CHATROOM.md` handoff逐项一致。独立完整all-features suite为library 478/478，完整
  no-default suite为433/433；两者CLI均20 passed / 1 existing ignored，其余integration/trybuild/example/doc
  targets无失败。双配置workspace all-target check、无waiverstrict Clippy、format与diff check均通过；完整suite
  生成的三个feature-matrix fixture `Cargo.lock`已在最终gate后删除并确认无残留。

因此FDR-045接受resolved，Phase 9 Task 4 public Agent/Skill/Plugin executable slice关闭。Chess/Task 5与README
未由本closure签收，Phase 9总gate保持打开。

### FDR-046 Medium: Task 5 scripted ports在guaranteed handoff前消费response script

状态：**resolved；两个scripted ports与same-Application retry proof已获独立签收。**

证据：

- Task 5要求offline scripted `ReactionPort`本身遵守完整handoff contract；`engine.md:513-527`规定只有
  `Ready(Ok(stream))`证明handoff，`Pending`与`Ready(Err)`都必须允许Application在pre-handoff边界重试；
- `model.rs:506`先校验Frame precondition，随后在`:507`执行`scripts.pop_front()`；但projection capture的
  `render_pom_document`仍可在`:520-532`失败并返回`SubmitFault::Rejected`。该Frame确定未handoff，accepted
  revision/capture虽未推进，下一次retry却已跳过原response script；
- `game.rs:1664-1674`更直接复现：先`pop_front()`取得`RejectBeforeHandoff`，再返回
  `SubmitFault::Rejected(Retryable/Unavailable/Transport)`。现有`provider_rejection_after_mount...`只断言accepted
  submission counter为0，没有断言script queue未消费，也没有在同一个Application上retry；
- 这与repository已接受的scripted provider顺序相反：`scripted_provider.rs:210-253`先完成所有fallible
  pre-handoff preparation，再claim script。Task 5 report关于scripted ports遵守完整ReactionPort contract的结论
  因此尚未成立。

影响：

生产Chess owner、typed state、UCI cleanup与panic arbitration当前未发现该问题；缺陷位于Task 5 conformance
fixtures。但fixtures把“Frame未交付”和“下一次计划response已消耗”混在一起，无法证明pre-handoff provider fault后
Application和port仍可一致重试，也可能让后续测试误把第二个script当作第一次response。Task 5依赖这些fixtures证明
native continuity和fault lifecycle，所以独立gate不能在此状态关闭。

要求：

- `ScriptedChessPort`必须在所有fallible projection capture完成后才claim script；任何pre-handoff render/rejection
  必须保持script queue、accepted revision、capture和submission/lifecycle counters不变；
- `ScriptedGamePort`不得为`RejectBeforeHandoff`执行`pop_front()`；可以先只读front或使用外部test-only fault
  injection，只有crossing poll返回`Ready(Ok(stream))`时才消费对应script并推进accepted state；
- 增加确定性回归，在同一个mounted `Application`上先得到pre-handoff `Rejected`，证明Frame revision/continuity、
  script/capture/counters均未推进；外部清除fault后retry必须消费原来的第一个text script并成功完成，而不是跳到下一项；
- fix仅限`model.rs`/`game.rs`的`cfg(test)` scripted ports与tests，以及Task 5 report/progress bookkeeping。production
  Chess candidate保持冻结；重跑相关focused tests、default/no-default Chess、双配置strict Clippy、format和diff，
  重新冻结hash。Task 6不得开始。

独立closure复核：

- `ScriptedChessPort::submit`现在先完成fallible projection render，再检查external rejection flag、只读
  `scripts.front()`并构造borrowed fact stream；只有crossing poll返回`Ready(Ok(stream))`时才pop script、推进
  remaining count、projection/lifecycle capture与accepted revision（`model.rs:520-589`）；
- `ScriptedGamePort`同样在external rejection flag与front inspection之后构造stream，随后才于successful crossing
  推进script/accepted/submission state（`game.rs:1666-1703`）。原来的queue item rejection已删除；
- 新回归`offline_native_prehandoff_rejection_retries_same_application_without_mutation`（`model.rs:821`）先以旧顺序
  得到RED：`remaining_scripts`实际0、预期1。修复后第一次`react()`返回structured
  `Submit/Retryable/Unavailable/Port(Transport)`，script、accepted、projection、lifecycle均不推进；清除external
  fault后同一Application第二次`react()`得到与失败attempt完全相同的Frame revision、`prepared_against`和Full
  basis，消费原始resignation script、完成typed publication/post-reconcile，最后consuming shutdown使原control stale；
- reviewer独立执行FDR-046 exact 1/1与stress 30/30；default和no-default完整Chess各11/11。production Task 5
  ownership审查确认普通root + example-local `ChessControl`、complete state write→explicit react→typed read，禁止的
  legacy symbols为零；normal/resignation、multi-turn UCI、retry exhaustion、provider rejection、post-handoff timeout、
  setup-after-mount与operation-panic-over-cleanup-panic路径均汇入UCI cleanup和consuming Application shutdown；
- reviewer双配置workspace all-target check、无waiverstrict Clippy、format、diff和offline `--help`通过；完整
  no-default suite为library 433/433，完整all-features suite为478/478，两者CLI均20 passed / 1 existing ignored，
  其余integration/trybuild/doc targets无失败；full suites生成的三个feature-matrix lockfiles已最终删除；
- 最终hash与implementation handoff一致：`model.rs`为
  `a4ba4ccc5a26c3c769c80d8f0a224f5b05ff2096f75023ffbbaae08401a0e362`，`game.rs`为
  `e4da439cd60b9defb77c82a770a002e72f37b42dd473804619adaedf0c0cfb82`，其余Task 5 source hash保持冻结。

因此FDR-046接受resolved，Phase 9 Task 5 native Chess migration关闭。Task 6与Task 7仍未签收，Phase 9总gate
保持打开。

### FDR-047 Medium: README primary native lifecycle snippet不能编译

状态：**resolved；primary snippet及Task 6 documentation已获独立签收。**

证据：

- Task 6要求user-facing runnable snippets使用真实curated public API；candidate在`README.md:49-99`把primary
  lifecycle snippet标记为`rust,ignore`，writer gates因此没有编译该片段；
- reviewer把该片段原样放入一个`default-features = false`的独立downstream crate并执行`cargo run`，编译在
  `README.md:79`失败：`formatted text slots require a captured Rust identifier`。`view!`格式槽不能直接捕获
  field expression `"{props.task}"`；
- reviewer仅在临时harness中增加`let task = props.task;`并改为`task { "{task}" }`后，同一downstream crate
  完整编译、运行、执行一次Debug Full Frame并consuming shutdown成功。Runtime/API实现没有缺陷。

影响：

README把这段代码作为首要Component authoring与Application lifecycle入口，但用户原样使用会在macro expansion
阶段失败。现有`cargo test --doc`的6项ignored和examples gates无法发现这个问题，因此“actual curated public API”的
Task 6证据尚不完整。

要求：

- 仅修改README snippet：先把`props.task`绑定为captured identifier，再传给`view!`；不得改runtime、examples、
  Cargo、Chess文档或Task 1-5 source；
- 对修复后的完整snippet执行独立no-default downstream compile/run，而不是依赖ignored doctest；记录exact command
  与结果；
- 重跑全部credential-free README commands、Task 6双配置docs/examples/strict Clippy gates、format、diff、links，
  冻结README/report/progress hashes；Task 7不得开始。

独立closure复核：

- reviewer从修复后README反向移除唯一两行shape change，重建出的candidate SHA-256精确为
  `b6bb30ae916f6ad7f0d616ace01e2376dd5c4f29744b40d31ae61a4971511012`，证明README除
  `let task = props.task`与`"{task}"`替换外无额外FDR-047改动；
- 修复后的完整README code block与reviewer no-default harness逐byte一致，唯一额外内容是Tokio `main` wrapper；
  `cargo run`成功mount普通root、提交一个Debug Full Frame、满足capture assertion并consuming shutdown；
- reviewer独立运行六个offline binaries，Agent/Skill/Plugin/Signal输出与文档一致，Responses acceptance以
  `ACCEPTANCE PASSED`结束；Chess offline suite为11/11，no-default check和credential-free help均通过；
- reviewer独立运行九项Task 6 commands：doc tests 0 failed / 6 existing ignored、双配置all-examples check、双配置
  strict rustdoc、双配置无waiverstrict Clippy、format与diff全部通过；relative links 14/14，primary path仅在明确
  compatibility section出现legacy名称，feature-matrix lockfile无残留；
- README最终hash为`4f463df886a6ed100d1ed9e953bf8bb5e386796433a222d050462ee103b8dd47`；
  Chess doc、public rustdoc、engine、Cargo与Task 3-5 source hashes保持冻结。README/Chess架构陈述与accepted
  Application/ReactionPort/Chess lifecycle一致，未发现新的semantic或security finding。

因此FDR-047接受resolved，Phase 9 Task 6 documentation slice关闭。Task 7现在正式open，但Phase 9总gate仍须
等待最终full verification与whole-branch review。

### FDR-048 High: current documentation仍把legacy ProviderPort/ApplicationHost边界作为长期权威

状态：**resolved；三份documentation authority修正已获独立签收。**

证据：

- `docs/semantic-agent-view.md:3-7`明确标记自身为current，并声称final Component syntax和long-term plan由
  `provider-port-application-host-boundary.md`拥有；
- `docs/provider-port-application-host-boundary.md:1-3`没有historical/superseded状态，正文在`:15-19`把active
  pipeline定义为`ComponentHost::render -> ProviderPort::execute`，在`:80-93`把orchestration交给
  `ApplicationHost::dispatch_llm_reaction`，并在`:140-154`宣称public provider boundary只有
  `ProviderPort::execute(RenderedProjection)`；
- `docs/provider-port-application-host-open-questions.md:3-8`继续把上述boundary当作当前已确认语义，并在`:24`
  把ToolCall staging归给`ApplicationHost`；
- 这与`engine.md:83`的public `Application<P>` owner、`:159-276`的private FrameSession与
  `ReactionPort::declare/submit`、以及`:273-275`明确只保留one-minor deprecated legacy compatibility直接冲突。
  Task 2 no-default compile surface与Task 6 primary docs也已证明旧边界不是current architecture。

影响：

这是documentation-only finding，但current文档链会把用户重新导向default-feature-only deprecated ownership，
重新引入Phase 9要消除的split history/session owner。它不仅是历史正文未更新：一个明确标记current的入口主动把旧
boundary称为长期权威，因此在release前必须fail closed地切断该authority链。

要求：

- `provider-port-application-host-boundary.md`顶部增加醒目的superseded/historical banner，明确当前权威是
  `engine.md`与`frame-driven-runtime-plan.md`，正文只保留legacy pre-Frame历史快照；旧API仅在
  `legacy-provider-port` compatibility feature中临时存在，不能用于新实现；关联旧visual artifacts也必须称为历史；
- `provider-port-application-host-open-questions.md`同样标记为historical/superseded，不得作为当前issue tracker；
  仍有价值的deferred topic由`engine.md`/当前plan解释，禁止继续宣称ApplicationHost ownership；
- `semantic-agent-view.md`保留current POM derive内容，但把Component/runtime authority链接改到`engine.md`和
  `frame-driven-runtime-plan.md`，不得再把旧boundary称为final syntax或long-term plan；
- fix只限上述三份Markdown以及Task 7 report/progress bookkeeping；不改旧历史正文、runtime、tests、examples、
  Cargo、README、Chess docs、engine、reviewer ledger或architecture artifacts；
- 验证全仓current/superseded链接关系、所有relative links、旧名称只存在于明确historical/compatibility上下文，
  并运行双配置doc tests、format、diff和121-path source freeze。修复独立签收后才可从finding处恢复Task 7剩余audit。

独立closure复核：

- `provider-port-application-host-boundary.md:3-11`现在明确由`engine.md`/frame plan supersede，正文与关联HTML/JSON
  仅为historical pre-Frame snapshot；旧ownership只属于temporary default-enabled deprecated feature，并直接给出
  native Application/ReactionPort/ordinary-root/handler路径；
- `provider-port-application-host-open-questions.md:3-7`明确不是current tracker/public contract；
  `semantic-agent-view.md:3-9`保留POM subsystem ownership，但把runtime/provider authority和implementation sequence
  分别指向engine与current frame plan；
- reviewer对三份文件从稳定marker开始与pre-fix `HEAD`做binary suffix comparison，historical body全部逐byte相同；
  只发生两个banner insertion与一个existing status block replacement；
- reviewer独立验证11/11 relative links；全仓显式`Status: current`文档只有semantic doc，且其到两份superseded
  文档的链接为0。旧docs间唯一direct link位于superseded historical snapshot内；current primary legacy名称只在
  明确compatibility/rejection/migration上下文；
- 双配置doc tests均0 failed / 6 existing ignored，format与diff通过；embedded 121-path manifest逐项匹配且digest
  仍为`a842291f39f570cd7b98bafed1a560c686edfae0994d809a37ded5bc78b13466`，fixture lockfiles为0；
- 最终文档hash为`7bbbeaab...` boundary、`f54c5e36...` open questions、`ddbbc645...` semantic。未修改runtime、
  tests、examples、Cargo、README、engine或architecture artifacts。

因此FDR-048接受resolved。Task 7可从该finding的mandatory stop位置继续剩余whole-branch audit；Phase 9仍未签收。

### FDR-049 Medium: orphan Provider abstraction artifacts把legacy execute API展示为current runtime contract

状态：**resolved；historical artifact及其Archify delivery已获独立签收。**

证据：

- tracked `docs/provider-abstraction-class.architecture.json:5-18`标题/视图为“Provider抽象与HistoryPolicy”及
  “运行时中立接口”，并宣称“Component只依赖ProviderPort”；`:42-47`把`ProviderPort · execute(projection)`
  放在中心，`:155`将region标作“运行时中立契约”，`:318-322`在“权威状态”card再次宣称ProviderPort隔离runtime；
- generated `provider-abstraction-class.html:4498-4504`携带相同title/guided view，`:4574`显示“运行时中立契约”，
  `:4613-4622`直接可见`ProviderPort · execute(projection)`，`:5069-5074`显示“权威状态”card；
- 两个artifact都没有`historical`、`superseded`、`deprecated`、`legacy-provider-port`或`ReactionPort`标记。
  JSON没有任何inbound文档引用；HTML唯一文件名引用来自JSON的`meta.output`，不是一个可提供authority classification的
  parent document；
- FDR-048所接受的`component-provider-boundary` pair不同：它们由明确superseded parent banner逐项列为historical
  visual artifacts。这个独立pair没有该classification，直接打开HTML或导出图像时会自称current contract。

影响：

该pair没有current Markdown入口，因此severity低于FDR-048，但它是repository-tracked、可直接浏览/分享/导出的
architecture artifact。读者无法从artifact自身区分历史pre-Frame design与current
`Application<P>/ReactionPort`，其“runtime-neutral/authoritative”标签会强化已deprecated的split ownership。

要求：

- 使用Archify typed JSON IR作为唯一source，先完整阅读`/home/greygoo/.agents/skills/archify/SKILL.md`；不得手改
  generated HTML；
- 保留旧diagram topology作为历史证据，但JSON metadata、guided runtime view、runtime boundary/central legacy
  port标记和authority card必须在direct HTML、SVG/export与JSON inspection中都明确`historical/superseded`；
- 明确current authority为`docs/engine.md`与`frame-driven-runtime.architecture.html`，current contract是
  `Application<P> + private FrameSession + ReactionPort`；旧`ProviderPort::execute`只能称为temporary deprecated
  compatibility snapshot；
- 用Archify compile/deliver/check重新生成HTML，运行standard/showcase validation并做desktop/mobile screenshot visual
  review，证明historical marker首屏可见、无overlap/crop，JSON/HTML source一致；
- fix只限这两个artifact以及Task 7 report/progress/plan bookkeeping，不改current frame architecture、runtime、tests、
  examples、Cargo、README、engine、reviewer ledger或其他historical docs；
- 独立签收后再从本finding继续Task 7；nonblocking-demand/runtime question和后续audit仍未完成，不能提前关闭。

独立closure复核：

- JSON metadata、guided runtime view、top SVG boundary、central `Legacy ProviderPort`/deprecated tag、两个
  `legacy impl ProviderPort` labels和authority card都直接标出historical/superseded，并给出current
  `Application<P> + private FrameSession + ReactionPort`及`docs/engine.md`/current frame diagram；
- `meta.animation`改为`none`，符合未请求motion的historical static deliverable，并使formal mobile action row完整；
  graph topology未改。reviewer从HEAD与current提取component IDs/types/positions/sizes、connection IDs/directions/routes/
  vias和boundary membership，digest逐byte相同；
- reviewer独立执行Archify doctor、standard/showcase validate与final HTML check：均9/9，0 errors/0 warnings，最小
  label-route clearance 16px；独立showcase deliver为626655 bytes、SHA-256
  `6a6ff60cf2214ea0135fe734f344feeae0410ef6b95810a804f8b72ce687c856`，与repository HTML byte-identical；
- reviewer直接重拍desktop 1440×1000 light/dark；formal CDP mobile 430×932 light/dark的toolbar宽398px、page
  horizontal overflow 0、clipped text空、header/guided/diagram overlap全false，historical title和current authority均首屏
  可见；guided deep link显示exact old-vs-current note；
- Archify原生export SVG包含historical marker 10次、两个authority path各11次、Legacy ProviderPort 4次、legacy
  impl labels 8次；4980×2940 PNG人工检查可读、无blank/overlap/crop；
- final JSON为`8bc6b8d6...`，HTML为`6a6ff60c...`。current frame architecture hashes保持
  `bf03faa2...`/`6d518977...`，121-path manifest digest保持`a842291f...`，fixture locks为0，diff check通过。

因此FDR-049接受resolved。Task 7可再次从mandatory stop恢复；nonblocking-demand/runtime question必须优先完成，
Phase 9仍未签收。

### FDR-050 Medium: accepted maximum CLI acts与Frame full-reserve budget不一致

状态：**resolved；独立closure见FDR-051后的联合sign-off。**

证据：

- `external.rs:43-46`固定External profile为32MiB Frame、16MiB Component，并允许每个external text output达到
  4MiB；`agentview.rs:45,58-90`按raw UTF-8把CLI business state保留到8MiB，`:138-158`再把完整state写入
  `external_state` POM；
- ignored regression `agentview_cli.rs:623-641`构造`4MiB - 4096`个`&`的合法`text_delta`，连续act三次后要求
  full observation；其ignore理由明确说maximum-size contract deferred；
- Task 7 writer和reviewer独立显式运行该test，分别在7.57s/7.75s得到同一RED：第三次流程返回
  `external frame-driven application failed`，test为0 passed / 1 failed；
- reviewer直接驱动同一daemon：initial observe成功；act1成功（response 12,571,481 bytes），act2成功
  （4,190,787 bytes），act3返回41-byte generic failure；随后同一daemon `observe`仍成功（16,762,184 bytes）且
  shutdown成功。因此operation contract失败但session并未永久terminalize；
- 三个acts形成约8,380,434 raw state，并同时累积shared canonical provider facts。XML/POM rendering expansion、
  canonical history和16MiB next-Full component reserve都可能参与32MiB Frame约束；现有payload-free External fault隐藏
  exact `FrameBudgetFault`，所以不能仅凭raw×escape ratio断定唯一根因。

影响：

这是authenticated local CLI correctness/DoS-boundary mismatch，不是永久session loss。每个act都满足公开per-act/wire
上限，却在driver已经接受并开始处理后才退化成generic internal failure；用户无法从当前contract预测第三次失败。
同时现有ignored test使full suite遗漏了唯一maximum-size continuity证据。扩大shared runtime budget会放大所有External
consumers的内存面，不能作为默认修复。

要求：

- 先增加test-only或temporary诊断，捕获sanitization前的exact Application stage/kind/code/reason及
  `FrameBudgetFault` variant/actual/max bytes，区分Component encoding、canonical history、Full reserve和frame overhead；
  production public fault仍必须payload-free；
- 保持External 4MiB per-act输入与16/32MiB shared profile冻结，除非实现方停止并提交独立contract证据；不得仅放大
  budget、降低测试输入、改成期待failure或吞掉Application error；
- 在CLI driver/business-state层保证每个已接受act仍有合法下一Full。若aggregate canonical history是原因，应在达到
  边界前用显式bounded business snapshot、consuming old Application shutdown和新target Full进行rotation；若exact
  meter证明只需收紧retained state，则必须对所有XML/JSON expansion给出数学上界并证明任意重复次数不会再撞aggregate
  reserve。不得依赖一个只让三次循环碰巧通过的magic cap；
- operation error、cleanup error与user panic precedence保持；rotation/recovery不得泄漏old ingress、重复act、复用target
  identity或丢失bounded business state。只能使用现有public Application/Signal/External APIs；若无法表达，停止报告API
  blocker而不是扩大framework API；
- 移除该test的ignore并保持原三个maximum escaped acts成功；补充scaled deterministic many-iteration unit proof、
  worst-case escapable characters、plain/unicode边界、over-limit preflight rejection、post-rotation Full/new-target、old-owner
  consuming cleanup和same-daemon后续observe。测试不得需要凭据/网络；
- fix范围限`src/bin/agentview.rs`、`tests/agentview_cli.rs`、必要时External skill contract文字，以及Task 7 report/progress/
  plan bookkeeping。`external.rs`与core runtime、Cargo、README、engine、examples、reviewer docs保持冻结；
- focused ignored RED→GREEN后串行运行CLI完整suite（预期21 active / 0 ignored）、双配置full/check/strict Clippy、format、
  diff、121-path source freeze与fixture-lock cleanup。独立签收后才可恢复Task 7剩余audit。

### FDR-051 Medium: fragmented legal CLI act因重复全量snapshot工作在ack前超时

状态：**resolved；final provisional-response/commit correction独立通过，Task 7与Phase 9总gate关闭。**

证据：

- FDR-050 candidate把每个admitted text event写入`ExternalCliState::push()`；该函数先clone完整state，随后
  `canonical_item_bytes()`重新render并canonicalize完整POM。canonical cap淘汰循环每pop一个旧event又重新执行一次，
  最后还会再次meter/ensure；每个Signal update同时dirty Component并触发累计snapshot reconciliation；
- reviewer用当前candidate hash `5800379c...`驱动独立foreground daemon。三组协议都只含合法`text_delta`，decoded
  text总量固定为4,000,000 bytes，小于4MiB；frame数分别为10、100、1000，小于65,536；inner protocol wire分别为
  8,000,320、8,003,200、8,032,000 bytes，小于8MiB。三组均在client固定15秒response deadline处失败，其中
  10-frame为15,437ms、100-frame为15,421ms，stderr均为`timed out waiting for agentview server`；1000-frame同样
  15秒timeout且无response；
- 相同4,000,000-byte quote payload放在一个frame时candidate成功，但已经耗时10,185ms并返回约16,000,971-byte
  response。总payload不变而仅拆分frame即越过deadline，排除了单纯最终Frame大小，证明fragment count驱动累计工作；
- reviewer用授权前exact bin hash `8bebc5a...`构建clean A/B。相同1/10/100-frame shapes分别在7,677ms、7,474ms、
  7,411ms返回原FDR-050 budget failure；因此candidate虽然把budget failure改成后台继续处理，却没有在公开CLI deadline
  内给出ack。它不是已关闭的FDR-050 GREEN；
- 所有daemon均使用独立loopback地址和随机本地token，无凭据、外部网络或Stockfish。测试后foreground daemon均被
  明确终止，工作树source与Cargo未修改。

影响：

这是authenticated local CLI correctness/availability defect。请求通过8MiB wire、4MiB text和65,536-frame全部公开
preflight，daemon取得stateful act ownership后，client却得到`NoRetry` timeout且无法判断act最终是否admitted/rotated。
Exactly-once contract禁止重试这个uncertain act；daemon在完成旧请求前也不能处理恢复observe/shutdown。允许的frame上限
比10/100-frame repro高几个数量级，因此仅把15秒改大不能给出确定上界，也会掩盖超线性实现。

要求：

- 保持FDR-050的4MiB/8MiB/16MiB/32MiB limits、rotation target/Full、bounded suffix、exactly-once、partial-admission、
  consuming shutdown和sanitized fault行为；不得缩小repro payload/frame count、改成期待timeout或仅提高timeout；
- snapshot raw/canonical accounting必须对event append/whole-event eviction呈amortized linear或更好。允许通过structured
  canonical serializer一次计算并缓存每个entry的encoded contribution、批量淘汰或对单一oversized suffix做有界搜索；
  不得在逐项pop循环中反复render/canonicalize完整snapshot，也不得手写不完整的XML/JSON escape表；
- old target在第一次admitted event后已经hidden；后续fragment不能仅为更新CLI business snapshot而重复发布相同hidden
  Component candidate。可用显式bounded snapshot owner与Signal visibility fence分离累计state，但每个admitted event的
  state commit、operation error/panic precedence、partial recovery和old-owner fencing必须保持；
- 增加active fragmented-quote integration regression，至少覆盖1000×4000或等强shape，显式断言frame/text/wire都在上限内、
  public act在现有deadline内成功、返回distinct-target Full且随后same-target Delta可用。另加不依赖wall-clock的scaled unit
  complexity proof，约束完整snapshot canonicalization/reconciliation次数不会随fragment count×snapshot size增长；
- 重跑原maximum ampersand回归至少3次、双配置CLI/full/check/strict Clippy、format/diff、121-path manifest与fixture-lock
  cleanup。writer只能追加FDR-051 response并更新Task 7 bookkeeping，不能自签FDR-050/FDR-051或恢复remaining audit。

Candidate独立review补充（hash `c70859bd...`）：

- 新`1000 x 4000`回归独立为10,490ms GREEN，原maximum ampersand为18,210ms GREEN，bin 15/15通过；混合
  quote/backslash/tab/newline/XML字符与多字节Unicode的500轮append/eviction property复核逐轮证明cached item与fresh
  structured serialization完全一致，跨多个64KiB chunk的canonical suffix也满足UTF-8/maximality；
- 但公开允许的联合上限仍是RED：65,536个空`text_delta`在8,418ms成功；把每frame改为47个quotes后，frame count仍为
  65,536，decoded text为3,080,192 bytes，inner wire为8,257,536 bytes，三者都低于既有上限，却连续两次在15,610ms与
  15,579ms得到`timed out waiting for agentview server`且无response；foreground daemon均使用独立loopback/random token并
  在测试后kill/reap；
- 该失败继续属于FDR-051，不另立finding：首轮correction已消除累计snapshot的超线性项，但每entry构造完整POM并
  canonicalize的常数项叠加65,536次后，合法stateful act仍成为不可安全重试的uncertain act。`1000 x 4000`单例不能代表
  公开frame/wire可同时接近上限；
- 下一candidate不得提高15秒timeout、降低/预拒绝65,536 frames或缩小4/8MiB limits。应把per-entry contribution降为
  serializer-backed canonical string content meter或等价的低常数线性结构，并以fresh full-item equality覆盖全部字符类；
  增加active `65_536 x 47 quotes`联合边界回归，断言exact counts、deadline内distinct-target Full/empty replay及后续Delta；
  同时保留空frame上限、`1000 x 4000`、three-act ampersand、deterministic work counters与全部ownership/failure gates。

FDR-050/FDR-051联合closure（amended hash `6e324224...`）：

- production在`agentview.rs:245`以structured serializer-derived contribution维护8MiB raw/8,388,276-byte canonical
  snapshot cap，whole-event eviction只消费cached sums；oversized entry只做64KiB reverse chunks与单一boundary search。
  `agentview.rs:546`的zero-allocation JSON string writer经过XML validation，prefix/separator/envelope仍由serializer probes
  推导；完整POM equality覆盖quote/backslash/control/XML-sensitive/multibyte字符与500个unique mixed sequences，无escape表；
- snapshot-owned `Arc<VecDeque<_>>`只在旧可见snapshot与首次admitted mutation分离时copy-on-write；business snapshot与
  one-time Signal visibility fence保持分离。replacement先mount distinct target，再安装完整bounded snapshot、准备empty-replay
  Full、consuming shutdown旧owner；partial operation failure保留recovery Full，stale Signal、cleanup error与原panic payload
  precedence由binary 16/16及64 rotations证明；
- reviewer在stable source上独立通过joint-limit active regression：65,536 frames、3,080,192 decoded bytes、8,257,536
  protocol bytes的act为12,802ms；all-features/no-default完整CLI中的同一act分别13,037/12,808ms，两套均23/23。disposable
  consecutive-state proof在同一daemon连续执行两次joint act，已有65,536-entry snapshot后的第二次仍为13,768ms（首次
  13,105ms），两次均distinct-target Full/null base/empty replay并接same-target Delta；
- reviewer另通过`1000 x 4000` quote 8.86s、原three-act maximum ampersand 15.68s及binary 16/16。writer final三轮
  joint为13,729/13,456/12,790ms，fragmented为9.19/8.96/8.94s，maximum为16.07/15.80/15.86s；15秒client
  timeout、65,536-frame、4/8/16/32MiB limits均未改变；
- current 121-path manifest独立验证121/121，digest `dcb293a5...`；core External、Cargo、skill、Chess、FDR-048/049和
  current architecture hashes保持冻结，fixture locks为0，format/diff clean。实现只在授权CLI/test/bookkeeping范围。

因此FDR-050和FDR-051接受resolved，mandatory stop解除。该closure不签收Task 7或Phase 9；remaining
Chess/repository-security/release/docs/Archify audit现在恢复。

### FDR-052 Medium: root release manifest不能package且包含内部review-control artifacts

状态：**resolved；plain publish-order residual保留，独立closure见本节末。**

证据：

- `Cargo.toml:17`声明`agentview-derive = { path = "agentview-derive" }`但没有version requirement；root package自身为
  `0.1.0`，derive package也为`0.1.0`；
- reviewer在stable 121-path source上运行`cargo package --allow-dirty --no-verify`，Cargo在构建archive前退出101：
  `all dependencies must have a version requirement specified when packaging`，并明确指出`agentview-derive`打包时会移除
  `path`。因此当前root crate无法形成release package；
- 对照`cargo package -p agentview-derive --allow-dirty --no-verify`成功，产生8-file package；failure局限于root
  dependency metadata，不是derive源码或registry/network行为；
- `cargo package --list --allow-dirty`虽能列清单，但当前376 paths包含`.claude/handoffs/...`、`CHATROOM.md`和
  `docs/frame-driven-runtime-review.md`。这些是agent/reviewer控制面，不是crate用户文档；secret signature扫描无命中，
  ignored root `.env`为0600且不在package中，所以当前没有credential exposure，但artifact边界不正确；
- Cargo另warning缺少documentation/homepage/repository。计划没有提供权威URL，reviewer不把该warning并入blocking
  correction，也不允许实现方猜测metadata。

影响：

这是release packaging defect，不影响已运行runtime correctness。当前public crate无法通过Cargo的最低package manifest
校验；即使只补version，默认file discovery仍会把内部handoff/reviewer状态发布给下游。Phase 9明确建立public owner/API与
one-minor compatibility release边界，Task 7也保留feature/dependency/release-metadata audit，因此不能在已知root package
失败时签收总gate。

要求：

- 仅在root `Cargo.toml`给`agentview-derive` path dependency增加与workspace crate一致的`version = "0.1.0"`；不得升级
  dependency、改变features/defaults、runtime代码或lock resolution；
- 用root package `exclude`（或等价Cargo-supported manifest边界）排除`/.claude/**`、`/CHATROOM.md`和
  `/docs/frame-driven-runtime-review.md`。保留README引用的`docs/engine.md`、`docs/frame-driven-runtime-plan.md`、Chess
  docs、architecture artifacts、examples和正常tests；不得以大范围include遗漏当前公开文档；
- 不添加虚构repository/homepage/documentation URL；warning可作为后续真实release metadata residual；
- active GREEN必须包括derive package、root `cargo package --allow-dirty --no-verify`、root package list exact exclusion与
  required public docs presence、从`.crate`读取normalized manifest证明path被替换为`version = "0.1.0"`；不得publish；
- 串行重跑`cargo metadata`、双feature check/strict Clippy、双feature full tests、docs/rustdoc、README/offline commands、
  format/diff和fixture-lock cleanup。121-path manifest只允许`Cargo.toml`与已接受`src/bin/agentview.rs`两行变化；Cargo.lock、
  CLI hashes、core、Chess、FDR-048/049和architecture保持冻结。实现方不能自签FDR-052、Task 7或Phase 9。

FDR-052独立closure（Cargo hash `ca0c5245...`）：

- root manifest只增加matching `agentview-derive` version与三条package excludes；reviewer独立重建derive 8-file package和
  command-local patched root 373-file package。normalized root dependency为`version = "0.1.0"`且无path；RED/green list
  恰好只移除`.claude` handoff、`CHATROOM.md`与review ledger，全部要求的public docs/source/examples/tests仍存在；
- plain post-fix package已越过missing-version RED，只因derive尚未按正确publish order进入registry而停止。没有publish、
  repository Cargo config或虚构URL；Cargo.lock与features/targets/dependency resolution不变；
- writer双full为478/433 library、16/16 binary、23/23 CLI，双check/rustdoc/strict Clippy/examples 38/38、Chess 11/11、
  九个offline commands、1374-byte README harness、format/diff均通过。reviewer package/list/normalized-manifest复核通过；
  current 121-path两行manifest digest为`1a6fe7b3...`且119/119 frozen paths匹配。

因此FDR-052接受resolved；missing repository/homepage/documentation URL与derive-before-root publish order仅作为真实release
流程residual，不阻塞当前gate。

FDR-051 post-closure reopen（相同source hash `6e324224...`）：

- reviewer在FDR-052 stable manifest上执行final `cargo test --all-features --no-fail-fast`，CLI target为21/23：
  `fragmented_maximum_quote...`与`joint_frame_and_wire_limit...`均返回`Connection reset by peer`；其余target继续通过；
- 清理仅三份fixture locks后立即重跑完整all-features CLI，再次21/23且同两test reset；joint输出在1,794ms前断开，
  排除15秒assert本身造成failure；
- 随后exact joint单例运行到15,683ms并返回`timed out waiting for agentview server`。因此同一合法65,536-frame、
  3,080,192-byte decoded、8,257,536-byte wire stateful act仍可成为NoRetry uncertain act；此前12.8-13.7s GREEN没有
  提供足够end-to-end运行余量；
- 复现时32 CPUs、34GiB available、load average低于1，无OOM/thermal日志、cargo/rustc竞争、daemon残留或binary/source
  hash漂移。connection reset形状还要求分别计量1秒authenticated request ingress、operation/Full准备、1秒response write及
  15秒client response deadline，而不能只重复总wall time。

该证据撤销FDR-051 closure但不撤销FDR-050 budget/rotation closure。下一candidate不得提高1秒request/write或15秒client
response timeout、降低frame/text/wire limits、减小active shape、仅改test串行度或依赖release build。应在disposable
foreground daemon分别冻结ingress/dispatch/snapshot/Full/shutdown/encode/write/client parse阶段计数/耗时，优化CLI-owned
end-to-end路径并保留exact accounting、ownership、failure/panic及FDR-052 freeze；final acceptance至少要求focused重复、
complete CLI双模式和双full suites在stable bytes上全部通过。

FDR-051 final closure（source `e97f5abe...`，CLI tests `c376b4c9...`）：

- `agentview.rs:810`至`:1260`使用fixed bounded `OnceLock` slots、atomics和`Notify`建立streaming prepared verifier；worker
  通过同一serializer-backed meter/eviction algebra构造candidate，真实handler仍逐event dispatch、验证kind/text/order并执行
  原one-time Signal visibility fence。success只在published/accepted/event count exact一致时采用candidate；mismatch、partial、
  malformed、disconnect、limit或operation error按已接受prefix恢复，panic保持原payload并eager取消speculation；
- `agentview.rs:1402`、`:1418`、`:1747`及`:3246`引入private typed provisional envelope和随机32-byte ticket。该framing
  只允许`Act`且只接受结构化验证的`Observation { mode: "full" }`；client完整验证并buffer public JSON但不写stdout。server仅在
  exact candidate、operation/error precedence、replacement adoption及old-owner consuming shutdown全部成功后发送小型Commit；
  其他路径发送matching Replace后走原ordinary bounded response。missing/malformed/wrong ticket、partial body、panic、timeout或
  response loss均NoRetry且provisional bytes保持不可见；Replace在读取最终body前显式释放candidate buffer；
- request仍以structured header/raw payload/footer传输，header、payload和footer共享一个absolute 1秒ingress deadline；HMAC
  继续stream原`DaemonRequest` serde bytes，challenge/proof labels及MAC input逐字节不变。每次server write保持1秒bound，client
  总deadline保持15秒；65,536 frames、4MiB text、8MiB wire/state、8,388,276-byte snapshot及16/32MiB External budgets均未改变；
- deterministic proofs覆盖legacy MAC、strict response kind/canonical metadata、public byte pass-through、large Error、Commit/Replace、
  provisional仅Act/Full、wrong ticket、missing disposition、partial/over-limit body、operation与cleanup precedence、原panic payload、
  one Signal fence、unique serializer equality、transactional rejection、prefix recovery、wrong token/server proof、stalled ingress、
  slow reader及response-loss non-replay。最后两个strict-gate hygiene corrections仅把test helper标为`#[cfg(test)]`并采用等价auto-deref；
- final source至少五次执行三条active maximum回归。最后三轮exact joint为10,313/10,647/10,664ms，fragmented harness为
  7.14/7.57/7.22s，three-act repeated harness为14.63/14.11/13.81s；随后完整all-features/no-default CLI的joint分别为
  11,034/10,175ms。最差单request仍有3,966ms deadline margin。双模式complete CLI在final bytes各通过3次，均26/26；
- final双配置full suites为library 478/433、binary 22/22、CLI 26/26，全部integration/trybuild/doc targets green；examples
  双配置38/38，Chess双配置11/11。dual all-target check、doctest、`-D warnings` rustdoc、strict Clippy、format和diff均通过；
- FDR-052复核产生derive 8-file与command-local patched root 373-file offline packages；normalized derive dependency只有
  `version = "0.1.0"`。九条credential-free commands和README downstream harness通过；authority links 25/25、current-link与
  secret scans、frozen Archify artifacts均通过；119/119未授权manifest paths匹配，新121-path digest为`a87bca2f...`；
  fixture locks、repository Cargo config和active gate/daemon processes均为0。core External、Cargo、skill、README、Engine、Chess、
  FDR-048/049 docs/artifacts及current architecture hashes保持冻结。

因此FDR-051最终接受resolved。该closure同时完成Task 7；未发现新的Critical/High/Medium blocker，Phase 9总gate可以关闭。

### Reviewer gate note (historical snapshot before FDR-022 implementation)

本段保留reviewer提出FDR-022时的原始gate判断，不再表示当前实现状态；当前状态以第4节gate、第7节ledger
和第8节implementation response为准。

FDR-017、FDR-018和FDR-020的closure已由独立复核接受；FDR-021在v1
`CompleteTranscript`边界内typed fail closed，future provenance extension保持deferred。此前
FDR-017至FDR-021的原始blocking note已被这些response取代。

但是FDR-019 closure复核发现新的FDR-022同值碰撞。它发生在v1 append-compatible replay和正常remount，
不属于FDR-021 deferred extension。因此当前Phase 0、3、5可关闭，Phase 4 v1 gate仍受FDR-022阻塞。
第7节ledger由主实现agent维护，reviewer不在本次复核中直接改写其status。

## 4. Gate 判断

### Phase 0

原 review 要求重新打开的四个 protocol 点均已闭合：profile race fence、structured fault
taxonomy、coherent declaration constructor，以及 declare-time recovery capability / submit-time
exact proof 的边界。FDR-017 已把完整 Frame/fault/Full rebase shape同步到两份权威文档；FDR-018
把observed epoch与committed epoch拆成两个不可回滚水位。Phase 0 protocol gate 可以关闭。

### Phase 1

ownership skeleton、move-only Frame、borrowed stream、same-poll handoff/commit contract 和完整 poll
transition matrix 已实现。Phase 1 internal protocol gate 可以关闭；`Application<P>` 仍按计划保持
crate-private，public export 留到 Phase 9，不在这里提前承诺稳定性。

### Phase 2

dirty projection retention、projection revision、bootstrap declare/reconcile、legacy prepare/dispatch split
和 post-reconcile 均有测试。Phase 2 gate 可以关闭。

### Phase 3

terminal grammar、history-before-event、ToolOutput ordinal staging、payload-free faults 和 near-linear
streaming admission 均已覆盖。FDR-020 已删除 Drop 中的fallible materialization：canonical transaction
buffer在event release前直接更新，normal finish与Drop只做不可失败归还。Phase 3 gate 可以关闭。

### Phase 4

exact JCS meter、hypothetical next-Full admission、ToolOutput retryability和 `#[diff]` 独立baseline已实现并
通过focused tests。FDR-019的execution-scope reset会清空authored ledger，FDR-021的non-append replay在
v1明确fail closed。FDR-022新增持久cross-scope ambiguity multiset：旧scope provider outputs与scope切换
前未reconcile tail不再按value claim；普通item碰撞会在Frame prepare阶段typed fail closed，而`#[diff]`
append item保持直接提交。Phase 4 v1 gate可以关闭。

### Phase 5

private `Application::react()` 已接通 declare、reconcile、prepare、same-poll commit、fact/lane pump 和
post-reconcile。FDR-010最初由terminal cancellation guard闭合；该mitigation现已被admission-time exact
fallback reserve和reusable cancellation取代。handoff前后取消都保持Application ready；handoff后保留已发布
canonical tail，并为unresolved admitted ToolCall物化预留的unknown-outcome ToolOutput。FDR-020继续保证
Drop恢复不会静默丢失已发布tail。Phase 5 gate 可以关闭。
Phase 6不由Phase 5自动关闭；其当前进展与剩余target见下一节。

### Phase 6

Responses native `ReactionPort` 已完成 exact Frame lowering、same-poll handoff、ordered fact stream、
private compaction recovery、sticky terminal fault、mode fence和sanitized fault mapping；FDR-012至FDR-016
以及FDR-023至FDR-027均有direct regression。Chat Completions只保留request wire baseline，直接实现
Full/Delta lowering、single-text fact grammar、same-poll handoff、sticky fault和mode fence；Debug只capture
exact Frame，具有有限profile、domain-separated identity和mode fence。FDR-028至FDR-032均已闭合。
Phase 6 built-in target gate可以关闭；legacy adapter没有被用来证明native correctness。

### Phase 8

Phase 8 gate关闭。独立review接受FDR-035 production teardown、FDR-036 bounded retirement state、FDR-037
runtime lifecycle、FDR-038 immediate panic unwind、FDR-039/FDR-040 shutdown migration与FDR-041 External panic
precedence。完整Component demand、hook topology、provider handler、`spawn`/`use_future`/bounded coroutine、mount
fence、retirement cancellation、panic transparency和consuming shutdown surface均已核对，未发现新增blocker。

### Phase 8 reviewer sign-off: accepted

reviewer focused gates：public async authoring 4/4、provider bindings 24/24、signal transaction/fence 21/21、
async task context 5/5、task supervisor 14/14、External 30/30、Application完整52/52；FDR-036 distinct-ID与FDR-037
runtime-closure各独立stress 50/50，FDR-041 External stress 100/100。最终独立全量
`cargo test --all-features --no-fail-fast`为library 475/475、CLI 20 passed / 1 existing ignored，其余integration、
trybuild和doc suites全部通过；all-target check、strict Clippy、format和diff check通过。

### Phase 9 public core, compatibility and executable workflow slices

partial gate关闭：public `Application<P>` lifecycle/fault/snapshot、public Component-demand consumption、curated
Frame/ReactionPort exports、private port/session fence、default legacy/no-default native boundary、native
`signal_reaction`/Responses provider acceptance、public External typed/empty completion、Agent/Skill/Plugin真实public
executables、保留explicit orchestration的native Chess，以及README/public documentation均已独立签收。FDR-042至
FDR-047均resolved，Task 1至Task 6关闭。

### Phase 9 reviewer sign-off: accepted

Task 7 whole-branch correctness/security review、FDR-048至FDR-052 corrections及全部final repository gates已完成。
public Application/Frame/ReactionPort owner boundary、legacy compatibility isolation、native executable workflows、Chess、CLI
bounded authenticated framing、panic/cleanup precedence、documentation authority、architecture artifacts和release package boundary
均与`docs/engine.md`一致。FDR-051 final source在不改变任何deadline/limit/core API的前提下保留4/8/16/32MiB budgets、
65,536-frame cap、exact HMAC/public JSON、NoRetry与consuming shutdown，并给joint legal maximum保留至少3,966ms实测余量。
Phase 9总gate关闭。

## 5. Verification

当前稳定快照执行结果：

- `cargo test --all-features --no-fail-fast`：final source独立门禁全部通过；library 478/478、binary 22/22、
  CLI 26/26，其他integration/trybuild targets无失败，doc tests有6项既有ignored；
- `cargo check --workspace --all-targets --all-features`与no-default配置：均warning-free通过；
- `cargo fmt --all -- --check`：通过；
- `git diff --check`：通过；
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`与
  `cargo clippy --workspace --all-targets --no-default-features -- -D warnings`：均通过、无waiver；
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`在两种feature配置下均通过；
- `cargo test --no-default-features --no-fail-fast`：final source完整suite通过；library 433/433、binary 22/22、
  CLI 26/26，其余enabled integration/trybuild targets无失败，doc tests保持6项既有ignored；
- Phase 9 feature matrix：4/4通过，覆盖native no-default、十个explicit-feature probes、十个default-wiring probes与
  constructor E0599 mutation guard；
- Phase 9 Task 3：`signal_reaction` 8/8并独立stress 50/50，provider acceptance 4/4并独立stress 30/30；两个
  no-default binaries均通过，分别证明Full→Delta、fact handler state publication、fresh Full target、wire history、
  old/fresh/error task destructor和server join；
- FDR-044 compile topology：default/all-features为16 pass + 43 fail fixtures，no-default为13 pass + 34 fail；两种
  harness均2/2，完整no-default suite从exit 101变为全绿；
- Phase 9 Task 4/FDR-045：External 33/33、internal roles 3/3、downstream public completion 1/1、Agent 2/2、
  Skill 2/2、Plugin 5/5及三个no-default binaries通过；completion/Skill/Plugin/Agent stress分别为40/40、40/40、
  40/40、30/30。三条public path匹配同一exact first-Frame golden；Plugin partial/final cleanup与operation-panic
  precedence通过。九文件frozen hash set复核一致，最终三个feature-matrix fixture lockfiles已清理；
- Phase 9 Task 5/FDR-046：default/no-default Chess各11/11，same-Application pre-handoff no-mutation retry exact
  1/1并独立stress 30/30；offline help、合法white→UCI black→resign continuity、retry exhaustion、provider rejection、
  timeout、setup-after-mount和双panic cleanup均通过。完整双配置suite、all-target checks、strict Clippy、format、diff和
  fixture lock cleanup全部独立通过；
- Phase 9 Task 6/FDR-047：完整README snippet在独立no-default downstream crate中compile/run成功；六个offline
  examples、Chess 11/11 + check/help、九项serial docs/examples/rustdoc/strict Clippy/format/diff gates与14/14 links
  全部通过。candidate reconstruction、changed/unchanged hashes和无fixture lockfile均独立复核；
- Phase 9 Task 7 pre-FDR-048 handoff：all-features library 478/478、no-default 433/433，binary unit 9/9与CLI
  20 passed / 1 existing ignored双配置一致；example matrix双配置各38/38，trybuild topology为16+43与13+34，
  feature matrix 4/4；双配置full/doc/all-target/rustdoc/strict Clippy、format、diff与全部README commands通过；
  focused Application 52/52、Responses 17/17和15/15、External 33/33、panic 17/17、cancellation 11/11、shutdown
  8/8、FDR-046 1/1。三个fixture lockfiles已清理，121-path manifest digest `a842291f...`保持；
- FDR-048：三个historical/current authority blocks获得独立binary-suffix、11/11 links、current-link scan、双配置
  doctest、format/diff与121-path source-freeze验证，finding关闭；
- Task 7 resumed audit/FDR-049 RED：orphan `provider-abstraction-class` JSON/HTML无status marker并自称
  ProviderPort execute为runtime-neutral/authoritative contract；artifact hashes为JSON `51a92735...`、HTML
  `146f7ba7...`；
- FDR-049 GREEN：Archify standard/showcase/final checks 9/9、0 warnings，JSON→HTML deterministic delivery、
  unchanged topology、desktop/formal-mobile dual-theme direct/guided/native-export visual checks、current-frame freeze、
  121-path manifest与0 fixture locks全部独立通过；finding关闭；
- Task 7 runtime-demand question：fresh no-default bidirectional current-thread/multi-thread/no-runtime matrix无finding，public
  demand 3/3、runtime closure 1/1、nonblocking panic 1/1通过；
- FDR-050 RED：ignored maximum escaped act test由writer/reviewer独立复现0/1 failure；direct daemon sequence证明前两次
  act成功、第三次generic Application failure、随后observe/shutdown仍成功。source/corrected artifacts保持冻结；
- FDR-050 candidate：exact private diagnosis为`FullReserveTooLarge { required_full_bytes: 37_728_948,
  max_frame_bytes: 33_554_432 }`；one-act-per-target rotation、8,388,276-byte structured snapshot cap、active maximum
  regression、64-rotation ownership proof和双配置full gates均由writer通过，但尚未获得reviewer签收；
- FDR-051 RED与首轮candidate复核：原candidate对固定4,000,000-byte quote text的10/100/1000-frame协议timeout；
  首轮correction使`1000 x 4000` GREEN，但合法`65,536 x 47 quotes`联合边界连续15,610/15,579ms timeout；
- FDR-050/FDR-051 closure：stable amended candidate使用serializer-backed content meter、cached eviction与one-time Signal
  fence；reviewer joint-limit为12,802ms，双模式完整CLI同一act为13,037/12,808ms且各23/23，连续已有65,536-entry
  snapshot的第二act为13,768ms；`1000 x 4000`为8.86s、three-act maximum为15.68s、binary 16/16，limits/timeout/core
  hashes未改变，121-path manifest `dcb293a5...`匹配；两finding关闭并恢复remaining audit；
- FDR-052 GREEN：matching derive version与三条package excludes通过reviewer derive/root archive、373-path list、
  normalized version-only manifest和public-content复核；writer双full/check/rustdoc/strict Clippy/examples/offline gates通过，
  121-path两行digest `1a6fe7b3...`、119/119 frozen；finding关闭；
- FDR-051 final closure：source `e97f5abe...`、CLI tests `c376b4c9...`；typed private provisional Full只在matching
  ticketed Commit后公开，Replace/error/loss路径不泄漏candidate。final exact joint五轮以上全部通过，最后独立CLI样本为
  11,034/10,175ms，最差仍有3,966ms margin；完整CLI双模式各三次26/26，limits/timeouts/core不变；
- final release/authority freeze：derive/root package为8/373 files，normalized dependency无path；offline commands与README
  harness通过；relative links 25/25，authority/secret/architecture scans通过；121-path digest `a87bca2f...`且119/119 frozen，
  fixture locks、Cargo configs和active processes均为0；
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`在all-features与no-default两种配置下均通过；
- FDR-018至FDR-022 focused regression tests全部通过，包括higher epoch在pre-handoff rejection后保持、
  scope reset、ledger corruption后的infallible Drop、non-append replay，以及cross-scope provenance
  fail-closed classification；
- Responses native focused gates：`native_reaction::tests` 17/17、`frame_request::tests` 15/15、
  `reaction_fault::tests` 3/3；覆盖same-poll handoff、Full/Delta exact lowering、count/digest和System-bound
  compaction proof、output-index barrier、ToolOutput exactly-once、optional primary、sticky terminal、
  legacy/native mode fence以及accepted-state declaration consistency；
- Chat focused gates：`chat_completions::` 25/25，legacy integration 27/27；覆盖exact Full/Delta request、
  request-only baseline、same-poll handoff、empty/nonempty terminal grammar、retryable/sticky fault、body limit、
  profile、accepted-state validation和legacy/native mode fence；
- Debug focused gates：7/7，public integration 2/2；覆盖Full/Delta capture、unpolled/first-poll transition、
  continuity/profile precondition、两种mode顺序、有限profile和typed identity exhaustion；
- Phase 7 focused gates：External 18/18、driver demand 6/6、Agent/Skill/Plugin integration 3/3、Application
  demand state-ordering 1/1、CLI 20 active tests；覆盖same-poll queue handoff、claimed-act cancellation、
  reserve/control-Drop barrier、stale ingress、Full recovery、sticky demand、Skill frontend separation、Plugin
  multi-parent ownership和三路canonical Frame golden；
- reviewer的`base -> provider collision -> remount authored collision`序列已进入repository regression，
  现在返回typed `AmbiguousProjectionProvenance`且不推进revision/cursor；direct remount前未checkpoint的
  provider tail也有独立回归；
- Archify 2.12 standard 与 showcase validation：均通过，0 errors / 0 warnings；最小
  label-route clearance为4.7px；
- Archify 2.12 `deliver`与独立`check`均通过9/9 checks；最终HTML为620398 bytes，SHA-256
  `6d518977324c38d0cd098c543e12eec3dbaf756c3f6e3009220d898180591d60`；
- 最终HTML的light/dark完整页面和canonical PNG均已检查，无文字裁切、节点重叠、误导性route或主题
  可读性问题；`validation: passed`，`visual_review: passed`，`correction_rounds: 2`。

## 6. 后续顺序

1. Phase 9 Tasks 1至Task 7已完成，当前无需继续Frame-driven runtime release-blocking实现；
2. FDR-021仅在未来引入occurrence-provenance history replacement时恢复设计；v1继续typed fail closed；
3. 真实发布时先publish `agentview-derive 0.1.0`再publish root crate，并补充真实repository/homepage/documentation URL；
4. default-enabled legacy compatibility surface按承诺保留一个minor release后单独移除，不回写本次Phase 9 contract。

## 7. Implementation ledger

本节是实现与 review 之间的固定交接面。reviewer 可以在第 3 节继续追加稳定编号的 finding；
只有主实现 agent 更新本节状态，subagent 不直接并发编辑本文档。

状态只使用：`open`、`in progress`、`resolved`、`deferred`。`resolved` 必须附源码与测试锚点；
不能以讨论结论或代码已起草代替验证。

| Finding / work item | Status | Resolution evidence |
|---|---|---|
| FDR-001 profile handoff precondition | resolved | `reaction.rs:350`, `reaction.rs:429`, `application.rs:1631` |
| FDR-002 structured, payload-free faults | resolved | `reaction.rs:608`, `admission.rs:973`, `application.rs:558`; sentinel tests at `admission.rs:1973`, `application.rs:1376` |
| FDR-003 coherent resume declaration | resolved | `reaction.rs:175`; pre-render rejection test at `application.rs:1252` |
| FDR-004 streaming admission complexity | resolved | owned incremental transaction at `admission.rs:387`, `admission.rs:444`; linear-work test at `admission.rs:2182` |
| FDR-005 ToolCatalog canonical grammar | resolved | UTF-16 sort/validation at `reaction.rs:258`; tests at `reaction.rs:799`, `frame.rs:934` |
| FDR-006 recovery proof boundary | resolved | protocol boundary at `engine.md:254`, `frame-driven-runtime-plan.md:136`; built-in conformance remains Phase 6 |
| FDR-007 architecture delivery | resolved | core directed relationships at architecture JSON `:128`, `:204`, `:233`, `:254`, `:265`; Archify 2.12 generator at HTML `:6`; showcase delivery and visual receipt in FDR-007 response |
| FDR-008 conformance matrix / plan status | resolved | poll matrix at `application.rs:1454` through `application.rs:1656`; plan status at `frame-driven-runtime-plan.md:5` |
| Phase 4 next-Full budget closure | resolved | exact reserve at `frame.rs:112`, incremental tracker at `frame.rs:531`; equivalence test at `frame.rs:1016`, admission tests at `admission.rs:2062` through `admission.rs:2160` |
| Phase 4 projection reconciliation ledger | resolved | diff/reconciliation split at `projection_diff.rs:17`, `projection_diff.rs:32`; ordered claims and ambiguity fence at `projection_diff.rs:141`; tests at `projection_diff.rs:966` through `projection_diff.rs:1046` |
| FDR-009 Full replay/projection duplicate | resolved | compiler at `frame.rs:241`; Full reset test at `frame.rs:1223`; contract at `engine.md:403` |
| FDR-010 post-handoff reaction cancellation | resolved; terminal mitigation superseded | exact fallback construction/materialization at `admission.rs:32`, `admission.rs:531`, `admission.rs:629`; atomic reserve accounting at `frame.rs:670`, `frame.rs:699`; monitor-aware recovery and Drop order at `application.rs:165`, `application.rs:189`, `application.rs:734`; reuse/Delta regressions at `application.rs:2977`, `application.rs:3061`; contract at `engine.md:527`, `engine.md:879` |
| FDR-011 nested ToolOutput classification | resolved | exact delegation at `application.rs:737`; classification test at `application.rs:1407` |
| Phase 5 private structured pipeline | resolved | `Application::react()` at `application.rs:187`; concurrent fact/lane pump at `application.rs:316`; integration tests at `application.rs:1700` through `application.rs:1954` |
| FDR-012 Responses private compaction coverage | resolved | versioned canonical count/digest and System binding at `frame_request.rs:27`, `frame_request.rs:40`, `frame_request.rs:154`, `frame_request.rs:289`; recovery tests at `frame_request.rs:797`, `frame_request.rs:824`, `frame_request.rs:876`, `frame_request.rs:934` |
| FDR-013 Responses ProviderFact output ordering | resolved | reaction-local barrier and ordered commit at `native_reaction.rs:495`, `native_reaction.rs:733`; private-before-public test at `native_reaction.rs:1773` |
| FDR-014 System instruction replacement semantics | resolved | normalized snapshot compiler/checkpoint at `frame.rs:229`, `frame.rs:285`, `frame.rs:333`; ordinary reconciliation exclusion at `projection_diff.rs:178`; contract at `engine.md:288`, plan `:495`; regression matrix starts at `frame.rs:1182` |
| FDR-015 production FrameProfile source | resolved | defaults/config at `async_openai.rs:82`, `async_openai.rs:223`; mount-stable target at `async_openai.rs:327`; profile tests start at `async_openai.rs:2052` |
| FDR-016 sanitized OpenAI fault mapping | resolved | closed mapper/observability at `reaction_fault.rs:13`, `reaction_fault.rs:93`, `reaction_fault.rs:188`; mapping and sentinel tests at `reaction_fault.rs:214`, `reaction_fault.rs:333`, `reaction_fault.rs:377` |
| FDR-017 authoritative Phase 0 contract consistency | resolved | authoritative shapes at `engine.md:159`, `engine.md:296`, plan `:251`, `:311`, `:345`; Full rebase contract at `engine.md:309`, plan `:445`; foreign namespace test at `reaction.rs:871` |
| FDR-018 observed TargetEpoch monotonicity | resolved | separate observed watermark at `frame.rs:173`, validation/advance at `frame.rs:360`, `frame.rs:391`; refresh at `application.rs:264`; regression test at `application.rs:1499` |
| FDR-019 projection execution-scope fence | resolved | compatible checkpoint fence at `frame.rs:230`; authored reset at `projection_diff.rs:220`; non-collision remount test at `frame.rs:1348`; cross-scope provider half completed by FDR-022 |
| FDR-020 infallible admission abort materialization | resolved | validated item transfer at `transcript.rs:380`; owned transaction at `admission.rs:387`; direct text mutation at `admission.rs:603`; infallible Drop at `admission.rs:960`; injection test at `admission.rs:1769` |
| FDR-021 replay replacement provenance | deferred | v1 rejects non-append replay before reconciliation at `frame.rs:226`; typed fault/mapping at `frame.rs:701`, `application.rs:643`; tests at `frame.rs:1408`, `application.rs:1432`; future provenance grammar at `engine.md:362`, plan `:160` |
| FDR-022 cross-scope provider/authored value collision | resolved | persistent ambiguity state/reset/claim order at `projection_diff.rs:32`, `projection_diff.rs:181`, `projection_diff.rs:227`; scope-tail fence at `frame.rs:324`; typed mapping at `application.rs:608`; regressions at `frame.rs:1587`, `frame.rs:1631`, `frame.rs:1661`, `projection_diff.rs:979`, `projection_diff.rs:1016`, `projection_diff.rs:1043` |
| FDR-023 Responses terminal target state | resolved | sticky target fault at `async_openai.rs:327`, `async_openai.rs:381`; post-handoff guards at `native_reaction.rs:274`, `native_reaction.rs:495`; tests at `native_reaction.rs:1386`, `native_reaction.rs:1442`, `native_reaction.rs:1466` |
| FDR-024 Responses completion without primary text | resolved | optional-primary validation at `output.rs:706`; native terminal adapter at `native_reaction.rs:824`; commentary/tool/reasoning tests at `native_reaction.rs:1643`, `native_reaction.rs:1674`, `native_reaction.rs:1704` |
| FDR-025 legacy/native mode fence | resolved | mode state/checks at `async_openai.rs:320`, `async_openai.rs:526`, `async_openai.rs:533`; crossing claims at `async_openai.rs:599`, `native_reaction.rs:102`; tests at `native_reaction.rs:1503`, `native_reaction.rs:1517` |
| FDR-026 compaction/System snapshot binding | resolved | versioned proof at `frame_request.rs:27`; Full verification at `frame_request.rs:154`; seal binding at `frame_request.rs:289`; regression at `frame_request.rs:876` |
| FDR-027 accepted declaration/private state consistency | resolved | declare/crossing validation at `native_reaction.rs:230`, `native_reaction.rs:246`, `native_reaction.rs:89`; regression at `native_reaction.rs:1490` |
| FDR-028 Chat output replay ownership | resolved | request-only baseline at `chat_completions/frame_request.rs:19`; exact two-turn body regression at `chat_completions/native_reaction.rs:850` |
| FDR-029 Chat empty completion grammar | resolved | empty terminal branch at `chat_completions/native_reaction.rs:411`; regression at `:828` |
| FDR-030 Debug legacy/native mode fence | resolved | crossing claims at `debug.rs:148`, `debug.rs:168`; unpolled/precondition/both-order regressions at `debug.rs:344`, `debug.rs:383`, `debug.rs:418` |
| FDR-031 Debug identity exhaustion/domain | resolved | fallible domain-separated allocator at `debug.rs:22`, `debug.rs:26`; regressions at `debug.rs:302`, `debug.rs:329` |
| FDR-032 Debug finite FrameProfile | resolved | finite defaults at `debug.rs:23`, `debug.rs:38`; stable profile regression at `debug.rs:302` |
| Phase 7 native External Frame exchange | resolved | exact observation/port at `external.rs:78`, `external.rs:494`; crossing send at `external.rs:547`; poll/cancellation/failure tests at `external/tests.rs:149`, `external/tests.rs:176`, `external/tests.rs:210` |
| Phase 7 External ingress and continuity recovery | resolved | ingress claim/reset at `external.rs:346`, `external.rs:564`; late/full/fault recovery tests at `external/tests.rs:392`, `external/tests.rs:414`, `external/tests.rs:429` |
| Phase 7 mount-fenced driver demand | resolved | handle/receiver at `driver_demand.rs:26`, `driver_demand.rs:51`; race/fence tests at `driver_demand.rs:140` through `driver_demand.rs:208`; state-ordering integration at `application.rs:2085` |
| Phase 7 Skill frontend separation | resolved | thin role at `integration.rs:21`; latest/subcommand/no-submit test at `integration/tests.rs:131` |
| Phase 7 Plugin multi-parent session proof | resolved | thin role at `integration.rs:71`; independent registry/session and stale-message test at `integration/tests.rs:180` |
| Phase 7 Agent/Skill/Plugin canonical golden | resolved | shared compiler golden at `integration/tests.rs:80` |
| FDR-033 External post-handoff disconnect stream fault | resolved | control liveness/Drop at `external.rs:176`, `external.rs:211`; post-reserve reject at `external.rs:521`; nonterminal EOF fault at `external.rs:634`; cancellation/barrier regressions at `external/tests.rs:224`, `external/tests.rs:259` |
| FDR-034 permanent External control loss | resolved | first-wins sticky terminal at `external.rs:214`; terminal reserve/liveness rejection at `external.rs:507`; mount/completed/claimed/reserve regressions at `external/tests.rs:228`, `external/tests.rs:247`, `external/tests.rs:306`, `external/tests.rs:335` |
| FDR-035 production Application teardown owner | resolved | call-time detached cleanup at `external.rs:924`, active reaction join at `external.rs:1003`, CLI ACK boundary at `src/bin/agentview.rs:799`; reviewer shutdown 7/7, detached cleanup 30/30 and CLI ACK 30/30 accepted |
| FDR-036 bounded task-supervisor retirement state | resolved | transient claims at `task.rs:53`, core/actor release at `task.rs:519`, `task.rs:1018`; same-ID/distinct-ID/pending-claim regressions at `task.rs:1204`, `task.rs:1241`, `task.rs:1352`; stale context at `async_task.rs:414`; reviewer 14/14 + 5/5 + stress 50/50 accepted |
| FDR-037 task-supervisor runtime lifecycle | resolved | actor RAII/affinity at `task.rs:819`, shared Closed-aware arbiter at `application.rs:93`; Application A-to-B three-boundary regression at `application.rs:3201`; reviewer supervisor 14/14 and Application stress 50/50 accepted |
| FDR-038 immediate Component task-panic unwind | resolved | immediate payload take at `task.rs:417`, Application arbiter at `application.rs:54`, sibling abort at `task.rs:973`; regressions at `application.rs:3389`, `application.rs:3471`, `application.rs:3547`, `application.rs:3682`; reviewer focused closure accepted |
| FDR-039 supervisor shutdown runtime migration | resolved | extracted-runtime per-poll fence at `task.rs:190`; A-to-B pending regression at `task.rs:1653`; reviewer focused closure accepted |
| FDR-040 External shutdown waiter runtime migration | resolved | public waiter per-poll runtime fence and detached cleanup at `external.rs:924`; before/after-Pending regressions at `external/tests.rs:209`, `external/tests.rs:240`; reviewer focused closure accepted |
| FDR-041 fresh task panic arbitration | resolved; cancellation-terminal setup superseded | outer-boundary arbiter at `application.rs:99`; recovery suppression at `application.rs:182`, `application.rs:503`; direct/drop-without-repoll regressions at `application.rs:3489`, `application.rs:3551`; boundary-priority regression at `application.rs:4756`; External owner recovery at `external.rs:1022`, `external.rs:1234` and regression at `external/tests.rs:960`; historical reviewer External 30/30 and stress 100/100 accepted |
| FDR-042 public Component-demand consumption | resolved | public consumers at `application.rs:338`, `application.rs:379`; deterministic downstream panic/request tests at `component_api_application_demand.rs:114`; real Agent scheduling at `examples/frame_agent.rs:35`; reviewer 3/3 + Application 52/52 + demand stress 50/50 + Agent stress 50/50 accepted |
| FDR-043 legacy EventInput surface isolation | resolved | feature boundary at `Cargo.toml:12`, `authoring.rs:27`, `component/mod.rs:21`, `execution/mod.rs:10`; native/legacy constructors at `host.rs:69`, `host.rs:75`, `external.rs:904`, `external.rs:920`; isolated matrix 4/4 plus no-default 430/430, all-features 475/475 and dual strict Clippy accepted |
| FDR-044 no-default compile-harness topology | resolved | dynamic discovery/filter at `component_api_component_compile.rs:17`; native static fixture at `pass_static_component.rs:43`; reviewer default 16+43 and no-default 13+34 fixtures, full 475/475 + 430/430 accepted |
| Phase 9 native signal/provider examples | resolved | native scripted port at `scripted_provider.rs:205`; signal lifecycle at `signal_reaction.rs:194`; Responses cleanup/fresh target at `component_runtime_visual_acceptance.rs:601`; reviewer signal 8/8 + stress 50/50, provider 4/4 + stress 30/30 accepted |
| FDR-045 public External completion surface | resolved | public `ExternalAct::text` at `external.rs:706`, `ExternalControl::complete` at `external.rs:345`; exact grammar/limit regressions at `external/tests.rs:1231`, `:1265`, `:1290`; downstream no-default lifecycle at `component_api_external_completion.rs:85`; reviewer focused/stress/full dual-config gates accepted |
| Phase 9 Task 4 public Agent/Skill/Plugin executables | resolved | shared exact bytes at `frame_workflow_golden.rs:10`; real public paths at `frame_agent.rs:77`, `frame_skill.rs:223`, `frame_plugin.rs:430`; Skill passive frontend at `frame_skill.rs:240`; Plugin ownership/cleanup/panic gates at `frame_plugin.rs:447`, `:489`, `:529`, `:559`; reviewer Task 4 sign-off accepted |
| Phase 9 Task 5 native Chess migration | resolved | ordinary root and exported `ChessControl` at `chess_agent.rs:158`; explicit state-write/react/read at `model.rs:186`; native owner cleanup/panic arbitration at `game.rs:872`; offline lifecycle/game gates at `model.rs:671`, `game.rs:1773`; reviewer default/no-default 11/11 plus full dual-config gates accepted |
| FDR-046 Task 5 scripted-port pre-handoff mutation | resolved | claim-after-preparation at `model.rs:520` and `game.rs:1666`; same-Application no-mutation retry at `model.rs:821`; reviewer exact 1/1 + stress 30/30 accepted |
| Phase 9 Task 6 README/public documentation | resolved | native architecture and Chess docs at `README.md:7`, `chess-runtime-target.md:7`; corrected runnable lifecycle snippet at `README.md:61`; reviewer exact downstream compile/run, all offline commands, nine dual-config doc gates and links accepted |
| FDR-047 README primary snippet compile failure | resolved | captured identifier binding at `README.md:76`; reviewer exact candidate reconstruction and byte-identical no-default harness GREEN accepted |
| Phase 9 Task 7 final verification/review | resolved | FDR-048 through FDR-052 closed; dual final full/check/doc/rustdoc/strict-Clippy/examples/offline/package/authority/Archify/hash gates green; Phase 9 reviewer sign-off accepted |
| FDR-048 stale current-document legacy authority | resolved | superseded banners at `provider-port-application-host-boundary.md:3`, `provider-port-application-host-open-questions.md:3`; current authority correction at `semantic-agent-view.md:3`; reviewer binary suffix, 11/11 links, authority scan and dual doctests accepted |
| FDR-049 orphan legacy provider architecture artifacts | resolved | historical classification in `provider-abstraction-class.architecture.json:5`, `:15`, `:45`, `:155`, `:318`; generated HTML deterministic; reviewer Archify 9/9, topology, dual-theme mobile/desktop, guided/export checks accepted |
| FDR-050 maximum External CLI act/full-reserve mismatch | resolved | bounded CLI snapshot plus one-act distinct-target rotation at `agentview.rs:245`, `:1596`; active three-act maximum at `agentview_cli.rs:760`; reviewer exact accounting, ownership, repeated maximum, dual-mode CLI and frozen-core checks accepted |
| FDR-051 fragmented legal External CLI act timeout | resolved | bounded prepared verifier and typed provisional Full/Commit/Replace at `agentview.rs:810`, `:1168`, `:1402`, `:1747`, `:2801`, `:3246`; strict framing/lifecycle proofs at `:3640`, `:3877`; active limits at `agentview_cli.rs:1035`, `:1097`, `:1155`; reviewer final joint margin >=3,966ms, dual CLI 3x26/26 and dual full gates accepted |
| FDR-052 root release package manifest/boundary | resolved | matching derive version and exact package excludes at `Cargo.toml:7`, `:22`; reviewer derive/root package, 373-path boundary, normalized version-only manifest and 119-path freeze accepted |
| Phase 8 user-panic transparency | resolved | direct user invocation has no production catch; FDR-038 through FDR-041 resolved; reviewer panic/task/External stress and full gate accepted |
| Phase 8 Component authoring and async lifecycle | resolved | public primitives, hook topology, demand, mount fence, bounded coroutine, retirement and shutdown surfaces independently accepted; Phase 8 signed off |

## 8. Implementation responses

本节是实现方对第 3 节 finding 的正式回复。reviewer 若发现新问题，应在第 3 节增加新的稳定
FDR 编号；实现方随后在本节追加同编号 response，并同步第 7 节状态。

### FDR-001 response: resolved

- `Frame` 现在保存 prepare 时使用的完整 `FrameProfile`（`reaction.rs:350`）。
- `Frame::check_handoff_precondition()` 在 crossing poll 前同时比较 declaration validity、target
  identity、continuity 和完整 profile（`reaction.rs:429`）。
- profile-only race 返回 typed `SubmitFault::ProfileChanged`，不会 handoff 或 commit
  （`reaction.rs:650`）。
- `profile_only_change_rejects_before_handoff_or_commit` 覆盖 limits/capabilities 只变 profile 的 race
  （`application.rs:1631`）。

### FDR-002 response: resolved

- `ReactionPortFault` 改为 closed `kind/code/reason`，不接收 String、source 或 provider payload
  （`reaction.rs:608`）。
- admission mismatch 只保留 output key、长度和 closed reason；canonical/budget errors 在进入
  Application boundary 前被 sanitize（`admission.rs:973`, `admission.rs:1173`）。
- `ApplicationFault` 统一为 `stage/kind/code/reason`；tool name、authored value和底层source在分类后丢弃。
  user panic不再进入该fault boundary，而是保留原payload直接unwind。
- 两组 sentinel tests 验证 `Display`、`Debug` 和 `source()` 不包含 authored/provider 内容
  （`admission.rs:1973`, `application.rs:1376`）。

### FDR-003 response: resolved

- public `TargetDeclaration::resume(revision, profile)` 直接从 opaque revision 派生 identity 和 epoch，
  public caller 无法再传入矛盾 scope（`reaction.rs:175`）。
- raw contradictory constructor 仅存在于 `cfg(test)`；`Application::mount()` 在 root render 前调用
  declaration validation（`application.rs:136`）。
- wrong-target 与 wrong-epoch cases 均证明 root 未执行（`application.rs:1252`）。revision namespace
  本身是 opaque session token，不再有独立、可矛盾的 declaration namespace 参数。

### FDR-004 response: resolved

- `ReactionAdmissionGuard` 现在独占已经验证的完整canonical sequence，并在该transaction buffer中增量
  维护本轮tail；`admit()`不再逐delta clone/rebuild完整history（`admission.rs:387`,
  `admission.rs:444`）。
- event只在对应buffer mutation与budget candidate commit后返回；normal finish或Drop只做不可失败的
  validated buffer归还（`admission.rs:543`, `admission.rs:960`）。
- `streaming_delta_construction_work_is_linear_in_history_and_output` 对长 history 和多 delta 的构造工作量
  设定线性上界（`admission.rs:2182`）。

### FDR-005 response: resolved

- `ToolCatalog::new()` 使用 UTF-16 code-unit comparator，并拒绝空 name 与重复 name
  （`reaction.rs:258`）。
- empty/duplicate test 位于 `reaction.rs:799`；supplementary Unicode 与 BMP 排序 golden test 位于
  `frame.rs:934`。

### FDR-006 response: resolved at protocol level

- `declare() -> FullRequired` 只证明 content-independent recovery strategy 存在，并且 required private
  causal artifacts 仍被 port 保留；它不声称验证 exact request（`engine.md:254`,
  `frame-driven-runtime-plan.md:136`）。
- port 收到 exact Full 后，才在 crossing poll 前联合校验 canonical payload、required private artifacts、
  wire bytes 与 token limit；失败必须是 typed pre-handoff fault。
- 该 finding 的 contract contradiction 已解决。Responses、Chat Completions、External 对这一 contract
  的实现与 conformance 属于 Phase 6，尚未以本 response 冒充完成。

### FDR-007 response: resolved

- architecture JSON 已把 `Ready Ok(stream)` 改为返回 `Application`，并补上
  `Application -> FrameSession` synchronous commit，以及
  `FactStream -> FrameSession admission -> Component dispatch`（JSON `return-stream`、
  `commit-frame-session`、`admit-fact`、`dispatch-event` relationships）。
- `dispatch admitted event` label局部左移后，standard与showcase均为0 errors / 0 warnings，最小
  label-route clearance为4.7px；`request / observation`也收窄为有向语义准确的`request handoff`。
- Archify 2.12 `deliver`原子生成最终HTML，独立artifact `check`通过9/9 checks；HTML为620398 bytes，
  SHA-256 `6d518977324c38d0cd098c543e12eec3dbaf756c3f6e3009220d898180591d60`。
- 最终light/dark页面与canonical PNG已做独立像素检查，没有裁切、重叠、误导性route或主题可读性
  问题。交付receipt：`validation: passed`，`visual_review: passed`，`correction_rounds: 2`。

### FDR-008 response: resolved

- poll harness 已覆盖多次 Pending 后取消、Ready(Err)、Pending 后 Ready(Err)、同一 future 的
  `Pending -> Ready(Ok)` crossing transition、重复相同 declaration 和 profile-only change
  （`application.rs:1454` 至 `application.rs:1656`）。
- crossing poll test 证明 synchronous FrameSession commit 在 outer poll 返回 stream 前完成
  （`application.rs:1656`）。
- plan 首页已更新为 Phase 5 private pipeline 状态（`frame-driven-runtime-plan.md:5`）。

### Phase 4 next-Full budget response: resolved

- `FrameMeter::validate_full_reserve()` 使用 actual replay、actual staged inputs 加完整
  `max_component_bytes` envelope，checked arithmetic 失败即 fail closed（`frame.rs:112`）。
- `FullReserveTracker` 增量维护与完整 JCS meter 相同的 array/item/string byte boundaries
  （`frame.rs:531`; equivalence test `frame.rs:1016`）。
- TextDelta、ToolCall 与 ToolOutput 都在发布 event/ticket/result 前检查 hypothetical next-Full；
  oversized ToolOutput 保持原 slot 可重试，guard Drop 的 interrupted text 仍在 reserve 内
  （`admission.rs:2062` 至 `admission.rs:2160`）。

### Phase 4 projection reconciliation response: resolved

- semantic `#[diff]` baseline 与 provider-output occurrence ledger 已拆为 `ProjectionDiffState` 和
  `ProjectionReconciliationState`（`projection_diff.rs:17`, `projection_diff.rs:32`）。
- ordinary items 按 node 和 occurrence 确定性 claim；provider output 也按 occurrence claim；
  `#[diff]` 生成的 patch 由 append policy 保证不会被 historical/provider claim 吞掉
  （`projection_diff.rs:132`）。
- repeated occurrence、node omission/reappearance、provider output absorption、Full reset 与 `#[diff]`
  coexistence 均有测试（`projection_diff.rs:758` 至 `projection_diff.rs:913`）。

### FDR-009 response: resolved

- Full 被定义为 `replay + staged_inputs + reconciled component` 的 self-contained submission；raw complete
  projection 只进入 private checkpoint 与 component envelope meter（`engine.md:403`）。
- `FrameSession::prepare()` 先用 replay tail/staged inputs 更新 unclaimed outputs，再做 shared reconciliation；
  actual Full/Delta submission 使用 reconciled projection，而 complete projection 独立计量
  （`frame.rs:241`）。
- Full continuity reset test 证明 replay 中已有的 provider output 被 claim，不会再次出现在 Component
  section（`frame.rs:1223`）。

### FDR-010 response: resolved

历史v1曾采用明确terminal contract，不把reaction-local ToolCall lanes改造成跨invocation resumable
services；crossing handoff后的future Drop会poison Application，并以`CancelledAfterHandoff`阻止后续
declare/render/submit。该方案解决了finding中的无owner pending slot，但把“取消一次reaction”扩大成了
“终止整个Application”。

当前resolution取代了该terminal mitigation：ToolCall admission在发布event之前同时构造、验证并精确预留
unknown-outcome ToolOutput（`admission.rs:32`, `admission.rs:531`, `frame.rs:670`）；real ToolOutput原子替换
而不是叠加该reserve（`admission.rs:580`, `frame.rs:699`）。post-handoff普通Drop按callback -> lanes -> provider
stream -> recovery owner的顺序销毁资源，最后只为本attempt尚未完成的registration物化预验证fallback
（`application.rs:734`, `admission.rs:629`）。Application保持ready；下一次显式`react()`依赖port如实返回兼容
`Accepted`、更高epoch `FullRequired`或terminal declaration。task/direct panic则抑制recovery并保持fallback
hidden（`application.rs:182`, `application.rs:503`）。

回归覆盖reusable Full/Delta、interrupted text、fallback跨prepare/submit cancellation持久性、重复取消、
callback不resume/replay和panic排除（`application.rs:2977`起）；External next-`observe()` Full recovery、旧
ingress fencing与第三方port Drop contract位于`external/tests.rs:473`和`external/tests.rs:928`，Skill/Plugin
wrapper证据位于`integration/tests.rs:154`。权威contract已同步到`engine.md` Poll-level cancellation section和
plan ordering/tool test gates。

### FDR-011 response: resolved

`ApplicationFault::from_admission()` 现在先匹配 `ReactionAdmissionFault::ToolOutput(fault)`，直接调用
`from_tool_output(ApplicationFaultStage::Admission, fault)`；因此 identity exhaustion 保留 `Exhausted`，
budget 保留 `Limit`，reason 也保持具体 `ToolOutputStagingReason`（`application.rs:737`）。
`nested_admission_tool_output_fault_keeps_its_structural_classification` 固定该行为
（`application.rs:1407`）。

### FDR-012 response: resolved

`ResponsesFrameRequestState`只保存exact provider wire state、accepted revision和versioned canonical prefix
count/digest，不保存shared canonical mirror（`frame_request.rs:40`）。Full先验证candidate replay prefix，
再复用private wire artifact；count过大或同count不同digest均在HTTP前返回typed rejection
（`frame_request.rs:154`）。compaction保留pending function calls，matching Full recovery、mismatch和
count overflow由`frame_request.rs:797`、`:824`、`:934`覆盖；System binding由FDR-026补强。

### FDR-013 response: resolved

Native stream使用reaction-local `ready_outputs`、`buffered_deltas`和单调`next_output_index`。每个index只有
在public item形成或private item seal后才推进；public wire/canonical state在fact release前按同一顺序提交
（`native_reaction.rs:733`）。`later_text_waits_for_earlier_private_output_seal`
（`native_reaction.rs:1773`）固定“高index text先完成、低index reasoning后seal”的顺序。

### FDR-014 response: resolved

- System由shared Frame compiler作为replaceable snapshot管理，不由adapter根据隐藏history推断；
- compiler按完整projection的node/item render顺序合并全部System POM top-level children，归一为
  `None`或一个`ResolvedDocument`，不做last-value-wins或value dedup；
- Full的Component section首位至多一个normalized System item；Delta不携带System，表示保留
  accepted snapshot；initial/change/clear强制Full；
- System不进入canonical history、ordinary occurrence reconciliation或`#[diff]`；snapshot只在
  successful handoff后与revision/checkpoint原子commit；
- `frame.rs`的regression matrix覆盖多fragment顺序、initial Full、unchanged Delta、changed/clear
  Full、candidate discard、FullRequired restatement和canonical-history exclusion。

### FDR-015 response: resolved

Responses production defaults固定为Frame 16 MiB、Component 4 MiB、token hints `None`和semantic delta
enabled（`async_openai.rs:82`）；`with_responses_frame_constraints()`提供显式mount-time override并复用shared
profile validator（`async_openai.rs:223`）。target identity、epoch、accepted revision和profile在provider实例
中稳定保存（`async_openai.rs:327`）；7项profile/state tests覆盖default、custom、invalid shape、identity、
idempotent declaration、Accepted continuity和epoch exhaustion。

### FDR-016 response: resolved

Frame-native OpenAI路径只通过`map_openai_fault()`构造public `ReactionPortFault`
（`reaction_fault.rs:93`）。private diagnostics只以class、kind、byte length和HTTP status进入private tracing，
不把payload写入public error（`:188`）。完整class matrix、HTTP auth/retry语义和sentinel non-leak tests位于
`:214`、`:333`、`:377`；native request、HTTP、SSE、timeout和limit branches都调用该mapper。

### FDR-017 response: resolved

- `engine.md`与plan candidate API现在都列出`Frame::prepared_profile`、`SubmitFault::ProfileChanged`、
  `ReactionPortFault`和`ApplicationFault`的closed payload-free shape（`engine.md:159`, `engine.md:296`；
  plan `:251`, `:311`, `:345`）。
- Full revision语义已统一：只有`DeltaFrom`要求base与新revision属于同一FrameSession namespace并严格递增；
  从Accepted prepare的Full仍绑定exact precondition，但successful handoff可以rebase到当前FrameSession
  revision（`engine.md:309`, plan `:445`, conformance test `reaction.rs:871`）。
- declare-time只证明content-independent recovery capability；exact canonical/private/wire proof留在crossing
  poll。required-private-state旧冲突表述已删除。
- plan不再把同一identity的旧epoch revision列为Full fallback；epoch倒退/复用保持protocol fault。

### FDR-018 response: resolved

`FrameSession`现在把`highest_observed_epoch`与successful handoff后才推进的committed epoch分开
（`frame.rs:173`, `frame.rs:333`）。`Application::refresh_declaration()`在render/prepare/submit前调用
`observe_declaration()`；成功校验会同步、不可回滚地推进observed watermark（`application.rs:264`,
`frame.rs:360`, `frame.rs:391`）。

`observed_higher_epoch_survives_pre_handoff_rejection`固定以下序列：epoch 1 -> observe epoch 2 -> retryable
pre-handoff rejection -> epoch 1。第二次调用在render和submit前返回terminal protocol fault
（`application.rs:1499`）。

### FDR-019 response: resolved

`FrameSession::prepare()`先证明replay append compatibility，再按`ProjectionExecutionScope`选择compatible
checkpoint。scope变化会把basis降为Full，并调用`reset_authored_for_scope()`清空node-authored occurrence
ledger和diff baseline（`frame.rs:222`, `frame.rs:230`, `frame.rs:253`, `projection_diff.rs:220`）。

FDR-019只冻结authored scope fence，不再声称value-only provider occurrence跨scope可安全claim。
`non_collision_remount_succeeds_but_cross_scope_ambiguity_stays_fenced`证明新mount authored item重新进入
Component section；旧scope provider value的处理由FDR-022的ambiguity fence完成（`frame.rs:1348`）。

### FDR-020 response: resolved

`ReactionAdmissionGuard`不再把stable prefix留在Transcript、再从secondary ledger延迟materialize。
guard创建时通过crate-private validated take取得完整canonical sequence，TextDelta在event release前直接
写入`Interrupted` item，seal原位改成`Sealed`，ToolCall直接append（`transcript.rs:380`,
`admission.rs:387`, `admission.rs:603`, `admission.rs:712`）。normal finish和Drop都只调用无校验、不可失败
的restore；Drop没有allocation、rebuild或`try_from_items()`（`admission.rs:960`）。

`guard_drop_restores_released_tail_even_if_admission_ledger_is_corrupted`在event release后故意删除secondary
`AdmissionState` entry，随后Drop仍保留原prefix与实际interrupted tail（`admission.rs:1769`）。原review
指出的两处`unnecessary_unwrap`和test helper `implied_bounds_in_impls`也已清理；waive既有external enum后
strict Clippy通过。

### FDR-021 response: v1 safe, future extension deferred

第一版“累计value ledger支持任意replacement”的尝试经独立复核后被撤回：它无法证明checkpoint之后新
admit的provider output在replacement view中的occurrence origin，value matching也可能把authored occurrence
误认成provider provenance。因此没有把该尝试标为resolved support。

v1 production `HistoryPolicy`只允许`CompleteTranscript`。如果retained replay basis不再是selected view的
prefix，`FrameSession::prepare()`会在projection reconciliation和Frame production前返回专用
`ReplayReplacementUnsupported`（`frame.rs:226`, `frame.rs:701`）。Application将其映射为payload-free
terminal `Protocol/ReplayReplacementUnsupported`（`application.rs:643`）。fail-closed与classification测试
位于`frame.rs:1408`和`application.rs:1432`。

真正的canonical checkpoint/summary replacement保持`deferred`：开始该扩展前必须先定义每个selected
occurrence的origin/provenance、coverage、digest和ToolCall closure grammar（`engine.md:362`, plan `:160`）。
在该边界内，FDR-009/Phase 4的v1 correctness gate可以关闭；不能声称future replacement已经实现。

### FDR-022 response: resolved

v1没有伪造逐item provenance。`ProjectionReconciliationState`现在把本scope provider occurrence与持久
`ambiguous_provider_outputs`分开；scope reset清空node/current-scope ledgers，并把旧scope累计provider
outputs以及checkpoint之后尚未reconcile的replay tail/staged inputs一起移入ambiguity fence
（`projection_diff.rs:32`, `projection_diff.rs:227`, `frame.rs:324`）。因此direct remount也不能把旧tail错误
开放成新scope claim。

普通item的确定顺序是：`#[diff]` append-forced先直接提交；否则先claim同node ledger，再claim本scope
provider occurrence；如果随后仍命中跨scope ambiguity，则返回
`ProjectionReconciliationFault::AmbiguousProjectionProvenance`；最后才作为新authored item提交
（`projection_diff.rs:181`）。ambiguous multiset从不被claim，失败prepare不提交candidate，无关的成功Frame
也会继续保存fence。

fault经`FrameSessionFault::ProjectionReconciliation`精确映射为payload-free terminal
`FramePrepare/Protocol/AmbiguousProjectionProvenance`（`frame.rs:827`, `application.rs:608`）。公共trait、
`Frame`和`ReactionPort`形状没有变化；Application fault reason只新增这一closed classification。

repository regressions覆盖：

- reviewer原始`base -> provider collision -> remount authored collision`序列以及cursor不推进
  （`frame.rs:1587`）；
- 没有中间same-scope Frame的direct remount tail（`frame.rs:1631`）；
- non-collision remount成功、后续Frame仍受fence约束（`frame.rs:1661`）；
- 重复同值occurrence按count先消耗same-scope claim，剩余跨scope match fault
  （`projection_diff.rs:979`）；
- 无关成功Frame不消费ambiguity，以及`#[diff]` append item绕过ambiguity
  （`projection_diff.rs:1016`, `projection_diff.rs:1043`）；
- Application fault structural classification（`application.rs:1380`）。

同一execution scope内仍明确采用canonical value/count等价规则；这不是逐item provenance。若未来要求区分
同scope独立authored与provider的同值occurrence，必须扩展projection origin grammar，不能把本次保守
cross-scope fence描述为完整provenance实现。该限制已同步到`engine.md:361`和plan `:669`。

2026-08-29固定工作流复核重新执行了两组focused gates：
`cargo test --all-features --lib scope -- --nocapture`通过8项，
`cargo test --all-features --lib ambiguous -- --nocapture`通过3项。两组共同重放上述原始碰撞、
same-scope claim优先、direct remount tail、持久fence、`#[diff]` bypass与Application typed classification；
没有修改`native_reaction.rs`。

### FDR-022 reviewer sign-off: accepted for v1

reviewer独立重放原始同值反例并检查candidate生命周期后接受本resolution。实现没有伪造provenance：
跨scope普通item在无法证明origin时返回typed terminal fault；失败prepare不推进revision、cursor、checkpoint
或canonical history。scope切换前尚未checkpoint的tail、重复同值count、无关成功Frame后的持久fence、
`#[diff]` append-forced bypass和Application fault classification均有repository regression。

独立门禁结果：`cargo test --quiet`全部通过（library 319 tests）；
`cargo check --all-targets --all-features`、`cargo fmt --all -- --check`、`git diff --check`以及
`cargo clippy --all-targets --all-features -- -D warnings -A clippy::large-enum-variant`全部通过。
因此FDR-022可以签收，Phase 4 v1 gate可以关闭；future逐item provenance不在本次resolution范围内。

### FDR-023 response: resolved

`ResponsesReactionTarget`现在区分retryable continuity loss和sticky terminal fault：retryable fault或未完成
stream Drop清除accepted revision并推进epoch；terminal fault保存exact closed classification，后续
`declare()`稳定返回同一fault；epoch exhaustion也进入terminal Declaration state
（`async_openai.rs:327`, `async_openai.rs:363`, `async_openai.rs:381`）。handoff至HTTP response之间由
`PostHandoffContinuity`守护，SSE state通过`failed_native_stream()`在yield error前记录fault
（`native_reaction.rs:274`, `native_reaction.rs:495`）。500/EOF、401和terminal protocol回归分别位于
`:1386`、`:1418`、`:1442`、`:1466`。

### FDR-024 response: resolved

`OpenAiOutputLedger::validate_completed_allowing_no_primary()`复用完整terminal identity/lifecycle/private-seal
校验，但不强制non-commentary final text（`output.rs:706`）。Native adapter自己保存primary output key，
commentary不成为primary；tool-only、reasoning+tool和commentary-only均发布
`ReactionCompleted { primary_text: None }`。direct regressions位于`native_reaction.rs:1643`、`:1674`、
`:1704`。

### FDR-025 response: resolved

Responses provider新增`Unclaimed | Legacy | FrameNative`实例级mode（`async_openai.rs:320`）。只读
`declare()`不claim；legacy/native都在各自first transport crossing poll、真实或模糊delivery之前claim。
request preparation/body-limit等确定pre-handoff failure保持Unclaimed；一旦handoff，另一trait入口返回
typed rejection，不能读取或推进另一套continuation。tests位于`native_reaction.rs:1503`和`:1517`。

### FDR-026 response: resolved

opaque compaction seal同时保存versioned normalized System instructions digest
（`frame_request.rs:27`, `frame_request.rs:289`）。Full recovery复用compaction前先比较candidate System；
change/clear返回`CompactionInstructionsMismatch`，same snapshot继续恢复。没有compaction时，普通System
replace/clear仍按FDR-014正常工作。regressions位于`frame_request.rs:876`和`:994`。

### FDR-027 response: resolved

Frame-native `declare()`和crossing poll都会验证`Accepted(revision)`对应存在且revision相同的
`ResponsesFrameRequestState`（`native_reaction.rs:230`, `native_reaction.rs:246`, `native_reaction.rs:89`）。
missing/mismatched required state立即转为sticky terminal Declaration fault；FullRequired仍允许从exact
Frame重建。`accepted_declaration_without_matching_native_state_fails_closed`位于`:1490`。

### FDR-028 response: resolved

`ChatFrameRequestState`只保存accepted revision与上一份request的wire messages；SSE completion不修改该
baseline。下一Frame的replay tail按`replay -> staged_inputs -> projection`首次append prior output，随后在
handoff poll原子安装新baseline。`next_delta_adds_the_prior_output_and_new_input_once`检查两轮真实HTTP body，
第二轮严格为`authored, answer, next`，没有重复assistant output。

### FDR-029 response: resolved

Chat `[DONE]`分支在accumulated text为空时直接发布
`ReactionCompleted { primary_text: None }`并结束stream；非空时才依次发布key 0的`TextSealed`与terminal。
`empty_completion_does_not_fabricate_a_text_output`覆盖empty grammar，普通nonempty lifecycle测试覆盖
delta/seal/completion三段identity一致性。

### FDR-030 response: resolved

`DebugProviderPort`新增`Unclaimed | Legacy | FrameNative`。legacy在prompt成功render、准备写capture时claim；
native在exact precondition验证后、写Frame capture时claim。unpolled submit、continuity/profile mismatch均
零capture且保持Unclaimed；两个handoff顺序的另一入口都返回typed fault。

### FDR-031 response: resolved

Debug allocator现在返回`Result<TargetIdentity, ReactionPortFault>`，耗尽时保存稳定terminal
Internal/Declaration state；`new()`和`Default`不panic。identity使用独立高64位Debug domain，tests覆盖两个
instance不相等、domain marker和local exhausted counter重复返回同一closed classification。

### FDR-032 response: resolved

Debug声明有限默认`max_frame_bytes = 16 MiB`、`max_component_bytes = 4 MiB`，继续支持semantic Delta。
profile test验证exact defaults、declaration幂等、instance identity分离；Application Full/Delta capture test
证明Accepted continuity继续使用同一mount-stable profile。

### FDR-033 response: resolved

External shared state现在用同一mutex线性化control liveness与ingress安装。last control先Drop时推进
continuity，`submit()`在reserve后确定pre-handoff reject；ingress先安装时Drop关闭其sender，已handoff的
stream因此可以终止。没有terminal或既有fault的channel EOF固定只yield一次retryable
`StreamTransport`，不会退化成normal EOF或重复fault。claimed-act cancellation和
`reserve -> control Drop -> install` barrier回归分别位于`external/tests.rs:224`和`:259`。

### FDR-034 response: resolved

last `ExternalControlInner` Drop现在在与ingress相同的shared mutex下安装sticky terminal
`Unavailable/Declaration`，而不是推进一个已经没有receiver的recoverable epoch（`external.rs:214`）。
已有terminal fault遵守first-fault-wins；若已有active ingress，同一个terminal fault会关闭当前fact stream；
terminal state也阻止后续continuity reset覆盖该结论（`external.rs:151`, `external.rs:214`）。sender已经被
act claim时，EOF fallback也会读取target terminal，不会把永久owner loss降级回retryable fault
（`external.rs:580`, `external.rs:659`）。

`submit()`在outbound receiver已经关闭以及`reserve -> Drop -> install`两个pre-handoff路径都会读取同一
sticky target fault，确定返回terminal `SubmitFault::Rejected`；不会返回retryable Transport，也不会
handoff Frame（`external.rs:507`, `external.rs:530`）。

repository regressions覆盖：control在Application mount前消失时declaration失败且root零render
（`external/tests.rs:228`）；accepted reaction正常结束后control消失时重复declare返回相同terminal fault
（`external/tests.rs:247`）；最后一个control在claim sender后被取消时EOF仍返回sticky terminal
（`external/tests.rs:306`）；reserve后control消失时terminal reject且不会hang（`:335`）。active stream也
验证收到同一个terminal classification并由后续declare保持sticky（`:210`）。

验证：External focused tests 21/21、全量`cargo test --all-features --no-fail-fast`通过（library 405 tests，
其余integration/trybuild suites通过，既有7项ignored）、`cargo check --all-targets --all-features`、
`cargo fmt --all -- --check`和`git diff --check`通过。Phase 7 gate是否重新关闭仍等待reviewer独立签收。

### FDR-034 reviewer sign-off: accepted

独立复核接受该修复：

- reviewer原`/tmp/fdr034-repro`现在输出
  `terminal declaration fault: reaction port Terminal: Unavailable/Declaration`，不再返回recoverable epoch；
- control Drop、`declare()`、queue receiver关闭和reserve后的liveness check由同一shared target terminal串联；
  Drop-before-lock是terminal pre-handoff，submit先安装ingress则是合法handoff后stream terminal，没有
  Pending跨越boundary；
- terminal遵守first-fault-wins，stream guard不能再用continuity reset覆盖它；claimed sender和full queue
  仍可在EOF fallback读取同一terminal；
- reviewer独立运行`cargo test --lib component::execution::external::tests --quiet`：21/21通过，覆盖mount前、
  completed reaction后、claimed act cancellation、active stream和post-reserve Drop；
- 并发Phase 8快照稳定后，reviewer再次运行全量`cargo test --all-features --quiet`：library 418/418且其余
  suites无失败；`cargo check --all-targets --all-features`、strict Clippy、format和`git diff --check`全部通过。

FDR-034可以关闭，Phase 7重新签收；当前后续工作为Phase 8。

### FDR-035 response: implementation candidate

`ExternalApplication::shutdown(self)`现在在调用时立即把唯一owner移入专用cleanup task，返回的waiter只负责
观察完成；waiter被取消或Drop不会取消cleanup（`external.rs:918-944`）。cleanup先向active reaction发送取消、
join reaction并恢复`Application` owner，再调用`Application::shutdown()`完成tree fence、task abort与join
（`external.rs:1003-1027`）。

CLI从`DaemonState`取走owner后await完整shutdown，只有cleanup完成后才写Shutdown response
（`src/bin/agentview.rs:797-809`）。Agent、Skill和Plugin的实际test owners也显式调用consuming shutdown
（`integration/tests.rs:112-114`, `:168`, `:233-235`）。repository regressions证明waiter取消后cleanup仍完成
（`external/tests.rs:269`），以及CLI acknowledgement等待active reaction和mount-task destructor barrier
（`src/bin/agentview.rs:1012`）。本finding在ledger中保持`in progress`，等待独立review接受production owner
coverage。

### FDR-036 response: implementation candidate

core和actor的永久generation watermark已替换为per-retirement临时claim；claim在`Retire` enqueue前安装，
matching task全部abort并join后在actor与core两侧释放，completed weak waiter同时移除。overlapping retirement
各自持有claim，一个completion不能释放另一个仍在途的fence（`task.rs:53`, `task.rs:500`, `task.rs:1019`）。

永久stale authority属于mount共享的`MountFence`。`ComponentTaskContext::register()`持有该fence直到`Start`
enqueue；unmount通过同一fence完成invalidate后才调用`retire()`。因此start先赢时，core mutex和actor FIFO保证
`Start`位于`Retire`之前并被回收；retire先赢时，stale registration不会到达supervisor。retirement完成后可以
删除临时claim，而旧context仍由`MountFence`永久拒绝（`async_task.rs:58`, `application.rs:570`,
`application.rs:607`）。

回归覆盖同一Component的512代完成后全部bookkeeping归零、512个distinct `root/item#N`缩到一个live task再
缩到零、阻塞task destructor期间core/actor claim与waiter保持且拒绝迟到start、join后全部归零，以及cleanup
完成后旧Component context仍返回`SpawnError::StaleMount`（`task.rs:1214`, `task.rs:1249`, `task.rs:1353`,
`async_task.rs:414`）。focused task tests 14/14、async-task authoring tests 5/5通过；本finding保持`in progress`，
等待独立closure。

### FDR-037 response: implementation candidate

supervisor首次创建actor时保存稳定`tokio::runtime::Handle::id()`；start、retire、preflight、monitor、
retirement wait和shutdown都执行runtime-affinity fence（`task.rs:488-514`）。monitor、retirement和consuming
shutdown的异步wait在每次poll检查当前runtime，不只在future入口检查。foreign runtime会同步关闭supervisor、
abort旧actor、关闭retirement waiters并通知monitor，后续start/retire fail closed。

`ActorLifecycle` RAII finalizer覆盖actor future因owning runtime shutdown被直接Drop的路径，并同步调用
`close_after_actor_failure()`（`task.rs:759-795`）。回归覆盖owning runtime关闭、alive-but-idle runtime A到B
迁移，以及monitor/retirement的pending future迁移（`task.rs:1340`, `task.rs:1397`）。本finding保持
`in progress`；consuming shutdown的额外迁移窗口分别由已签收的FDR-039与FDR-040闭合。

Application outer boundary现在同时仲裁application terminal state、fresh task panic与supervisor `Closed`；
`react()`把`Closed`映射为既有typed Component-runtime terminal fault，blocking wait与nonblocking take返回
`DriverDemandFault::StaleMount`。nonblocking take在消费前后各仲裁一次，不能在runtime关闭后泄漏`Ok(false)`或
`Ok(true)`（`application.rs:93`, `application.rs:351`, `application.rs:433`）。

Application级回归在runtime A mount一个pending `use_future`并预置sticky demand，关闭A后在runtime B依次调用
`react()`、blocking demand和nonblocking take；三者均有界失败，port计数保持declare/render/submit/handoff为
`1/1/0/0`（`application.rs:3201`）。完整Application module 52/52通过。

### FDR-036 and FDR-037 reviewer sign-off: accepted

reviewer独立核对transient claim的FIFO/MountFence证明和Closed-aware outer arbiter；task 14/14、async context 5/5、
Application 52/52通过，distinct-ID与runtime-closure各stress 50/50。两项finding接受resolved。

### FDR-038 response: resolved

第一项Component task panic现在立即发布原payload；`TaskPanicMonitor::take_payload()`不等待sibling drain
（`task.rs:343-350`）。actor在进入任何await前同步`abort_all()`，queued future cleanup随后进行
（`task.rs:869-906`）。Application的唯一panic arbiter先原子写入terminal state，再take payload并
`resume_unwind`（`application.rs:54-63`, `application.rs:196-207`）；preflight、retire、retirement wait和task
start观察到`Panicked`时全部进入同一arbiter。caller主动catch后，后续API返回typed terminal fault，不会再次
取得payload。

回归覆盖blocking sibling destructor不阻塞unwind、reconcile中panic保留原payload、bootstrap首次poll顺序无
假设，以及没有active driver时在下一boundary unwind（`application.rs:3270`, `:3352`, `:3428`, `:3461`）。
独立review已接受本finding的focused closure。

### FDR-039 response: resolved

`MountTaskSupervisor::shutdown()`提取`ActorControl`后继续保存其`runtime_id`，并在join的每次poll先验证当前
runtime；mismatch时abort actor、关闭supervisor与waiters并返回typed `Closed`，不会在foreign runtime永久等待
（`task.rs:122-164`）。A上先poll至Pending、再迁移到B的确定性回归位于`task.rs:1467`。独立review已复跑
supervisor tests并接受本finding的focused closure。

### FDR-040 response: resolved

public `ExternalApplication::shutdown()` waiter保存cleanup task创建时的Tokio runtime identity，并在每次poll前
验证；foreign/no-runtime poll立即返回`ExternalApplicationFault::ShutdownTaskFailed`
（`external.rs:924-944`）。返回typed failure时其`JoinHandle`只被Drop/detach，不会abort已经取得唯一owner的
cleanup task，因此恢复驱动原runtime后cleanup仍继续完成。

repository regressions分别覆盖cleanup首次poll前迁移和cleanup已经Pending后的迁移
（`external/tests.rs:171-266`）；waiter Drop、active reaction join和task destructor barrier仍由
`external/tests.rs:269`及`src/bin/agentview.rs:1012`覆盖。独立review另用owner-liveness barrier验证detached
cleanup继续释放owner，并已接受本finding的focused closure。

### Phase 8 user-panic transparency response: implementation candidate

owner冻结v1不做user panic recovery。生产路径已移除Component root/render、provider/legacy event callback、native
tool callback/future、streaming `FromStr` decoder、engine observer与Responses usage observer外层的
`catch_unwind`；panic不再转换为`RootPanicked`、`RenderPanicked`、`AttemptPanicked`、handler/tool/parser panic
fault，也不再被observer路径吞掉（`host.rs:200`, `attempt.rs:615`, `attempt.rs:662`, `handler.rs:23`,
`native_tool.rs:33`, `streaming_xml.rs:259`, `application_host.rs:85`, `usage.rs:115`）。正常callback
`Result::Err`仍保留原typed fail-closed contract。
`ReactionLifecycle::Drop`在stack unwind期间不重入user observer（`application_host.rs:100`），避免在已经panic的
observer/state上继续调用或以secondary panic覆盖原payload；panic路径不伪造terminal/cleanup observation。
Runtime只用RAII丢弃尚未publish的candidate，不声称回滚user callback已经写入的共享状态。caller若在Runtime
外部自行catch同步user panic，必须丢弃相关Application和可能已mutate的user资源；v1不提供隔离副本、rollback或
poisoned-state recovery。下面的task-panic terminal state是跨Tokio task边界搬运payload所需的额外fence，不代表
同步callback panic可以被catch后恢复。
task panic payload开始unwind不等待sibling drain；若caller随后consuming shutdown该terminal Application，
supervisor仍join已abort tasks并等待destructor后返回`Panicked`（`task.rs:121`），不会把cleanup遗漏到owner之外。
该candidate范围是frame-driven Component/Application Runtime。legacy `AgentTurnObserver`的post-commit
fire-and-forget contract未被本次Phase 8暗改；若后续统一panic policy，需要单独引入supervised observer owner。

Component task future不再包裹`FutureExt::catch_unwind`，而是直接交给Tokio `JoinSet`（`task.rs:836`）；Tokio
`JoinError`只负责在`task.rs:970`取回原payload并送到Application arbiter。production catch仅剩actor boundary
payload transport（`task.rs:785`）、queued-future Drop仲裁（`task.rs:958`）和reaction Drop仲裁
（`application.rs:94`），均不恢复对象或生成业务fault。workspace没有增加`panic = "abort"`profile。

测试中的`catch_unwind`只作为观察外壳，验证root/render、sync/async handler、System/hook contract、observer与
usage observer panic确实离开production API；candidate render panic另验证不会publish projection/topology。
focused gate与`cargo test --all-features --no-fail-fast`完整gate均已通过；独立review仍待完成。

### FDR-041 response: resolved; cancellation-terminal setup superseded

三个outer driver入口共用`begin_outer_driver_boundary()`（`application.rs:99`）。helper先仲裁已经锁存且
尚未消费的task panic，再读取Application terminal state；若准备返回既有terminal classification，会在返回前用
同一个monitor再仲裁一次，并重新读取最终state。READY路径原有的biased wait/operation后status check保持不变；
最终检查之后才发生的panic可线性化为发生在本次返回之后，由下一outer boundary传播。

当前cancellation recovery control持有同一个panic monitor；biased task-panic分支在drop reaction之前显式suppress，
而Recovery Drop还同步检查monitor status，覆盖panic已经锁存但caller不再poll、直接drop `react()`的交错
（`application.rs:165`, `application.rs:182`, `application.rs:503`）。因此panic不会被误报为cancellation，也不会
发布unknown-outcome fallback。确定性Application回归先取消已handoff reaction并保持Application ready，随后释放
mount task并等待supervisor锁存panic；三个入口都传播exact原payload，catch后再次调用只返回typed
`ComponentRuntime`/`StaleMount`（`application.rs:3551`, `application.rs:4756`起）。External integration同样在
blocked handler取消后取回owner，再触发task panic并由下一次public `observe()`传播原payload
（`external/tests.rs:960`）。

以下保留的是原terminal mitigation时期的历史gate记录。第一版focused gate为3个Application regressions和1个
External integration regression；实现方单次门禁和
`cargo test --all-features --no-fail-fast`通过，library 468/468、CLI 20 passed和1项既有ignored，其余
integration/trybuild suites无失败；`cargo check --all-targets --all-features`、不带waiver的strict Clippy、format和
`git diff --check`也全部通过。

reviewer独立复核接受三个direct Application边界，但External regression压力复跑50轮得到37 pass / 13 fail；失败
返回`ObservationChannelClosed`而不是原panic payload。该response尚未闭合production integration requirement，
FDR-041保持open，详见第3节reviewer复核结果。

### FDR-041 follow-up: External closure/completion arbitration（historical implementation record）

`ExternalApplication::await_next_observation()`不再把`ObservationChannelClosed`立即分类为control fault。该分支保留
当前`ExternalReaction`和armed cancellation guard，await同一个reaction `JoinHandle`，然后与select中的
JoinHandle-ready分支共用`finish_before_observation()`分类：panic `JoinError`仍由`consume_failed_reaction()`取回
原payload并`resume_unwind`；Tokio cancellation保留`ReactionTaskFailed`；正常completion、Application fault和
reaction cancellation保留原有分类（`external.rs:1108-1190`, `external.rs:1255-1264`）。这里没有新增
`catch_unwind`，也不提供panic后的Application恢复。

该等待由sender ownership给出有界保证。唯一persistent frame sender是reaction内
`Application -> ExternalProviderPort::frames`；`ExternalControl`只持有receiver。`submit()`唯一的额外sender clone
由`OwnedPermit`拥有，成功handoff时`permit.send(...)`返回的sender在`submit()`返回前同步丢弃；模块内也不调用
`Receiver::close()`。因此empty receiver观察到channel closed时，persistent sender和所有submit permit都已经销毁。
正常返回但尚未消费的`ExternalReactionCompletion`仍持有`owner -> Application -> port -> sender`，不会制造这个
signal；closure之后只可能剩Tokio task completion publication，而不是仍持有producer的provider/user工作
（`external.rs:203-215`, `external.rs:422-426`, `external.rs:510-572`, `external.rs:1153-1163`）。

新增确定性回归覆盖三个边界：`closed_observation_waits_for_reaction_completion_before_classifying`先销毁owner关闭
channel，再延迟reaction panic，证明首次poll保持Pending且释放后传播exact payload；
`outstanding_submit_permit_keeps_observation_channel_open_until_handoff`固定sender strong count为
`1 -> 2 (OwnedPermit) -> 1`并证明permit存活时observation不能报告closure；
`aborted_reaction_after_observation_closure_is_a_bounded_task_failure`证明abort在1秒内分类为
`ReactionTaskFailed`而不是hang或`ObservationChannelClosed`（`external/tests.rs:639`,
`external/tests.rs:1021`, `external/tests.rs:1063`）。

实现方复跑External focused为30/30；原production regression
`late_component_task_panic_after_cancellation_crosses_external_boundary`独立重复50轮为**50 passed / 0 failed**。
`cargo test --all-features --no-fail-fast`完整门禁通过：library 471/471、CLI 20 passed和1项既有ignored，其余
integration、trybuild和doc tests无失败；`cargo check --all-targets --all-features`、不带waiver的strict Clippy、
`cargo fmt --all -- --check`和`git diff --check`也全部通过。提交时finding、ledger和Phase 8 gate仍保持open，
等待下述独立复核。

### FDR-041 reviewer sign-off: accepted

reviewer独立确认sender ownership证明、共享completion classifier和三个新增确定性边界；External完整surface
30/30通过，原production竞态回归独立重复100轮为100/100。panic payload、abort typed classification、permit
liveness与catch后的owner fence均未退化。FDR-041接受resolved；该结论不替代Phase 8其余surface review。
后续reusable cancellation实现保留这项sign-off，并新增“锁存panic后不再poll而直接drop reaction”的保护回归；
cancellation不再创建terminal state，但尚未传播的task panic仍优先且会抑制fallback recovery。

## 9. Responses Phase 6 migration contract and closure

本节前半保留migration scout冻结的边界；这些要求现已由上述Responses native实现和test gate落实。

Responses port 保留 HTTP/config、mount-stable target declaration/accepted revision、wire-only encrypted
reasoning/compaction/output order、reaction-local SSE ledger 和 usage observer。以下状态必须删除或停止作为
authoritative owner：provider `ToolOutputStaging`/sink/receipt、submitted projection/items、unclaimed provider
outputs、projection diff memo、history epoch、semantic budget和projection reconciliation。这些已经由
private `FrameSession` 统一拥有。

Frame lowering 固定按以下 typed section 顺序读取，port 不再 dedup，也不反序列化
`canonical_bytes()`：

```text
frame.submission().replay()
-> frame.submission().staged_inputs()
-> frame.submission().projection().items()
```

Full 用三段构造完整 canonical candidate，再合并已验证 coverage 的 required private artifacts；
DeltaFrom 要求当前 accepted revision exact 等于 base，并追加三段新 items。ToolCatalog 每 Frame 都是完整
snapshot。真实 wire body、bytes和token limit必须在 crossing poll前验证。

Responses lifecycle 到 `ProviderFact` 的映射冻结为：output text delta -> `TextDelta`；validated message
done -> `TextSealed`；validated function-call done -> `ToolCall`；validated response completion ->
`ReactionCompleted { primary_text }`。reasoning/compaction只 seal private artifact，不 yield public fact。
commentary delta也必须 yield；`CompletedOpenAiOutput` 必须携带 primary output index，不能在 terminal adapter
重新猜 key。

Codex lowering已升级为一个canonical item到`0..N` wire items；interrupted assistant编码为assistant output
加interruption boundary（`codex_http_v1.rs:282`）。Native crossing poll不消费provider-owned ToolOutput
receipt：pre-handoff failure不安装state；真实/模糊delivery poll同步安装private wire candidate与accepted
revision，并在同一poll返回`Ready(Ok(stream))`（`native_reaction.rs:43`）。

Phase 6 最小 test gate包括 exact Full/Delta body、continuity/profile race、same-poll handoff、private
reasoning/compaction recovery、coverage digest mismatch、完整 text terminal grammar、下一Frame ToolOutput
exactly-once、out-of-order output release、interrupted replay和wire-body inclusive limit。

上述Responses gate已全部覆盖：request conformance 15项、native lifecycle 17项、fault mapping 3项；独立
review在sticky terminal、optional primary、mode fence、accepted-state一致性和System-bound compaction proof
修复后未发现新的协议blocker。该结论只关闭Responses slice，不提前关闭Chat或Debug migration。

### Phase 5 pipeline response: implemented

private `Application::react()` 当前顺序是：refresh declaration -> reconcile complete projection -> prepare
Frame -> submit and synchronous commit -> concurrent fact/lane pump -> conditional post-reconcile
（`application.rs:187`）。ToolCall fact admission 后立即启动 lane；`tokio::select!` 让 lane 与 pending
fact stream 并发推进，stream fault 后也会 drain 已启动 lanes（`application.rs:316`）。本阶段不自动
驱动 reaction，外部 Agent / Skill / Plugin driver 仍负责决定何时显式调用 `react()`。

## 10. Chat and Debug Phase 6 closure

Chat native port不复用legacy `ChatHistory`作为correctness owner。`ChatFrameRequestState`只保存accepted
request wire messages；Full从exact Frame重建，Delta要求exact accepted base并append三个typed section；
System只允许Full Component snapshot，serialized body limit在handoff前按最终JSON bytes检查。SSE映射是
单一text lifecycle key 0，empty completion没有key；retryable/drop推进epoch，terminal fault sticky；
legacy/native只在first real transport poll claim mode。

Chat v1明确是text-only：非空`ToolCatalog`在handoff前typed reject，上游tool call terminal reject。
`FrameCapabilities`尚未有tool-support bit，因此本closure只证明现有text-only target，不声称Chat具有
Responses的ToolCall能力。

Debug native port只复制`Frame::canonical_bytes()`及revision/basis，不保留shared history/diff。它的submit
future无await，首次poll同步完成precondition、capture、Accepted更新并返回`Ready(Ok(stream))`；mode、
identity exhaustion和profile均按FDR-030至FDR-032闭合。

### Debug reviewer sign-off: accepted for Phase 6

reviewer独立核对后未发现新的blocker：首次真实handoff固定`Legacy`或`FrameNative`，local render与
precondition failure在claim前返回；unpolled submit、continuity/profile mismatch均不capture、不claim。
identity exhaustion返回稳定terminal declaration fault且不panic，Debug identity使用与Responses、Chat
分离的domain。native submit无await，首次poll同步capture、更新accepted revision并返回
`Ready(Ok(stream))`。

独立门禁结果：Debug单元测试7/7、Debug公共API测试2/2，以及带既有
`clippy::large-enum-variant` waiver的strict Clippy全部通过。因此Debug slice可以签收，Phase 6 built-in
target migration gate可以关闭。

## 11. Phase 7 post-implementation review (historical snapshot before FDR-034)

本节保留FDR-034之前的Phase 7签收证据；其当时的“无新blocker”结论曾被FDR-034复核覆盖。FDR-034的
后续修复和独立签收见第8节；当前Phase 7 gate状态以文首状态为准。

独立复核接受以下已经实现的slice：

- External outbound queue传递exact move-only `Frame`；`ExternalObservation`的Full/Delta lineage直接来自
  `FrameBasis`和`FrameRevision`，没有第二套string diff baseline（`external.rs:78`, `external.rs:85`）。
- queue capacity先由`reserve_owned()`异步取得，continuity/profile在crossing poll重新校验，permit send后
  同poll返回fact stream（`external.rs:503`）。pre-handoff reserve cancellation与普通receiver failure已有
  direct poll tests（`external/tests.rs:149`, `external/tests.rs:176`, `external/tests.rs:210`）。
- ingress generation由adapter分配，control在注入前一次性claim sender；正常stream结束后的late act返回
  `StaleIngress`（`external.rs:180`, `external.rs:346`, `external/tests.rs:392`）。后续reusable cancellation
  supersession让post-handoff `ExternalApplication` cancellation归还Application owner；下一次显式`observe()`
  驱动更高epoch Full recovery，cancelled generation的late ingress仍返回`StaleIngress`
  （`external/tests.rs:928`）。
- internal driver demand使用mount-owned pending bit和`Notify::enable()`关闭wait-before/request竞态；request
  sticky、coalesce、wait cancellation、mount Drop fence均有unit test，Signal write后request的下一Frame也有
  Application integration test（`driver_demand.rs:62`, `driver_demand.rs:111`, `application.rs:2085`）。
- `SkillPort`和`PluginPort`只是frame-native queued exchange的semantic role wrapper，不拥有history、diff或
  cursor（`integration.rs:21`, `integration.rs:71`）。Skill latest/typed state mutation不会隐式submit；Plugin
  registry按parent持有独立Application并拒绝late message（`integration/tests.rs:131`,
  `integration/tests.rs:180`）。
- Agent(Debug target)、Skill和Plugin对相同root产生相同canonical Frame payload，三者都走private
  `Application + FrameSession` compiler（`integration/tests.rs:80`）。

FDR-033已经关闭：control liveness与ingress安装在同一mutex下线性化；last control Drop在没有active
ingress时推进continuity，submit取得queue permit后会在handoff前重新检查control liveness。已经handoff但
未见terminal/fault的fact-channel EOF固定产生一次retryable `StreamTransport`，因此claimed act cancellation
不会再退化为normal EOF。新增回归同时固定了`reserve -> last control Drop -> ingress install`交错，证明该路径
在handoff前typed reject且不会hang（`external.rs:176`, `external.rs:211`, `external.rs:521`,
`external.rs:634`, `external/tests.rs:224`, `external/tests.rs:259`）。

独立门禁结果：External focused 18项、driver demand 6项、integration 3项和Application demand integration
1项通过；`cargo test --quiet`全量通过（library 401 tests，其余integration/trybuild suites通过，既有7项
ignored）；`cargo check --all-targets --all-features`、`cargo fmt --all -- --check`和`git diff --check`通过。
不带waiver的strict Clippy也通过。未发现新的协议blocker，Phase 7可以sign off；
Phase 0至Phase 7 gates均可关闭，下一阶段是Phase 8。

Residual scope保持原计划边界：public Component demand hook和async lifecycle属于Phase 8；curated public
`Application`、兼容feature和executable examples属于Phase 9；Skill subcommand schema与Plugin multiplexing
wire syntax不在本phase冻结。

## 12. Phase 8 implementation checkpoint

本节由实现方维护，只记录提交给独立review的candidate，不提前关闭Phase 8 gate；reviewer的新finding仍在
第3节使用稳定FDR编号追加。

- generalized hook topology使用一个lexical cursor和closed `HookKind`。Signal、provider handler、reaction
  demand、future与coroutine发生kind/order/count/site drift时直接panic；失败的candidate render不发布
  topology、callback或task factory；
- `use_provider_event_handler(ProviderEvent::TEXT, callback)`在`view!`外声明，使用typed selector和ordered
  multicast；returned error沿用handler fault，callback invocation/future panic直接unwind。Signal write只dirty，
  post-reconcile可见但不隐式submit；
- public `use_reaction_request() -> ReactionRequest`只提交mount-fenced sticky/coalesced demand；它不dirty、
  不render、不react、不submit。低层Host没有orchestration capability时contract panic；旧mount handle返回
  `ReactionRequestError::StaleMount`；
- public `spawn`只能在committed handler或Component-owned task context中注册一次性task，nested spawn继承同一
  mount scope；render或普通外部调用返回typed `SpawnError`；
- `use_future`每个mount启动一次，same-mount rerender不重启；`use_coroutine(capacity, service)`保留一个bounded
  typed sender和FIFO inbox，async send提供backpressure，stale/closed rejection归还原message；
- bootstrap与后续mount使用相同start boundary：projection/topology commit后立即向Application-owned supervisor
  注册new-mount task。bootstrap registration在`Application::mount()`返回前完成，不依赖第一次`react()`或
  driver wait；注册不隐式产生demand、dirty或Frame submit；
- unmount先通过共享`MountFence`关闭Signal、handler、demand、spawn和coroutine capability，再abort并await该
  mount全部task；retirement完成后才发布replacement projection并允许下一Frame submit。pending retirement
  future取消后可继续等待同一个transition；
- supervisor用一个lazy Tokio actor和`JoinSet`按mount管理task。user future直接交给Tokio，不由framework显式
  `catch_unwind`；Tokio `JoinError`是跨task stack的payload transport。completed weak retirement waiter会prune，
  core/actor stale fence只在retirement排队、abort和join期间保留临时claim，完成后归零；永久stale identity由
  `MountFence`保持。actor保存Tokio runtime identity，RAII finalizer与逐poll affinity fence让runtime shutdown
  和future迁移确定fail closed；
- `react()`、blocking demand wait和nonblocking demand take共享outer-boundary arbitration；supervisor `Closed`
  在任何新declare/render/submit或sticky demand消费前typed fail closed，fresh panic仍优先；
- 第一项未捕获task panic立即发布原payload并同步启动sibling abort；sibling drain不是unwind前置条件。
  Application的唯一arbiter先标记terminal，再从当前或下一outer driver boundary `resume_unwind`。task panic优先于
  normal reaction、structured reaction fault、provider stream destructor panic和ready cancellation；caller
  catch后Application保持terminal；
- consuming shutdown先fence整个tree，丢弃pending transition/task factory，再abort并await全部task。
  `ExternalApplication::shutdown()`在调用时把唯一owner交给独立cleanup task，先cancel/join active reaction，
  再shutdown Application；waiter Drop不取消cleanup，foreign-runtime poll typed fail closed且只detach cleanup。
  CLI只在cleanup完成后ACK。task正常完成不dirty、不request、不react；普通Result由Component future自己处理。
  `use_resource`/`use_action`保持deferred。

public gate覆盖direct与canonical-qualified authoring、non-Clone props compile failure、mixed hook panic、System
contract panic和typed spawn failure。runtime gate覆盖bootstrap start、same-mount retention、bounded FIFO、nested
spawn、mount race、unmount abort + await、cancellation-safe retirement、同ID 512-generation与distinct-ID
current-cardinality bookkeeping、runtime shutdown/migration、production shutdown barrier、immediate panic unwind与
outer driver precedence。

实现方最新验证：`cargo test --all-features --no-fail-fast`全量通过，library 475/475、CLI 20 passed和1项
既有ignored，其余integration/trybuild suites无失败；FDR-036 task supervisor 14/14、async-task authoring 5/5，
FDR-037 Application 52/52且独立stress 50/50；External focused 30/30、实现方production竞态复跑50/50和
FDR-040 migration 2/2也通过，reviewer另对FDR-041复跑100/100。
CLI合法4 MiB act的完整response deadline由5秒修正为15秒，同时保留200 ms connect、1秒authentication和1秒
server write timeout；原flaky用例在4路并发且包含单核约束的stress复跑中4/4通过（`src/bin/agentview.rs:37`）。
`cargo check --all-targets --all-features`、不带waiver的strict Clippy、`cargo fmt --all -- --check`和
`git diff --check`通过。FDR-035至FDR-041及完整Phase 8 surface已获reviewer签收；Phase 8 gate关闭，后续进入
Phase 9 public API与examples migration。
