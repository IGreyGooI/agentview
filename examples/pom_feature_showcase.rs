//! Complete, offline Prompt Object Model showcase.
//!
//! This example is an executable specification for the pipeline:
//!
//! ```text
//! typed Rust view
//!     -> complete authored POM
//!     -> role-specific resolution
//!     -> resolved full/delta POM
//!     -> canonical Markdown + XML prompt
//! ```
//!
//! Run it with:
//!
//! ```bash
//! cargo run --example pom_feature_showcase
//! ```
//!
//! Add `--diagnostic-json` to print the complete authored and resolved POM AST
//! for every stage. The default output keeps the same pipeline visible through
//! compact AST summaries plus the exact provider prompts.
//!
//! The derive macro covers the normal authoring path. `RichMarkdownView` is the
//! one deliberately small low-level adapter in this file: strong text, fenced
//! code blocks, thematic breaks, and multi-block list items are typed POM
//! primitives that do not yet have derive field modes. It builds AST nodes; it
//! never serializes prompt markup by hand.
//!
//! The `StreamingToolRunner` section is retained as the legacy parser/runtime
//! reference. It is not the final POM Component authoring API; see
//! `pom_streaming_channels` and `pom_mount_plan` for the isolated P3 spikes.

use std::collections::BTreeMap;
use std::fmt;

use agentview::prelude::*;
use serde::Serialize;

// ── Scalar and streaming-tool roots ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, AgentView)]
#[agent_view(display)]
struct Revision(u64);

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "r{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, AgentView)]
#[agent_view(display)]
struct Transport(&'static str);

impl fmt::Display for Transport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "tool")]
struct InspectEdgeTool {
    #[view(attr)]
    name: &'static str,

    #[view(attr)]
    edge_id: &'static str,

    #[view(element)]
    purpose: &'static str,
}

fn inspect_edge_tool() -> InspectEdgeTool {
    InspectEdgeTool {
        name: "inspect_edge",
        edge_id: "...",
        purpose: "Read one semantic edge before deciding whether to update it.",
    }
}

#[derive(Default)]
struct ShowcaseParseContext {
    raw_output: String,
    artifacts: Vec<TurnArtifact>,
    inspected_edge_ids: Vec<String>,
}

impl ParseContext for ShowcaseParseContext {
    fn raw_output(&self) -> &str {
        &self.raw_output
    }

    fn set_raw_output(&mut self, output: String) {
        self.raw_output = output;
    }

    fn add_artifact(&mut self, artifact: TurnArtifact) {
        self.artifacts.push(artifact);
    }

    fn artifacts(&self) -> &[TurnArtifact] {
        &self.artifacts
    }
}

#[async_trait::async_trait]
impl StreamingTool<ShowcaseParseContext> for InspectEdgeTool {
    async fn on_open(
        &mut self,
        element: &XmlElement,
        context: &mut ShowcaseParseContext,
    ) -> Result<(), StreamingToolError> {
        let edge_id =
            element
                .attr("edge_id")
                .ok_or_else(|| StreamingToolError::InvalidAttribute {
                    tag: "inspect_edge",
                    attr: "edge_id",
                    reason: "required by the derived contract".to_owned(),
                })?;
        context.inspected_edge_ids.push(edge_id.to_owned());
        Ok(())
    }
}

// ── Markdown roots ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct ToolInstructionView {
    #[view(text)]
    before: &'static str,

    #[view(code_span)]
    invariant: &'static str,

    #[view(text)]
    middle: &'static str,

    #[view(xml)]
    call: InspectEdgeTool,

    #[view(text)]
    after: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct RuleItemView {
    #[view(text)]
    before: &'static str,

    #[view(code_span)]
    term: &'static str,

    #[view(text)]
    after: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(markdown = "paragraph")]
struct TaskParagraphView {
    #[view(text)]
    before: &'static str,

    #[view(code_span)]
    target: &'static str,

    #[view(text)]
    after: &'static str,
}

/// Covers POM Markdown primitives that intentionally have no derive field mode.
#[derive(Debug, Clone)]
struct RichMarkdownView {
    inline_tool: InspectEdgeTool,
}

impl AgentView for RichMarkdownView {
    type Root = Document;

