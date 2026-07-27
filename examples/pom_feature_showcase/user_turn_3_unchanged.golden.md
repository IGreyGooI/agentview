## Turn 3 — unchanged context

Continue with `edge.owner-v2`; the unchanged context should not be sent again.

<turn_reply transport="xml">
  <instruction>Use the typed tool contract before explaining the result.</instruction>
  <tool name="inspect_edge" edge_id="...">
    <purpose>Read one semantic edge before deciding whether to update it.</purpose>
  </tool>
</turn_reply>
