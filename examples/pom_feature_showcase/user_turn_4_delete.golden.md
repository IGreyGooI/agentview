## Turn 4 — context deletion

<agent_context rendering_mode="delta">
  <none />
</agent_context>

Acknowledge that `agent_context` is no longer active.

<turn_reply transport="xml">
  <instruction>Use the typed tool contract before explaining the result.</instruction>
  <tool name="inspect_edge" edge_id="...">
    <purpose>Read one semantic edge before deciding whether to update it.</purpose>
  </tool>
</turn_reply>
