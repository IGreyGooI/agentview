## Turn 2 — semantic delta

<agent_context rendering_mode="delta" kind="workspace_state">
  <phase>execute</phase>
  <next_command>`apply edge.owner-v2`</next_command>
  <focus rendering_mode="delta" kind="focus">
    <summary>Attach the verified owner edge.</summary>
    <rationale rendering_mode="delta">
      <none />
    </rationale>
  </focus>
  <plan rendering_mode="delta">
    <replace>
      <plan kind="plan" revision="r2">
        <plan_step n="1">Insert the verified edge.</plan_step>
        <plan_step n="2">Render one delta prompt.</plan_step>
      </plan>
    </replace>
  </plan>
  <observations rendering_mode="delta">
    <insert>
      <observation id="obs.2">
        <detail>The workspace owner is agent.a.</detail>
      </observation>
    </insert>
  </observations>
  <timeline rendering_mode="delta">
    <remove>
      <event id="event.2">Queued an edge inspection.</event>
    </remove>
  </timeline>
  <capabilities rendering_mode="delta">
    <insert>
      <item>urgent</item>
    </insert>
    <remove>
      <item>legacy</item>
    </remove>
  </capabilities>
  <agents rendering_mode="delta">
    <insert>
      <agent id="agent.c" role="operator">
        <status>ready</status>
      </agent>
    </insert>
    <remove>
      <agent id="agent.b" role="reviewer">
        <status>ready</status>
      </agent>
    </remove>
    <update>
      <agent id="agent.a" role="planner">
        <status>executing</status>
      </agent>
    </update>
  </agents>
  <facts rendering_mode="delta">
    <insert>
      <entry key="new" value="verified" />
    </insert>
    <remove>
      <entry key="obsolete" value="yes" />
    </remove>
    <update>
      <entry key="mood" value="tense" />
    </update>
  </facts>
  <transient_hint rendering_mode="delta">
    <none />
  </transient_hint>
</agent_context>

<parser_error code="missing_owner">The first streamed call omitted edge\_id="edge.ownership".</parser_error>

### Resolver feedback

The previous call violated `edge_id`; retry with the typed contract.

<artifact_detail severity="recoverable">Artifacts are complete POM blocks and never enter the diff cursor.</artifact_detail>

Retry only the `inspect_edge` call that failed validation.

Apply `edge.owner-v2` using the delta context above.

<turn_reply transport="xml">
  <instruction>Use the typed tool contract before explaining the result.</instruction>
  <tool name="inspect_edge" edge_id="...">
    <purpose>Read one semantic edge before deciding whether to update it.</purpose>
  </tool>
</turn_reply>
