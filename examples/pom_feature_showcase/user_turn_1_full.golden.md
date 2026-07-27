## Turn 1 — full context

<agent_context kind="workspace_state" session_id="session.42" schema_revision="r7">
  <objective>Upgrade the semantic tree without losing edge meaning.</objective>
  <stable_note>This unmarked node is identical in every captured state.</stable_note>
  <phase>observe</phase>
  <next_command>`inspect edge.ownership`</next_command>
  <focus kind="focus" id="focus.1">
    <summary>Find the owner edge.</summary>
    <rationale>Ownership is the first ambiguous relationship.</rationale>
  </focus>
  <plan kind="plan" revision="r1">
    <plan_step n="1">Inspect the edge.</plan_step>
    <plan_step n="2">Compare both endpoints.</plan_step>
  </plan>
  <observations>
    <observation id="obs.1">
      <detail>The node exists, but its edge guide was absent.</detail>
    </observation>
  </observations>
  <timeline>
    <event id="event.1">Captured the complete typed state.</event>
    <event id="event.2">Queued an edge inspection.</event>
  </timeline>
  <capabilities>
    <item>read</item>
    <item>legacy</item>
  </capabilities>
  <agents>
    <agent id="agent.a" role="planner">
      <status>waiting</status>
    </agent>
    <agent id="agent.b" role="reviewer">
      <status>ready</status>
    </agent>
  </agents>
  <facts>
    <entry key="mood" value="calm" />
    <entry key="obsolete" value="yes" />
  </facts>
  <transient_hint>Prefer the owner with an explicit workspace role.</transient_hint>
</agent_context>

Inspect `edge.ownership` and identify its owner.

<turn_reply transport="xml">
  <instruction>Use the typed tool contract before explaining the result.</instruction>
  <tool name="inspect_edge" edge_id="...">
    <purpose>Read one semantic edge before deciding whether to update it.</purpose>
  </tool>
</turn_reply>
