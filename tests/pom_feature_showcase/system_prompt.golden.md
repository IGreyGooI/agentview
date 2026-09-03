# AgentView Prompt Object Model feature showcase

Author one typed tree, then let role resolution and the canonical renderer decide the final prompt.

Keep `struct -> POM -> prompt` intact; invoke <tool name="inspect_edge" edge_id="..."><purpose>Read one semantic edge before deciding whether to update it.</purpose></tool> as typed XML rather than interpolated markup.

## Resolution workflow

1. Build `complete current state` before calling <tool name="inspect_edge" edge_id="..."><purpose>Read one semantic edge before deciding whether to update it.</purpose></tool>.
2. Compare only `DiffSlot` boundaries after <tool name="inspect_edge" edge_id="..."><purpose>Read one semantic edge before deciding whether to update it.</purpose></tool> has identified the edge.

## Authoring guarantees

- Dynamic values remain `TextNode` data and are escaped once.
- System and user stay separate `Document` roots.

### Builder-only typed Markdown primitives

The renderer preserves **semantic structure**, literal `inline code`, and typed inline XML such as <tool name="inspect_edge" edge_id="..."><purpose>Read one semantic edge before deciding whether to update it.</purpose></tool>.

~~~ xml
<inspect_edge edge_id="edge.1" />
~~~

---

7. One list item may own more than one block.

   ~~~ text
   second block in the same list item
   ~~~

<response_contract transport="markdown+xml" schema_revision="r7" optional_note="No raw prompt fragments">
  <instruction>Return typed tool calls followed by a concise explanation.</instruction>
  <grammar>`&lt;tool name="inspect_edge" edge_id="..."&gt;`</grammar>
  <limits kind="limits" max_tool_calls="3" strict="true" />
  <metadata>
    <entry key="dialect" value="hermes" />
    <entry key="renderer" value="canonical" />
  </metadata>
  <notes>
    <item>XML islands are structural.</item>
    <item>Markdown remains readable.</item>
  </notes>
  <rule id="typed">Use the derived contract as parser identity.</rule>
  <rule id="ordered">Preserve authored child order.</rule>
  <tool name="inspect_edge" edge_id="...">
    <purpose>Read one semantic edge before deciding whether to update it.</purpose>
  </tool>
  <list>
    <limit name="selection" value="1" />
    <limit name="updates" value="3" />
  </list>
  <map>
    <entry key="root_collection" value="map" />
  </map>
  <inferred_kind_view source="container-name-inference" />
</response_contract>

<workspace_session kind="workspace_guide" session_id="workspace.42">
  <purpose>Explain the active semantic edge once in context.</purpose>
  <active_edge kind="edge_guide" id="edge.ownership">
    <meaning>Connect a semantic node to the owner responsible for it.</meaning>
  </active_edge>
</workspace_session>