    fn build_root(&self) -> Result<Self::Root, PomError> {
        Document::try_build(|blocks| {
            blocks.try_paragraph(|inline| {
                inline.try_text("The renderer preserves ")?;
                inline.try_strong(|strong| strong.try_text("semantic structure"))?;
                inline.try_text(", literal ")?;
                inline.code_span(TextNode::new("inline code"));
                inline.try_text(", and typed inline XML such as ")?;
                inline.xml(self.inline_tool.build_root()?);
                inline.try_text(".")
            })?;
            blocks.code_block(
                Some("xml".into()),
                TextNode::new("<inspect_edge edge_id=\"edge.1\" />"),
            );
            blocks.thematic_break();
            blocks.try_list(ListKind::Ordered { start: 7 }, |list| {
                list.try_item(|item| {
                    item.try_paragraph(|inline| {
                        inline.try_text("One list item may own more than one block.")
                    })?;
                    item.code_block(
                        Some("text".into()),
                        TextNode::new("second block in the same list item"),
                    );
                    Ok(())
                })
            })?;
            Ok(())
        })
    }
}

// ── System document ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "limit")]
struct LimitView {
    name: &'static str,
    value: u8,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "rule")]
struct ContractRuleView {
    #[view(attr, name = "id")]
    rule_id: &'static str,

    #[view(text)]
    instruction: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "limits")]
struct ContractLimitsView {
    max_tool_calls: u8,
    strict: bool,
}

#[derive(Debug, Clone, AgentView)]
struct InferredKindView {
    source: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "response_contract")]
struct ResponseContractView {
    transport: Transport,

    #[view(attr, name = "schema_revision")]
    revision: Revision,

    optional_note: Option<&'static str>,

    #[view(element)]
    instruction: &'static str,

    #[view(code_span)]
    grammar: &'static str,

    limits: ContractLimitsView,

    metadata: BTreeMap<&'static str, &'static str>,

    notes: Vec<&'static str>,

    #[view(flatten)]
    rules: Vec<ContractRuleView>,

    #[view(root)]
    inspect_edge: InspectEdgeTool,

    #[view(root)]
    root_limits: Vec<LimitView>,

    #[view(root)]
    root_metadata: BTreeMap<&'static str, &'static str>,

    #[view(root)]
    inferred_kind: InferredKindView,

    #[view(skip)]
    _runtime_handler_id: usize,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "edge_guide")]
struct EdgeGuideView {
    id: &'static str,

    #[view(element)]
    meaning: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "workspace_guide")]
struct WorkspaceGuideView {
    session_id: &'static str,

    #[view(element)]
    purpose: &'static str,

