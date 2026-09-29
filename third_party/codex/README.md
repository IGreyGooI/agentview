# OpenAI Codex local compaction

Source: https://github.com/openai/codex

Revision: `eab107fed0811144b6fcb161a3f50a5865ca4c37` (2026-09-11).

The following resources are copied without changes from
`codex-rs/prompts/templates/compact/`:

- `src/provider/async_openai/local_compaction/prompt.md`
- `src/provider/async_openai/local_compaction/summary_prefix.md`

`src/provider/async_openai/local_compaction.rs` adapts the local summary request
and history reconstruction in `codex-rs/core/src/compact.rs`, including
`run_compact_task_inner_impl`, `collect_user_messages`, and
`build_compacted_history_with_limit`.

AgentView uses its existing Responses stream validator, bytes/4 estimate,
provider-private coverage and background candidate installation. Retained user
text is capped at Codex's 20,000 tokens and at one tenth of the configured model
window. It does not mutate canonical history or dispatch summary output. Failed
or oversized attempts preserve the accepted context; Codex's retry-time removal
of old history is not ported. No remote compaction capability is included.

The adapted Rust module and copied prompts are licensed under Apache-2.0. The
upstream [LICENSE](LICENSE) and [NOTICE](NOTICE) are included here. The rest of
AgentView retains its existing license.