    #[view(diff)]
    active_edge: EdgeGuideView,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct ShowcaseSystemDocument {
    #[view(heading = 1)]
    title: &'static str,

    #[view(paragraph)]
    purpose: &'static str,

    #[view(block)]
    inline_instruction: ToolInstructionView,

    #[view(heading = 2)]
    workflow_title: &'static str,

    #[view(ordered_list)]
    workflow: Vec<ToolInstructionView>,

    #[view(heading = 2)]
    guarantees_title: &'static str,

    #[view(unordered_list)]
    guarantees: Vec<RuleItemView>,

    #[view(heading = 3)]
    typed_primitives_title: &'static str,

    #[view(block)]
    typed_primitives: RichMarkdownView,

    #[view(xml)]
    response_contract: ResponseContractView,

    // System resolution sees the DiffSlot but silently materializes it.
    #[view(name = "workspace_session", diff)]
    workspace_guide: WorkspaceGuideView,

    // An absent system slot is silently omitted.
    #[view(name = "retired_workspace", diff)]
    retired_workspace: Option<WorkspaceGuideView>,

    #[view(skip)]
    _runtime_model: &'static str,
}

fn system_view() -> ShowcaseSystemDocument {
    ShowcaseSystemDocument {
        title: "AgentView Prompt Object Model feature showcase",
        purpose: "Author one typed tree, then let role resolution and the canonical renderer decide the final prompt.",
        inline_instruction: ToolInstructionView {
            before: "Keep ",
            invariant: "struct -> POM -> prompt",
            middle: " intact; invoke ",
            call: inspect_edge_tool(),
            after: " as typed XML rather than interpolated markup.",
        },
        workflow_title: "Resolution workflow",
        workflow: vec![
            ToolInstructionView {
                before: "Build ",
                invariant: "complete current state",
                middle: " before calling ",
                call: inspect_edge_tool(),
                after: ".",
            },
            ToolInstructionView {
                before: "Compare only ",
                invariant: "DiffSlot",
                middle: " boundaries after ",
                call: inspect_edge_tool(),
                after: " has identified the edge.",
            },
        ],
        guarantees_title: "Authoring guarantees",
        guarantees: vec![
            RuleItemView {
                before: "Dynamic values remain ",
                term: "TextNode",
                after: " data and are escaped once.",
            },
            RuleItemView {
                before: "System and user stay separate ",
                term: "Document",
                after: " roots.",
            },
        ],
        typed_primitives_title: "Builder-only typed Markdown primitives",
        typed_primitives: RichMarkdownView {
            inline_tool: inspect_edge_tool(),
        },
        response_contract: ResponseContractView {
            transport: Transport("markdown+xml"),
            revision: Revision(7),
            optional_note: Some("No raw prompt fragments"),
            instruction: "Return typed tool calls followed by a concise explanation.",
            grammar: "<tool name=\"inspect_edge\" edge_id=\"...\">",
            limits: ContractLimitsView {
                max_tool_calls: 3,
                strict: true,
            },
            metadata: BTreeMap::from([
                ("dialect", "hermes"),
                ("renderer", "canonical"),
            ]),
            notes: vec!["XML islands are structural.", "Markdown remains readable."],
            rules: vec![
                ContractRuleView {
                    rule_id: "typed",
                    instruction: "Use the derived contract as parser identity.",
                },
                ContractRuleView {
                    rule_id: "ordered",
                    instruction: "Preserve authored child order.",
                },
            ],
            inspect_edge: inspect_edge_tool(),
            root_limits: vec![
                LimitView {
                    name: "selection",
                    value: 1,
                },
                LimitView {
                    name: "updates",
                    value: 3,
                },
            ],
            root_metadata: BTreeMap::from([("root_collection", "map")]),
            inferred_kind: InferredKindView {
                source: "container-name-inference",
            },
            _runtime_handler_id: 41,
        },
        workspace_guide: WorkspaceGuideView {
            session_id: "workspace.42",
            purpose: "Explain the active semantic edge once in context.",
            active_edge: EdgeGuideView {
                id: "edge.ownership",
                meaning: "Connect a semantic node to the owner responsible for it.",
            },
        },
        retired_workspace: None,
        _runtime_model: "not prompt-facing",
    }
}

// ── User context and every XML diff strategy ─────────────────────────────────

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "stable_note")]
struct StableNoteView {
    #[view(text)]
    text: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "focus")]
struct FocusView {
    id: &'static str,

    #[view(diff)]
    summary: &'static str,

    #[view(diff)]
    rationale: Option<&'static str>,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "plan_step")]
struct PlanStepView {
    n: u8,

    #[view(text)]
    instruction: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "plan")]
struct PlanView {
    revision: Revision,

    #[view(flatten)]
    steps: Vec<PlanStepView>,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "observation")]
struct ObservationView {
    id: &'static str,

    #[view(element)]
    detail: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "event")]
struct TimelineEventView {
    id: &'static str,

    #[view(text)]
    description: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "agent")]
struct AgentStatusView {
    id: &'static str,
    role: &'static str,

    #[view(element)]
    status: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "workspace_state")]
struct WorkspaceStateView {
    session_id: &'static str,
    schema_revision: Revision,

    #[view(element)]
    objective: &'static str,

    #[view(root)]
    stable_note: StableNoteView,

    #[view(name = "phase", diff)]
    current_phase: &'static str,

    // A DiffSlot value may itself contain Markdown.
    #[view(code_span, diff)]
    next_command: &'static str,

    #[view(diff)]
    focus: FocusView,

    #[view(diff(replace))]
    plan: PlanView,

    #[view(diff(append))]
    observations: Vec<ObservationView>,

    #[view(diff(seq))]
    timeline: Vec<TimelineEventView>,

    #[view(diff(set))]
    capabilities: Vec<&'static str>,

    #[view(diff(key = "id"))]
    agents: Vec<AgentStatusView>,

    // BTreeMap uses intrinsic map-key identity under recursive diff.
    #[view(diff)]
    facts: BTreeMap<&'static str, &'static str>,

    #[view(diff)]
    transient_hint: Option<&'static str>,

    #[view(skip)]
    _runtime_handle: usize,
}

fn first_context() -> WorkspaceStateView {
    WorkspaceStateView {
        session_id: "session.42",
        schema_revision: Revision(7),
        objective: "Upgrade the semantic tree without losing edge meaning.",
        stable_note: StableNoteView {
            text: "This unmarked node is identical in every captured state.",
        },
        current_phase: "observe",
        next_command: "inspect edge.ownership",
        focus: FocusView {
            id: "focus.1",
            summary: "Find the owner edge.",
            rationale: Some("Ownership is the first ambiguous relationship."),
        },
        plan: PlanView {
            revision: Revision(1),
            steps: vec![
                PlanStepView {
                    n: 1,
                    instruction: "Inspect the edge.",
                },
                PlanStepView {
                    n: 2,
                    instruction: "Compare both endpoints.",
                },
            ],
        },
        observations: vec![ObservationView {
            id: "obs.1",
            detail: "The node exists, but its edge guide was absent.",
        }],
        timeline: vec![
            TimelineEventView {
                id: "event.1",
                description: "Captured the complete typed state.",
            },
            TimelineEventView {
                id: "event.2",
                description: "Queued an edge inspection.",
            },
        ],
        capabilities: vec!["read", "legacy"],
        agents: vec![
            AgentStatusView {
                id: "agent.a",
                role: "planner",
                status: "waiting",
            },
            AgentStatusView {
                id: "agent.b",
                role: "reviewer",
                status: "ready",
            },
        ],
        facts: BTreeMap::from([("mood", "calm"), ("obsolete", "yes")]),
        transient_hint: Some("Prefer the owner with an explicit workspace role."),
        _runtime_handle: 1001,
    }
}

fn second_context() -> WorkspaceStateView {
    WorkspaceStateView {
        session_id: "session.42",
        schema_revision: Revision(7),
        objective: "Upgrade the semantic tree without losing edge meaning.",
        stable_note: StableNoteView {
            text: "This unmarked node is identical in every captured state.",
        },
        current_phase: "execute",
        next_command: "apply edge.owner-v2",
        focus: FocusView {
            id: "focus.1",
            summary: "Attach the verified owner edge.",
            rationale: None,
        },
        plan: PlanView {
            revision: Revision(2),
            steps: vec![
                PlanStepView {
                    n: 1,
                    instruction: "Insert the verified edge.",
                },
                PlanStepView {
                    n: 2,
                    instruction: "Render one delta prompt.",
                },
            ],
        },
        observations: vec![
            ObservationView {
                id: "obs.1",
                detail: "The node exists, but its edge guide was absent.",
            },
            ObservationView {
                id: "obs.2",
                detail: "The workspace owner is agent.a.",
            },
        ],
        timeline: vec![TimelineEventView {
            id: "event.1",
            description: "Captured the complete typed state.",
        }],
        capabilities: vec!["read", "urgent"],
        agents: vec![
            AgentStatusView {
                id: "agent.a",
                role: "planner",
                status: "executing",
            },
            AgentStatusView {
                id: "agent.c",
                role: "operator",
                status: "ready",
            },
        ],
        facts: BTreeMap::from([("mood", "tense"), ("new", "verified")]),
        transient_hint: None,
        _runtime_handle: 2002,
    }
}

// ── Ephemeral artifacts and user document ────────────────────────────────────

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "parser_error")]
struct ParserErrorArtifactView {
    code: &'static str,

    #[view(text)]
    message: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "artifact_detail")]
struct ArtifactDetailView {
    severity: &'static str,

    #[view(text)]
    message: &'static str,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(document)]
struct CompositeArtifactDocument {
    #[view(heading = 3)]
    title: &'static str,

    #[view(block)]
    explanation: RuleItemView,

    #[view(xml)]
    detail: ArtifactDetailView,
}

#[derive(Debug, Clone, AgentView)]
#[agent_view(kind = "turn_reply")]
struct TurnReplyContractView {
    transport: Transport,

    #[view(element)]
    instruction: &'static str,

    #[view(root)]
    inspect_edge: InspectEdgeTool,
}

#[derive(Debug, AgentView)]
#[agent_view(document)]
struct ShowcaseUserDocument {
    #[view(heading = 2)]
    turn_title: &'static str,

    #[view(name = "agent_context", diff)]
    context: Option<WorkspaceStateView>,

    #[view(block)]
    artifacts: Vec<TurnArtifact>,

    #[view(block)]
    retry_note: Option<RuleItemView>,

    #[view(block)]
    task: TaskParagraphView,

    #[view(xml)]
    reply_contract: TurnReplyContractView,
}

fn parser_error_artifact() -> Result<TurnArtifact, TurnArtifactError> {
    TurnArtifact::try_from_view(&ParserErrorArtifactView {
        code: "missing_owner",
        message: "The first streamed call omitted edge_id=\"edge.ownership\".",
    })
}

fn composite_artifact() -> Result<TurnArtifact, TurnArtifactError> {
    let document = CompositeArtifactDocument {
        title: "Resolver feedback",
        explanation: RuleItemView {
            before: "The previous call violated ",
            term: "edge_id",
            after: "; retry with the typed contract.",
        },
        detail: ArtifactDetailView {
            severity: "recoverable",
            message: "Artifacts are complete POM blocks and never enter the diff cursor.",
        },
    }
    .build_root()?;
    TurnArtifact::try_from_document("resolver_feedback", document)
}

fn first_user_view() -> ShowcaseUserDocument {
    ShowcaseUserDocument {
        turn_title: "Turn 1 — full context",
        context: Some(first_context()),
        artifacts: Vec::new(),
        retry_note: None,
        task: TaskParagraphView {
            before: "Inspect ",
            target: "edge.ownership",
            after: " and identify its owner.",
        },
        reply_contract: TurnReplyContractView {
            transport: Transport("xml"),
            instruction: "Use the typed tool contract before explaining the result.",
            inspect_edge: inspect_edge_tool(),
        },
    }
}

fn second_user_view() -> Result<ShowcaseUserDocument, TurnArtifactError> {
    Ok(ShowcaseUserDocument {
        turn_title: "Turn 2 — semantic delta",
        context: Some(second_context()),
        artifacts: vec![parser_error_artifact()?, composite_artifact()?],
        retry_note: Some(RuleItemView {
            before: "Retry only the ",
            term: "inspect_edge",
            after: " call that failed validation.",
        }),
        task: TaskParagraphView {
            before: "Apply ",
            target: "edge.owner-v2",
            after: " using the delta context above.",
        },
        reply_contract: TurnReplyContractView {
            transport: Transport("xml"),
            instruction: "Use the typed tool contract before explaining the result.",
            inspect_edge: inspect_edge_tool(),
        },
    })
}

fn deletion_user_view() -> ShowcaseUserDocument {
    ShowcaseUserDocument {
        turn_title: "Turn 4 — context deletion",
        context: None,
        artifacts: Vec::new(),
        retry_note: None,
        task: TaskParagraphView {
            before: "Acknowledge that ",
            target: "agent_context",
            after: " is no longer active.",
        },
        reply_contract: TurnReplyContractView {
            transport: Transport("xml"),
            instruction: "Use the typed tool contract before explaining the result.",
            inspect_edge: inspect_edge_tool(),
        },
    }
}

fn unchanged_user_view() -> ShowcaseUserDocument {
    ShowcaseUserDocument {
        turn_title: "Turn 3 — unchanged context",
        context: Some(second_context()),
        artifacts: Vec::new(),
        retry_note: None,
        task: TaskParagraphView {
            before: "Continue with ",
            target: "edge.owner-v2",
            after: "; the unchanged context should not be sent again.",
        },
        reply_contract: TurnReplyContractView {
            transport: Transport("xml"),
            instruction: "Use the typed tool contract before explaining the result.",
            inspect_edge: inspect_edge_tool(),
        },
    }
}

// ── Pipeline and executable output ───────────────────────────────────────────

fn print_json(label: &str, value: &impl Serialize) -> anyhow::Result<()> {
    println!("\n=== {label} ===\n");
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn outer_slot_summary(document: &Document) -> String {
    let slots = document
        .children()
        .iter()
        .filter_map(|edge| match edge {
            ContentRef::DiffSlot(slot) => Some(format!(
                "{}:{:?}:{}",
                slot.role(),
                slot.strategy(),
                if slot.is_present() {
                    "present"
                } else {
                    "absent"
                }
            )),
            ContentRef::Node(_) => None,
        })
        .collect::<Vec<_>>();
    if slots.is_empty() {
        "none".to_owned()
    } else {
        slots.join(", ")
    }
}

fn print_authored_pom(
    label: &str,
    document: &Document,
    diagnostic_json: bool,
) -> anyhow::Result<()> {
    let title = format!("{label}: AUTHORED POM AST (Document, DiffSlots retained)");
    if diagnostic_json {
        print_json(&title, document)
    } else {
        println!("\n=== {title} ===\n");
        println!(
            "Document {{ block_edges: {}, outer_diff_slots: [{}] }}",
            document.children().len(),
            outer_slot_summary(document)
        );
        Ok(())
    }
}

fn print_resolved_pom(
    label: &str,
    document: &ResolvedDocument,
    diagnostic_json: bool,
) -> anyhow::Result<()> {
    let title = format!("{label}: RESOLVED POM AST (slot-free)");
    if diagnostic_json {
        print_json(&title, document)
    } else {
        println!("\n=== {title} ===\n");
        println!(
            "ResolvedDocument {{ block_edges: {} }}",
            document.children().len()
        );
        Ok(())
    }
}

fn print_candidate_cursor(
    label: &str,
    cursor: &UserDocumentCursor,
    diagnostic_json: bool,
) -> anyhow::Result<()> {
    let title = format!("{label}: CANDIDATE USER CURSOR (not yet committed)");
    if diagnostic_json {
        print_json(&title, cursor)
    } else {
        println!("\n=== {title} ===\n");
        println!("UserDocumentCursor {{ slot_baselines: {} }}", cursor.len());
        Ok(())
    }
}

fn print_prompt(label: &str, prompt: &str) {
    println!("\n=== {label}: CANONICAL PROVIDER PROMPT ===\n");
    println!("{prompt}");
}

#[cfg(test)]
fn resolve_and_render_system(document: Document) -> anyhow::Result<String> {
    let resolved = resolve_system_document(document);
    Ok(render_pom_document(&resolved)?)
}

fn resolve_and_render_user(
    document: Document,
    cursor: &UserDocumentCursor,
) -> anyhow::Result<(ResolvedDocument, String, UserDocumentCursor)> {
    let (resolved, candidate_cursor) = resolve_user_document(document, cursor)?;
    let prompt = render_pom_document(&resolved)?;
    Ok((resolved, prompt, candidate_cursor))
}

fn emit_user_stage(
    label: &str,
    document: Document,
    cursor: &UserDocumentCursor,
    diagnostic_json: bool,
) -> anyhow::Result<UserDocumentCursor> {
    print_authored_pom(label, &document, diagnostic_json)?;
    let (resolved, prompt, candidate_cursor) = resolve_and_render_user(document, cursor)?;
    print_resolved_pom(label, &resolved, diagnostic_json)?;
    print_candidate_cursor(label, &candidate_cursor, diagnostic_json)?;
    print_prompt(label, &prompt);

    // A real Agent publishes this candidate only after its turn commit
    // boundary. The offline example treats each stage as a successful commit.
    Ok(candidate_cursor)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let diagnostic_json = std::env::args()
        .skip(1)
        .any(|arg| arg == "--diagnostic-json");

    // The same derived XML root is both prompt schema and streaming-parser
    // registration identity (`<tool name="inspect_edge">`).
    let mut runner =
        StreamingToolRunner::new(ShowcaseParseContext::default()).with_tool(inspect_edge_tool());
    runner
        .feed(r#"<inspect_edge edge_id="edge.ownership" />"#)
        .await;
    runner.finalize().await;
    let parser_context = runner.into_context().await;
    assert_eq!(parser_context.inspected_edge_ids, ["edge.ownership"]);

    let system = system_view().build_root()?;
    print_authored_pom("SYSTEM", &system, diagnostic_json)?;
    let resolved_system = resolve_system_document(system);
    print_resolved_pom("SYSTEM", &resolved_system, diagnostic_json)?;
    print_prompt("SYSTEM", &render_pom_document(&resolved_system)?);

    let mut cursor = UserDocumentCursor::default();
    cursor = emit_user_stage(
        "USER TURN 1 / CONTEXT FIRST-SEEN / FULL",
        first_user_view().build_root()?,
        &cursor,
        diagnostic_json,
    )?;
    cursor = emit_user_stage(
        "USER TURN 2 / CONTEXT CHANGED / DELTA",
        second_user_view()?.build_root()?,
        &cursor,
        diagnostic_json,
    )?;
    cursor = emit_user_stage(
        "USER TURN 3 / CONTEXT UNCHANGED / SLOT OMITTED",
        unchanged_user_view().build_root()?,
        &cursor,
        diagnostic_json,
    )?;
    cursor = emit_user_stage(
        "USER TURN 4 / CONTEXT EXPLICITLY ABSENT / DELETE",
        deletion_user_view().build_root()?,
        &cursor,
        diagnostic_json,
    )?;

    assert!(
        cursor.is_empty(),
        "the explicit outer context deletion removes its committed baseline"
    );
    println!(
        "\n=== STREAMING TOOL DISPATCH ===\n\ncontract identity `inspect_edge` dispatched edge ids: {:?}",
        parser_context.inspected_edge_ids
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(value: &'static str) -> &'static str {
        value.trim_end_matches('\n')
    }

    #[test]
    fn system_prompt_matches_the_canonical_golden() {
        let prompt = resolve_and_render_system(system_view().build_root().unwrap()).unwrap();
        assert_eq!(
            prompt,
            expected(include_str!("pom_feature_showcase/system_prompt.golden.md"))
        );
        assert!(!prompt.contains("rendering_mode="));
        assert!(!prompt.contains("runtime_handler_id"));
        assert!(!prompt.contains("runtime_model"));
    }

    #[test]
    fn user_full_delta_unchanged_and_delete_prompts_match_their_goldens() {
        let cursor = UserDocumentCursor::default();
        let (_, full, cursor) =
            resolve_and_render_user(first_user_view().build_root().unwrap(), &cursor).unwrap();
        assert_eq!(
            full,
            expected(include_str!(
                "pom_feature_showcase/user_turn_1_full.golden.md"
            ))
        );

        let (_, delta, cursor) =
            resolve_and_render_user(second_user_view().unwrap().build_root().unwrap(), &cursor)
                .unwrap();
        assert_eq!(
            delta,
            expected(include_str!(
                "pom_feature_showcase/user_turn_2_delta.golden.md"
            ))
        );

        let (_, unchanged, cursor) =
            resolve_and_render_user(unchanged_user_view().build_root().unwrap(), &cursor).unwrap();
        assert_eq!(
            unchanged,
            expected(include_str!(
                "pom_feature_showcase/user_turn_3_unchanged.golden.md"
            ))
        );
        assert!(!unchanged.contains("<agent_context"));

        let (_, deletion, cursor) =
            resolve_and_render_user(deletion_user_view().build_root().unwrap(), &cursor).unwrap();
        assert_eq!(
            deletion,
            expected(include_str!(
                "pom_feature_showcase/user_turn_4_delete.golden.md"
            ))
        );
        assert!(cursor.is_empty());
    }

    #[test]
    fn second_turn_contains_every_diff_operation_shape() {
        let cursor = UserDocumentCursor::default();
        let (_, _, cursor) =
            resolve_and_render_user(first_user_view().build_root().unwrap(), &cursor).unwrap();
        let (_, delta, _) =
            resolve_and_render_user(second_user_view().unwrap().build_root().unwrap(), &cursor)
                .unwrap();

        for expected_fragment in [
            "<phase>execute</phase>",
            "<next_command>`apply edge.owner-v2`</next_command>",
            "<replace>",
            "<observations rendering_mode=\"delta\">\n    <insert>",
            "<timeline rendering_mode=\"delta\">\n    <remove>",
            "<capabilities rendering_mode=\"delta\">\n    <insert>",
            "<agents rendering_mode=\"delta\">\n    <insert>",
            "<remove>",
            "<update>",
            "<facts rendering_mode=\"delta\">\n    <insert>",
            "<transient_hint rendering_mode=\"delta\">\n    <none />",
            "<parser_error code=\"missing_owner\">",
            "### Resolver feedback",
        ] {
            assert!(
                delta.contains(expected_fragment),
                "missing `{expected_fragment}` in:\n{delta}"
            );
        }
    }

    #[tokio::test]
    async fn derived_tool_contract_is_also_the_parser_dispatch_identity() {
        let mut runner = StreamingToolRunner::new(ShowcaseParseContext::default())
            .with_tool(inspect_edge_tool());
        runner.feed(r#"<inspect_edge edge_id="edge.test" />"#).await;
        runner.finalize().await;

        assert_eq!(
            runner.into_context().await.inspected_edge_ids,
            ["edge.test"]
        );
    }
}
