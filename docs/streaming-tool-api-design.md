# AgentView Streaming Tool API Design

Date: 2026-09-07

Status: **AgentView v1 core implemented and tested; the in-repository Chess example is migrated.
Forgotten City consumer migrations and application-specific durable adapters remain external work.**

This document designs the Component API for model-authored streaming XML tools. It covers the
requirements exercised by `forgotten-city` while preserving the ownership rules in
[`engine.md`](engine.md). It does not treat provider-native function calls as XML tools.

The attempt lifecycle is shown in
[`streaming-tool-attempt.lifecycle.html`](streaming-tool-attempt.lifecycle.html). Its checked-in
source is [`streaming-tool-attempt.lifecycle.json`](streaming-tool-attempt.lifecycle.json).

## 1. Decision

Add one high-level, strict, multi-element contract API under the existing
`XmlStreamingToolCall` entry point:

```rust,ignore
XmlStreamingToolCall::new::<SelectorChannels>("forgotten-city.player-selector")
    .version("v2") // optional; the default is "v1"
    .state_with(...)
    // XmlEnvelope::strict_fragment() is the default.
    .element(...)
    .finish(...)
    .live_with(...)
    .publish_with(...)
    .on_rejected(...)
    .build()
```

`XmlStreamingToolCall::new::<C>(identity)` creates one strict contract declaration. The branch now
contains an incremental parser, typed element declarations, reducers, managed lanes, and a
reaction-owned supervisor. Each selected structured text stream is broadcast to every mounted
contract, which starts an isolated parser, state value, cardinality ledger, and effect ledger. This
document records the implemented framework boundary and the wider consumer migration criteria in
section 13. External application migrations are not implied by the AgentView implementation.

The high-level declaration owns five things for one contract in one provider reaction:

1. one prompt-producing XML fragment grammar containing its accepted top-level elements;
2. one isolated streaming parser for the selected provider text output;
3. fresh reducer state and occurrence/cardinality accounting;
4. staged `Output`, immediately interpreted but compensatable `Live`, and staged `Commit` values;
5. one terminal `Accept` or `Reject` decision after normal reaction EOF.

The reaction supervisor owns the cross-contract behavior: it broadcasts the selected input, waits
for every contract to reach a definitive result or recovery fence, runs Application
post-reconciliation once, and releases at most one coalesced successor demand. A tag name may be
declared by more than one contract because each parser is independent. Repeating a tag name inside
one contract remains a declaration fault.

Keep the two existing APIs, but narrow their roles:

- `StreamingXml::tag(...)` remains a prompt-free, permissive, raw lifecycle subscriber. It has no
  transaction, cardinality, retry, or publication guarantees.
- The existing `XmlStreamingToolCall::contract(...).empty_element(...)` chain remains a legacy
  shared-route, single-element compatibility API. It retains its existing permissive parser and
  wire syntax; it does not yet lower through the new contract worker, and is not the canonical API
  for new applications.

Do not restore the deleted `StreamingXml<E, D>` / `DurableComponent` surface. The useful semantics
from that implementation are retained as smaller layers without restoring its mounted runtime.

## 2. Scope

The API must express all four Forgotten City response shapes:

| Consumer | Grammar requirement | Runtime requirement |
| --- | --- | --- |
| Player selector | repeated self-closing element, five required/optional attributes | source order, 1..=5 policy, captured-pool validation, immediate tentative delivery, partial acceptance, retry feedback |
| Player phrase | exactly one text element | open resource, decoded text deltas, complete value, cancellation cleanup |
| NPC response | six heterogeneous, interleaved, repeated elements | cross-tag order, text streaming, open/complete actions, final cross-element validation |
| Chess move | one self-closing element with typed UCI attribute | exactly one, live preview, final validation, publication-gated output and commit |

The framework owns protocol mechanics. The application retains business authority.

AgentView owns:

- provider-neutral text selection and reaction EOF;
- per-contract XML scanning, resource limits, decoded text deltas, source spans, and source order;
- per-contract structural schema validation and typed wire decoding;
- fresh contract-attempt state, occurrence identity, cardinality accounting, and finalization;
- deterministic lane staging;
- live-effect receipt tracking, normal confirmation, reverse compensation, and recovery fencing;
- the publication boundary through an injected publisher interface;
- an Application-owned attempt supervisor and an explicit recovery driver.

The application owns:

- intent pools, legal moves, NPC knowledge, relationship rules, and other captured domain state;
- domain validation and whether a mixture of valid and invalid occurrences is accepted;
- retry budget, feedback text, and choosing the rejection action that requests a later reaction;
- concrete UI, `TextWriter`, preview, game-event, database, and outbox adapters;
- durable identifiers, idempotency, dead-letter policy, and operational recovery.

Cube Stage remains unchanged. Its Director and Script Writer use provider-native JSON tool calls,
which continue through `NativeToolCall` and the native tool lane.

## 3. Non-negotiable invariants

1. A provider reaction may mount multiple strict contracts. The Application broadcasts its selected
   structured text to every contract, and every contract parses it independently. Element names are
   scoped to a contract: the same tag in two contracts is valid, while duplicate declarations in one
   contract are an invalid Component declaration.
2. One reaction creates fresh state for every contract attempt. No count, diagnostic, decoded
   value, live receipt, or staged value crosses into another reaction or another contract.
3. Commentary/reasoning text is never parsed as an XML action stream. The default route selects the
   sole non-commentary text output and verifies it against the reaction's primary output.
4. Reaction EOF is a distinct runtime signal. `TextComplete` or `TextSealed` is not EOF.
5. All tags declared by one contract share one parser and one monotonically increasing
   contract-attempt order sequence. Parser events, diagnostics, and reducer emissions preserve
   source and dispatch order inside that contract. Different contracts do not share parser state,
   counters, business decisions, or effect ledgers.
6. Within a contract, a handler and every live effect it emits finish before the next XML event is
   dispatched.
7. Self-closing syntax produces `Open` followed immediately by `Complete` for the same occurrence.
8. `on_open` runs only after structural attribute validation and open-value decoding succeed.
   `on_open` and `on_delta` receive shared State read-only; only `on_complete`, after structural and
   complete-value decoding succeeds, may mutate it. Emissions from an element handler are
   occurrence-scoped until that occurrence completes.
9. Model contract, wire-decode, and domain violations are typed data and may be accumulated. Route,
   hard resource-limit, reducer-panic, runtime, and publisher failures are faults and never
   masquerade as model feedback.
10. Normal EOF runs cardinality checks and the final reducer exactly once. Provider failure,
    reducer panic, or cancellation does not run the normal final reducer.
11. `Output` and `Commit` stay attempt-local until accepted publication. `Live` may execute early,
    but every successful apply returns a receipt owned by the runtime.
12. Within a retained `Application`, every conclusively-not-published exit either proves no live
    effect was applied or drives all receipt rollbacks in reverse apply order. One rollback failure
    does not skip the remaining rollbacks. An indeterminate publication is neither a published nor
    a not-published exit: it retains all evidence and forbids rollback until resolved.
13. An indeterminate apply/publication or incomplete rollback/confirmation enters
    `RecoveryRequired` and fences successor work. It must be resolved through the explicit recovery
    API, never guessed into success or failure.
14. Streaming-tool rejection never rewrites canonical provider history. A corrective model retry is
    a later explicit reaction with feedback.
15. Raw `Signal` writes and arbitrary side effects performed by low-level callbacks are not
    transactional. Only the high-level lanes receive the guarantees in this design.
16. Once a side-effecting lane invocation starts, dropping the `react()` waiter cannot cancel that
    invocation. The Application-owned supervisor drives it to an outcome or retains its recovery
    evidence behind the fence.

## 4. Public type model

### 4.1 Channel contract

The channel marker keeps a contract's lane types nominal and prevents an erased runtime from
confusing `Live` with `Commit`.

```rust,ignore
pub trait StreamingToolChannels: Send + Sync + 'static {
    type Output: Send + 'static;
    type Live: Send + 'static;
    type Commit: Send + Sync + 'static;
    type Diagnostic: Send + Sync + 'static;
}

pub enum NoStreamingValue {}

pub enum StreamingToolEmission<C: StreamingToolChannels> {
    Output(C::Output),
    Live(C::Live),
    Commit(C::Commit),
    Diagnostic(C::Diagnostic),
}

#[must_use]
pub struct StreamingToolUpdate<C: StreamingToolChannels> {
    // Ordered exactly as authored by the reducer.
    entries: Vec<StreamingToolEmission<C>>,
}

impl<C: StreamingToolChannels> StreamingToolUpdate<C> {
    pub fn none() -> Self;
    pub fn output(value: C::Output) -> Self;
    pub fn live(value: C::Live) -> Self;
    pub fn commit(value: C::Commit) -> Self;
    pub fn diagnostic(value: C::Diagnostic) -> Self;

    pub fn with_output(self, value: C::Output) -> Self;
    pub fn with_live(self, value: C::Live) -> Self;
    pub fn with_commit(self, value: C::Commit) -> Self;
    pub fn with_diagnostic(self, value: C::Diagnostic) -> Self;
}
```

Each single-value constructor creates the first entry. Every consuming `with_*` call appends one
entry to the tail, so chain call order is emission order. Values need not implement `Clone`, and the
first version does not expose the backing vector or a generic untyped insertion method. Emission
order is retained even though only `Live` is interpreted immediately. The runtime stamps every
parser event, diagnostic record, and update entry from one contract-attempt-wide order counter. An
emitted entry also records the parser event that caused it:

```rust,ignore
pub enum StreamingEmissionOrigin {
    Xml {
        event_sequence: u64,
        occurrence: XmlOccurrenceId,
    },
    Finish,
}

pub struct StagedStreamingToolEmission<C: StreamingToolChannels> {
    pub sequence: u64,
    pub origin: StreamingEmissionOrigin,
    pub value: StagedStreamingToolValue<C>,
}

pub enum StagedStreamingToolValue<C: StreamingToolChannels> {
    Output(C::Output),
    Commit(C::Commit),
}
```

Updates returned by element handlers are implicitly scoped to that occurrence. If a later closing
tag, content check, or complete-value decode invalidates the occurrence, the framework removes its
staged Output/Commit entries and rolls back its Live receipts before continuing. Diagnostics remain
in the attempt report. If that rollback cannot be proven, the entire attempt stops behind the
recovery fence instead of continuing: all attempt-staged Output/Commit are discarded, every applied
receipt becomes a rollback target, and the supervisor saves a Terminal occurrence-cleanup fault.
Recovery never resumes that parser. Updates returned by `finish` have `Finish` origin and are
attempt-scoped. This rule is what makes accepting valid selector siblings alongside an invalid
sibling safe when local cleanup is definitive, without pretending an uncertain cleanup can be
partially accepted.

Framework diagnostics remain distinct from the application's diagnostic type:

```rust,ignore
#[non_exhaustive]
pub enum StreamingToolDiagnostic<D> {
    Contract(XmlContractViolation),
    Decode(XmlDecodeViolation),
    Domain(D),
}

pub enum StreamingToolDiagnosticOrigin {
    Parser {
        occurrence: Option<XmlOccurrenceId>,
        span: Option<XmlSourceSpan>,
    },
    Reducer(StreamingEmissionOrigin),
}

pub struct StreamingToolDiagnosticRecord<D> {
    pub sequence: u64,
    pub origin: StreamingToolDiagnosticOrigin,
    pub diagnostic: StreamingToolDiagnostic<D>,
}
```

An application may render these into retry feedback, log them, or accept a response that contains
some diagnostics. A diagnostic is a fact, not control flow: it does not automatically invalidate an
occurrence or reject the attempt. Decoder failure and the explicit complete validator below
invalidate an occurrence; the final reducer alone accepts or rejects the whole attempt.

### 4.2 Grammar and element contracts

One contract owns every element that is valid in its strict XML fragment grammar:

```rust,ignore
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlEnvelope {
    allow_unclosed_text_at_eof: bool, // private
}

impl XmlEnvelope {
    pub fn strict_fragment() -> Self;
    pub fn allow_unclosed_text_at_eof(self) -> Self;
    pub fn allows_unclosed_text_at_eof(&self) -> bool;
}

pub struct XmlToolElement;
pub struct SelfClosing;
pub struct TextContent;

pub struct XmlElementDraft<Form> {
    marker: PhantomData<fn() -> Form>,
}

pub struct XmlElementContract<Head, Value> {
    marker: PhantomData<fn() -> (Head, Value)>,
}

impl XmlToolElement {
    pub fn self_closing(name: &'static str) -> XmlElementDraft<SelfClosing>;
    pub fn text(name: &'static str) -> XmlElementDraft<TextContent>;
}

impl<Form> XmlElementDraft<Form> {
    pub fn required_attribute<T>(
        self,
        name: &'static str,
        prompt_example: &'static str,
    ) -> Self
    where
        T: FromStr + Send + Sync + 'static,
        T::Err: Display;

    pub fn required_attribute_with<T, E>(
        self,
        name: &'static str,
        prompt_example: &'static str,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display;

    pub fn optional_attribute<T>(
        self,
        name: &'static str,
        prompt_example: &'static str,
    ) -> Self
    where
        T: FromStr + Send + Sync + 'static,
        T::Err: Display;

    pub fn optional_attribute_with<T, E>(
        self,
        name: &'static str,
        prompt_example: &'static str,
        parse: impl Fn(&str) -> Result<T, E> + Send + Sync + 'static,
    ) -> Self
    where
        T: Send + Sync + 'static,
        E: Display;

    pub fn occurs(self, cardinality: XmlCardinality) -> Self;

    pub fn decode<Head, Value>(
        self,
        decode_open: impl Fn(XmlDecodedAttributes) -> Result<Head, XmlDecodeViolation>
            + Send
            + Sync
            + 'static,
        decode_complete: impl Fn(&Head, &str) -> Result<Value, XmlDecodeViolation>
            + Send
            + Sync
            + 'static,
    ) -> XmlElementContract<Head, Value>
    where
        Head: Send + Sync + 'static,
        Value: Send + 'static;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlCardinality {
    min: usize,         // private
    max: Option<usize>, // private, inclusive
}

impl XmlCardinality {
    pub const fn any() -> Self;
    pub const fn optional() -> Self;
    pub const fn exactly(count: usize) -> Self;
    pub const fn between(min: usize, max: usize) -> Self;
    pub const fn min(&self) -> usize;
    pub const fn max(&self) -> Option<usize>;
}

pub struct XmlDecodedAttributes { /* owned type-indexed slots */ }

impl XmlDecodedAttributes {
    pub fn take_required<T: Send + Sync + 'static>(
        &mut self,
        name: &'static str,
    ) -> Result<T, XmlDecodeViolation>;

    pub fn take_optional<T: Send + Sync + 'static>(
        &mut self,
        name: &'static str,
    ) -> Result<Option<T>, XmlDecodeViolation>;
}
```

`XmlToolElement` is re-exported from Component authoring/the crate prelude. The existing
`stream_parser::XmlElement` parsed-value type keeps its name and behavior; this new declaration
builder deliberately does not shadow it.

`XmlToolElement::self_closing` accepts only `<tag .../>`; this preserves the current compatibility
chain's wire behavior. `XmlToolElement::text` accepts a paired element containing decoded Unicode text
and rejects nested markup.
An explicit paired-empty syntax can be added later as a separate constructor without silently
widening `self_closing`. Strict fragments allow only declared top-level sibling elements and
whitespace outside them. Unknown elements, namespaces, duplicate attributes, undeclared
attributes, non-text children, and trailing text are contract violations. Registering the same
element name twice in one contract fails at mount before any provider request. The same name in two
different contracts is valid because each contract has its own parser and declaration namespace.

Strict fragment parsing is the builder default, so `.envelope(XmlEnvelope::strict_fragment())` is
usually unnecessary. `.ignore_unknown_elements()` is an opt-in, per-contract policy: it ignores an
unknown top-level element and its subtree for that contract only. This makes separately authored
contracts composable on the same reaction input without suppressing diagnostics from a contract
that intentionally remains strict.

`allow_unclosed_text_at_eof` is an explicit migration policy for the current NPC behavior. It only
permits an already-open `text` element to end at EOF. It does not make malformed opening markup,
unknown elements, or trailing content valid. On normal reaction EOF the parser emits any final
decoded delta, calls the registered complete decoder, dispatches exactly one `XmlComplete` with
`XmlElementForm::ImplicitEof`, and runs the same complete validation path as an explicit close.
Provider failure or cancellation never synthesizes completion. `strict_fragment` is the default.

Attribute declarations are the shared source for prompt examples and structural validation.
Unknown attributes are rejected by every strict element contract; there is no opt-in
`deny_unknown_attributes` switch whose default could disagree with the strict envelope. The
`between` constructor asserts `min <= max` at declaration time; an invalid declaration is never
model feedback.
Element and attribute declaration names use the same unqualified ASCII XML-name policy as
`pom::XmlName`: the first byte is an ASCII letter or `_`, and every later byte is an ASCII letter,
digit, `_`, `-`, or `.`. Each element contract has one attribute-name namespace; declaring the same
name twice is invalid regardless of required/optional mode, decoded Rust type, or parser function.
The same attribute name remains valid on two differently named elements.

These checks run while the Component attempt declaration is rendered and bound, before its prompt
document is produced and before any state, Live, publisher, or provider operation starts. The
existing public, non-exhaustive `ComponentAttemptFault` gains four structured variants:

```rust,ignore
InvalidStreamingToolElementName {
    contract: &'static str,
    element: &'static str,
    detail: String,
},
InvalidStreamingToolAttributeName {
    contract: &'static str,
    element: &'static str,
    attribute: &'static str,
    detail: String,
},
DuplicateStreamingToolElement {
    contract: &'static str,
    element: &'static str,
},
DuplicateStreamingToolAttribute {
    contract: &'static str,
    element: &'static str,
    attribute: &'static str,
},
```

`build()` therefore remains `Component`-returning. `ComponentHostFault::Attempt` preserves the
exact declaration variant; `Application::mount` and reconciliation retain the current sanitized
mapping to `ApplicationFaultKind::Terminal`, `ApplicationFaultCode::Component`, and
`ApplicationFaultReason::ComponentContract` at the actual `ApplicationFaultStage::Bootstrap` or
`ApplicationFaultStage::Reconcile`. `ApplicationFaultReason::InvalidDeclaration` is not reused
because that category belongs to the provider's `TargetDeclaration`. A duplicate attribute in model
output is different: after a valid declaration and provider submission it produces
`XmlContractViolationKind::DuplicateAttribute`, never a Component declaration fault.

`XmlDecodedAttributes` is consumed by the open decoder; `take_*` transfers each decoded value
without a `Clone` bound. A missing slot, duplicate take, or requested type mismatch is a
declaration/runtime invariant rather than model feedback. The `_with` variants cover domain wire
types that do not implement `FromStr`. The single mandatory `.decode(open, complete)` transition is
the only way to obtain an `XmlElementContract`, so `.element(...)` cannot receive a draft missing
either decoder. A later derive macro may remove repeated field names, but a derive is not required
for the first implementation.

### 4.3 Lifecycle values

Each valid element occurrence receives stable, attempt-local identity:

```rust,ignore
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamingToolAttemptId { /* opaque */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct XmlOccurrenceId { /* opaque */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmlSourceSpan { /* byte offsets into bounded raw output */ }

impl XmlSourceSpan {
    pub const fn start(&self) -> usize;
    pub const fn end(&self) -> usize;
    pub fn slice<'a>(&self, raw_output: &'a str) -> Option<&'a str>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlElementForm {
    SelfClosing,
    ExplicitClose,
    ImplicitEof,
}

pub struct XmlOpen<Head> {
    pub attempt: StreamingToolAttemptId,
    pub sequence: u64,
    pub occurrence: XmlOccurrenceId,
    pub head: Arc<Head>,
    pub span: XmlSourceSpan,
}

pub struct XmlTextDelta<Head> {
    pub attempt: StreamingToolAttemptId,
    pub sequence: u64,
    pub occurrence: XmlOccurrenceId,
    pub head: Arc<Head>,
    pub delta: String,
    pub accumulated: Arc<str>,
    pub span: XmlSourceSpan,
}

pub struct XmlComplete<Head, Value> {
    pub attempt: StreamingToolAttemptId,
    pub sequence: u64,
    pub occurrence: XmlOccurrenceId,
    pub head: Arc<Head>,
    pub value: Value,
    pub form: XmlElementForm,
    pub span: XmlSourceSpan,
}
```

The parser decodes entities and computes `delta`; applications no longer compare UTF-8 byte
lengths against cumulative snapshots. `accumulated` remains available for idempotent renderers.

`XmlSourceSpan` is a half-open `[start, end)` byte range into the exact bounded `raw_output` in the
same attempt summary. Both offsets are UTF-8 boundaries; for a framework-produced span,
`span.slice(summary.raw_output)` must return `Some` with the exact lexical source. Event spans have
the following normative meaning:

- `XmlOpen::span` covers the complete opening syntax, from `<` through `>`; for a self-closing
  element it includes the closing `/>`.
- `XmlTextDelta::span` covers the non-empty, contiguous raw content range consumed to produce that
  decoded delta. A decoded entity is attributed to its complete lexical range, such as `&amp;`, so
  decoded length need not equal span length. Delta spans for one occurrence are ordered,
  non-overlapping, and together cover exactly the raw content represented by its deltas.
- `XmlComplete::span` covers the complete closing tag for `ExplicitClose`, equals that occurrence's
  open span for `SelfClosing`, and is the zero-width `[raw_output.len(), raw_output.len())` EOF span
  for `ImplicitEof`.

Chunking may change individual text-delta boundaries and therefore individual delta spans, but not
their ordered raw coverage, decoded concatenation, opening span, completion span, or element form.

Every parser-derived item receives a sequence number, including diagnostics. Occurrence identity
is allocated when a lexically attributable opening element is seen, before value decoding. This
prevents an invalid `<move uci="bad"/>` from also producing the misleading diagnostic "missing
move" at EOF. `ImplicitEof` is produced only by the explicit lax envelope policy described above;
it reaches the same complete decoder/validator path as an explicit close, while the form remains
visible to application policy. It increments `completed` only when that validator returns Valid.

### 4.4 Shared attempt state and reducers

One contract builder accepts heterogeneous element declarations. Each element has its own typed
`Head` and `Value`, while every handler in that contract observes the same `State` and emits the
same root channels. State, reducer order, terminal decision, and managed effects never span two
contracts merely because they receive the same reaction input.

```rust,ignore
pub struct MissingCompletion;
pub struct ReadyCompletion;

pub struct XmlElementHandlers<State, C, Head, Value, Completion = MissingCompletion>
where
    C: StreamingToolChannels,
{
    marker: PhantomData<fn() -> (State, C, Head, Value, Completion)>,
}

impl<State, C, Head, Value>
    XmlElementHandlers<State, C, Head, Value, MissingCompletion>
where
    C: StreamingToolChannels,
    State: Send + 'static,
    Head: Send + Sync + 'static,
    Value: Send + 'static,
{
    pub fn on_open(
        self,
        reduce: impl Fn(&State, XmlOpen<Head>) -> StreamingToolUpdate<C>
            + Send
            + Sync
            + 'static,
    ) -> Self;

    pub fn on_delta(
        self,
        reduce: impl Fn(&State, XmlTextDelta<Head>) -> StreamingToolUpdate<C>
            + Send
            + Sync
            + 'static,
    ) -> Self;

    pub fn on_complete(
        self,
        reduce: impl Fn(&mut State, XmlComplete<Head, Value>) -> StreamingToolUpdate<C>
            + Send
            + Sync
            + 'static,
    ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion>;

    pub fn on_complete_validated(
        self,
        validate: impl Fn(
                &State,
                &XmlComplete<Head, Value>,
            ) -> XmlOccurrenceValidity<<C as StreamingToolChannels>::Diagnostic>
            + Send
            + Sync
            + 'static,
        reduce: impl Fn(&mut State, XmlComplete<Head, Value>) -> StreamingToolUpdate<C>
            + Send
            + Sync
            + 'static,
    ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion>;
}

pub enum XmlOccurrenceValidity<D> {
    Valid,
    Invalid(XmlOccurrenceRejection<D>),
}

pub struct XmlOccurrenceRejection<D> {
    diagnostics: Vec<D>,
}

impl<D> XmlOccurrenceRejection<D> {
    pub fn diagnostic(diagnostic: D) -> Self;
    pub fn with_diagnostic(self, diagnostic: D) -> Self;
    pub fn diagnostics(&self) -> &[D];
}
```

Reducers are deliberately synchronous and side-effect-free. Open/delta reducers may inspect but
cannot modify shared State; their occurrence-scoped emissions can therefore be discarded exactly
if complete decoding later invalidates the occurrence. `on_complete_validated` first runs its
validator with `&State` and a borrowed complete value. On `Invalid`, its diagnostics are recorded,
the mutable reducer is not invoked, and all earlier occurrence-scoped lanes are discarded or rolled
back. Only `Valid` invokes the reducer with `&mut State` and ownership of the complete value.
`on_complete` is the always-valid convenience form. This split prevents a domain-invalid phrase or
NPC item from mutating shared State before its earlier text Live effects are withdrawn. An async
closure holding a State borrow across an await would make cancellation and exact cleanup ambiguous;
asynchronous work belongs in the managed lane runtimes. Completion typestate requires exactly one
of `on_complete` or `on_complete_validated`; neither omission nor double registration type-checks.
Repeated `on_open` or `on_delta` calls intentionally append reducers and dispatch them in
registration order, concatenating their updates into the same occurrence scope.
Mutating decision state through interior mutability from an open/delta/validation reducer violates
the reducer contract and is outside the transactional guarantee.
Reducers do not have an infrastructure-error return channel: expected model/domain failure is a
Diagnostic, while a reducer panic follows the existing panic boundary and aborts normal
finalization.

The final reducer receives the complete bounded report:

```rust,ignore
pub struct XmlAttemptSummary<'a, D> {
    pub attempt: StreamingToolAttemptId,
    pub raw_output: &'a str,
    pub elements: &'a [XmlElementSummary],
    pub diagnostics: &'a [StreamingToolDiagnosticRecord<D>],
}

pub enum StreamingToolDecision<C: StreamingToolChannels> {
    Accept(StreamingToolUpdate<C>),
    Reject(StreamingToolRejection<<C as StreamingToolChannels>::Diagnostic>),
}

pub struct StreamingToolRejection<D> {
    // Additional domain diagnostics; no Output, Live, or Commit lane exists here.
    diagnostics: Vec<D>,
}

impl<D> StreamingToolRejection<D> {
    pub fn none() -> Self;
    pub fn diagnostic(diagnostic: D) -> Self;
    pub fn with_diagnostic(self, diagnostic: D) -> Self;
    pub fn diagnostics(&self) -> &[D];
}
```

`XmlElementSummary` exposes `seen`, `opened`, `decoded`, and `completed` counts. `seen` means a
lexically attributable declared start tag, `opened` means structural/open decoding succeeded,
`decoded` means complete-value decoding succeeded, and `completed` means complete validation was
Valid and the mutable reducer ran. Generic minimum and
maximum cardinality is enforced by the framework; cross-element rules remain in the final reducer.
Examples include "at least one thought and one speak", "share_knowledge immediately precedes the
matching speak", and "exactly one action across five alternative tags".

Maximum cardinality is checked before decoding and before an open reducer runs. Minimum
cardinality is checked at reaction EOF. Both checks use lexically attributable `seen` occurrences,
so one malformed `<move .../>` produces its specific violation rather than an additional, misleading
"missing move" violation. An open-decode failure affects only `seen`; a complete-decode failure may
also affect `opened`; explicit domain invalidation may affect `seen`, `opened`, and `decoded` but not
`completed`. The final reducer uses those counts plus State to decide whether enough valid domain
values exist.

### 4.5 Supporting public values

The snippets above use the following public, non-exhaustive support values. Their fields are either
opaque or bounded; arbitrary model text is available only through explicit raw/span accessors and
is never included in framework `Display` output.

```rust,ignore
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlContractViolationKind {
    Malformed,
    Incomplete,
    UnknownElement,
    UnknownNamespace,
    MissingAttribute,
    DuplicateAttribute,
    UnknownAttribute,
    InvalidAttribute,
    WrongElementForm,
    NestedMarkup,
    TextOutsideEnvelope,
    Cardinality,
}

pub struct XmlContractViolation {
    /* private: contract identity, optional element/attribute names, kind, bounded detail code */
}

impl XmlContractViolation {
    pub fn contract_identity(&self) -> &str;
    pub fn element_name(&self) -> Option<&str>;
    pub fn attribute_name(&self) -> Option<&str>;
    pub fn kind(&self) -> XmlContractViolationKind;
    pub fn detail_code(&self) -> &'static str;
}

pub struct XmlAttributeAccessFault { /* declaration-safe detail only */ }

#[non_exhaustive]
pub enum XmlDecodeViolation {
    Model {
        code: &'static str,
        expected: &'static str,
    },
    AttributeAccess(XmlAttributeAccessFault),
}

pub struct XmlElementSummary {
    pub contract_index: usize,
    pub name: &'static str,
    pub seen: usize,
    pub opened: usize,
    pub decoded: usize,
    pub completed: usize,
    pub implicit_eof: usize,
}

pub struct StreamingToolAttemptStart { /* attempt + bounded contract metadata */ }
pub struct StreamingToolAttemptContext { /* attempt + bounded contract metadata */ }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamingEffectId { /* opaque, attempt-local */ }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamingPublicationId { /* opaque, attempt-local */ }
pub struct LiveEffectContext { /* attempt + effect + emission origin */ }
pub struct LiveConfirmContext { /* attempt + effect */ }
pub struct LiveRollbackContext { /* attempt + effect + abort cause */ }
pub struct LiveSettlementContext { /* attempt + effect + settlement + optional rollback cause */ }
pub struct LiveRecoveryContext { /* attempt + effect + phase + optional rollback cause */ }
pub struct StreamingPublishContext { /* attempt + publication correlation */ }
pub struct StreamingPublishRecoveryContext { /* attempt + publication correlation */ }

#[non_exhaustive]
pub enum StreamingToolAbortCause {
    Rejected,
    OccurrenceInvalidated,
    ProviderFailed,
    Cancelled,
    ReducerFault,
    LiveFault,
    PublicationNotPublished,
    RuntimeFault,
}

impl StreamingToolAttemptStart {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn contract_identity(&self) -> &str;
    pub fn implementation_version(&self) -> &str;
}

impl StreamingToolAttemptContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn contract_identity(&self) -> &str;
    pub fn implementation_version(&self) -> &str;
}

impl LiveEffectContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn effect(&self) -> StreamingEffectId;
    pub fn emission_sequence(&self) -> u64;
    pub fn origin(&self) -> &StreamingEmissionOrigin;
}

impl LiveConfirmContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn effect(&self) -> StreamingEffectId;
}

impl LiveRollbackContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn effect(&self) -> StreamingEffectId;
    pub fn cause(&self) -> &StreamingToolAbortCause;
}

impl LiveSettlementContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn effect(&self) -> StreamingEffectId;
    pub fn settlement(&self) -> LiveSettlement;
    pub fn rollback_cause(&self) -> Option<&StreamingToolAbortCause>;
}

impl LiveRecoveryContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn effect(&self) -> StreamingEffectId;
    pub fn phase(&self) -> StreamingToolRecoveryPhase;
    pub fn rollback_cause(&self) -> Option<&StreamingToolAbortCause>;
}

impl StreamingPublishContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn publication(&self) -> StreamingPublicationId;
}

impl StreamingPublishRecoveryContext {
    pub fn attempt(&self) -> StreamingToolAttemptId;
    pub fn publication(&self) -> StreamingPublicationId;
}

pub struct RejectedStreamingToolAttempt<D> {
    pub context: StreamingToolAttemptContext,
    pub raw_output: String,
    pub diagnostics: Vec<StreamingToolDiagnosticRecord<D>>,
}

pub enum StreamingToolRejectionAction {
    Complete,
    RequestReaction,
}
```

`XmlDecodeViolation::Model` is retry feedback. `AttributeAccess` means the decoder requested an
undeclared/already-taken slot or the wrong Rust type and is classified as a framework declaration
fault. Hard byte/depth/count limits never use either violation type.

`StreamingToolDiagnosticRecord` is the sole source of event sequence and source origin. Its Parser
origin may have no occurrence (for outside text or EOF cardinality) and may have no span (for a
missing-at-EOF condition). `XmlContractViolation` intentionally does not duplicate sequence,
occurrence, or span; callers use the record origin and `XmlSourceSpan` getters above. All attempt,
effect, settlement, publication, and recovery context fields are private and exposed by the listed
getters, so adapters can key idempotency without depending on representation.

## 5. Managed effects and publication

### 5.1 Live effects

`Live` is for effects that must be visible before the provider response has finished: selector
options, phrase text, NPC speech, or a chess preview. It is not an arbitrary async callback.

In this section, "pure preparation" and "pre-I/O factory" mean no framework-external observable
business effect: no Signal write, actor command, database/outbox write, or business-semantic metric.
Only local construction and stable-key calculation are permitted. Diagnostic telemetry must be
idempotent and cannot participate in the business result; otherwise Err/panic could not prove the
lane operation absent or make reconstruction safe.

```rust,ignore
pub enum LiveApplyOutcome<Receipt, Error> {
    Applied(Receipt),
    NotApplied(Error),
    Indeterminate,
}

pub enum LiveSettleOutcome<Error> {
    Settled,
    NotSettled(Error),
    Indeterminate,
}

pub enum LiveApplyResolution<Receipt> {
    Applied(Receipt),
    NotApplied,
    StillIndeterminate,
}

pub enum LiveSettleResolution {
    Settled,
    NotSettled,
    StillIndeterminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveSettlement {
    Confirm,
    Rollback,
}

#[async_trait]
pub trait LiveEffectRuntime<Live>: Send + 'static {
    type Receipt: Send + Sync + 'static;
    type ApplyOperation: Send + Sync + 'static;
    type SettlementOperation: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    // Pure preparation: allocates the stable key/evidence before external I/O starts.
    fn prepare_apply(
        &mut self,
        context: &LiveEffectContext,
        effect: Live,
    ) -> Result<Self::ApplyOperation, Self::Error>;

    async fn apply(
        &mut self,
        context: &LiveEffectContext,
        operation: &Self::ApplyOperation,
    ) -> LiveApplyOutcome<Self::Receipt, Self::Error>;

    async fn resolve_apply(
        &mut self,
        context: &LiveRecoveryContext,
        operation: &Self::ApplyOperation,
    ) -> Result<LiveApplyResolution<Self::Receipt>, Self::Error>;

    // Pure preparation creates one separately retained operation per receipt.
    fn prepare_settlement(
        &mut self,
        context: &LiveSettlementContext,
        receipt: &Self::Receipt,
    ) -> Result<Self::SettlementOperation, Self::Error>;

    async fn confirm(
        &mut self,
        context: &LiveConfirmContext,
        receipt: &Self::Receipt,
        operation: &Self::SettlementOperation,
    ) -> LiveSettleOutcome<Self::Error>;

    async fn rollback(
        &mut self,
        context: &LiveRollbackContext,
        receipt: &Self::Receipt,
        operation: &Self::SettlementOperation,
    ) -> LiveSettleOutcome<Self::Error>;

    async fn resolve_settlement(
        &mut self,
        context: &LiveRecoveryContext,
        receipt: &Self::Receipt,
        operation: &Self::SettlementOperation,
    ) -> Result<LiveSettleResolution, Self::Error>;
}
```

`prepare_apply` performs no external I/O and must return immutable, panic-stable evidence containing
the stable key and all replay/recovery material. The supervisor stores that operation before it
polls `apply`, then
awaits the call before dispatching the next XML event. `Applied` stores its receipt in apply order.
`NotApplied` is a fault that proves this effect is absent, so earlier receipts can be rolled back.
`Indeterminate`, a caught adapter-future panic, or loss of the original waiter stops the attempt
while retaining the operation. After a caught adapter panic, the supervisor discards that runtime
instance and recreates one from the retained builder factory; operations and receipts must be valid
across instances produced by that factory. If `resolve_apply` later proves the effect was applied,
the returned receipt is immediately driven through rollback because an explicitly recovered
reaction is never resumed. A preparation error or panic is definitively pre-I/O and therefore
NotApplied; a panicked preparation instance is also discarded.

Accepted publication confirms receipts in apply order. Rejection or any conclusively
not-published failure
rolls them back in reverse order. `confirm` and `rollback` borrow rather than consume receipts and
their pre-stored settlement operations. A `NotSettled`, `Indeterminate`, or caught adapter panic
retains both values. Settlement continues across the remaining receipts after an individual
failure. The supervisor keeps a per-receipt state table, so one pass may retain several distinct
operations while preserving every result; a single cursor/operation slot is insufficient. Once publication
is known to have succeeded, rollback is forbidden: failed confirmations can only be confirmed or
recovered.

Each settlement operation is stored before `confirm` or `rollback` is polled. A settlement
preparation error/panic is pre-I/O: that receipt stays preparation-needed, the scan continues, and a
fresh adapter instance retries preparation during recovery. The settlement/recovery contexts expose
the original rollback cause when the intended operation is Rollback, allowing that immutable
operation to carry every value needed by a later instance.

Every Live context contains the opaque attempt identity and stable effect ID. The apply context also
contains the emission sequence and `StreamingEmissionOrigin`; settlement/recovery contexts carry
their intended phase. Implementations must make apply/confirm/rollback/recovery idempotent by effect
ID so a lost acknowledgement can be resolved safely. These framework IDs are process-local
correlation values, not durable database identities.

All side-effecting calls are owned by Application's `StreamingSupervisor` and its per-contract
workers. Once a call starts, cancelling or dropping the `react()` waiter does not drop that call.
The worker records the outcome, aborts its contract when required, and owns every receipt and
prepared operation until it is definitively settled or retained for recovery.

Consuming `Application::shutdown(self) -> Result<(), ApplicationFault>` first waits for a call
already in flight, then repeatedly drives the same recovery machinery with bounded backoff. It
returns only after every retained worker is clear, or returns the retained terminal fault.
Preparation-needed, NotSettled, Indeterminate, resolve error, or a recovery-time factory error
while retained evidence still needs an adapter all keep shutdown pending. A permanently unresolved
adapter therefore cannot discard evidence or acknowledge clean shutdown. Cancelling the shutdown
future, merely dropping `Application`, or terminating the process remains outside this in-process
guarantee. Adapters that need crash recovery must persist their own stable operation IDs.

### 5.2 Accepted publication

`Output` and `Commit` are moved together through one application-provided publication boundary:

```rust,ignore
pub struct AcceptedStreamingToolAttempt<C: StreamingToolChannels> {
    pub context: StreamingToolAttemptContext,
    pub raw_output: String,
    // Output and Commit remain interleaved in original emission order.
    pub entries: Vec<StagedStreamingToolEmission<C>>,
    pub diagnostics:
        Vec<StreamingToolDiagnosticRecord<<C as StreamingToolChannels>::Diagnostic>>,
}

#[async_trait]
pub trait StreamingToolAttemptPublisher<C: StreamingToolChannels>: Send + 'static {
    type Published: Send + 'static;
    type PublicationOperation: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    // Pure preparation: the returned operation owns the accepted attempt and stable request key.
    fn prepare(
        &mut self,
        context: &StreamingPublishContext,
        attempt: AcceptedStreamingToolAttempt<C>,
    ) -> Result<Self::PublicationOperation, Self::Error>;

    async fn publish(
        &mut self,
        context: &StreamingPublishContext,
        operation: &Self::PublicationOperation,
    ) -> StreamingPublishOutcome<Self::Published, Self::Error>;

    async fn resolve(
        &mut self,
        context: &StreamingPublishRecoveryContext,
        operation: &Self::PublicationOperation,
    ) -> Result<StreamingPublishResolution<Self::Published>, Self::Error>;
}

pub enum StreamingPublishOutcome<Published, Error> {
    Published(Published),
    NotPublished(Error),
    Indeterminate,
}

pub enum StreamingPublishResolution<Published> {
    Published(Published),
    NotPublished,
    StillIndeterminate,
}
```

For an in-memory workflow, the publisher can atomically update one authoritative `Signal` or send
one acknowledged actor command. For a durable workflow, it atomically stores the accepted outputs
and Commit payloads in an outbox. AgentView does not prescribe the store schema or deliver the
application's outbox items.

The outcomes are intentionally three-way:

- `Published` proves Output/Commit ownership transferred, after which the attempt enters
  `Confirming` and all live receipts are confirmed.
- `NotPublished` proves no publication occurred, so live receipts are rolled back.
- `Indeterminate` forbids both replay and rollback until `resolve` proves `Published` or
  `NotPublished`. `StillIndeterminate` and resolution errors retain the operation behind the fence.

A publisher preparation performs no external I/O. The supervisor stores its complete
`PublicationOperation` before polling `publish`; a preparation error or panic therefore proves
NotPublished. A publish/resolve invocation is supervisor-owned and is not cancelled with the
`react()` waiter. A panic after either future starts is treated as Indeterminate. The supervisor
discards that publisher instance, recreates one from the retained builder factory, and calls
`resolve` with the immutable operation. Publisher operations must be panic-stable and valid across
instances produced by that factory. A publisher is responsible for stable request/item
identities and idempotent durable recovery. For crash recovery, it must persist the request and
recovery evidence before returning `Indeterminate`; AgentView's attempt ID alone is not stable
across process restart.

### 5.3 Rejection

The final reducer can reject an otherwise normally completed provider reaction. Rejection:

1. accepts only `StreamingToolRejection<Diagnostic>`, so it cannot create a terminal Output, Live,
   or Commit emission;
2. discards already staged Output and Commit values;
3. rolls back all live receipts;
4. after its rollback is definitive, invokes the configured `on_rejected` handler exactly once with
   the owned raw output and diagnostics, unless cancellation arrived before the handler was claimed;
5. records a local `StreamingToolRejectionAction` without starting another provider call inline;
6. after every contract worker is definitive, lets Application run one post-cleanup reconciliation
   pass and coalesce the workers' requested successor reaction into at most one demand.

The rejection handler normally writes typed feedback into Component state and returns
`RequestReaction`. It must not call `ReactionRequest` directly: external mutation remains fenced
until every worker has cleaned up and Application has completed its post-cleanup reconciliation.
Application has the sole privileged path that converts the coalesced action into a latent demand.
`Complete` queues nothing. Retry budget and partial-acceptance policy stay in the application.

Canonical provider facts have already been admitted before Component dispatch. Rejection therefore
does not erase the assistant response or rewind the Frame/diff baseline. A "fresh retry" requiring
history rollback is deliberately unsupported; it would contradict the Engine's canonical history
contract. Provider transport retries before or around handoff remain a separate `ReactionPort`
concern.

### 5.4 Recovery fence

Recovery is an explicit Application operation rather than an implicit model retry. The public status
is reaction-scoped and reports every independently mounted contract:

```rust,ignore
#[derive(Debug, Clone)]
pub struct StreamingToolRecoveryReport {
    pub attempt: StreamingToolAttemptId,
    pub contract: &'static str,
    pub accepted: bool,
    pub settled: bool,
    pub fault: Option<ApplicationFault>,
}

#[derive(Debug, Clone)]
pub enum StreamingToolRecoveryStatus {
    NotRequired,
    InFlight {
        attempts: Box<[StreamingToolRecoveryReport]>,
    },
    StillRequired {
        attempts: Box<[StreamingToolRecoveryReport]>,
    },
    Recovered {
        attempts: Box<[StreamingToolRecoveryReport]>,
        reaction_requested: bool,
    },
}

impl<P: ReactionPort> Application<P> {
    pub async fn recover_streaming_attempt(
        &mut self,
    ) -> Result<StreamingToolRecoveryStatus, ApplicationFault>;
}
```

`NotRequired` means no worker set is retained. Every other status contains one report for each
strict contract that was mounted for the reaction. `accepted` records that contract's accepted
terminal path, while `settled` means its worker has no queued operation or retained recovery
evidence. `fault` preserves a terminal contract fault, including a non-rejection abort cause, or a
retained Application cleanup fault. A recovered NotPublished or interrupted apply must therefore
not appear as a clean accepted/settled contract merely because its effect ledger was released.

`InFlight` reports a worker command already being driven. `StillRequired` reports that a recovery
pass left at least one worker with evidence to resolve. `Recovered` is returned only after every
worker is clear and Application's post-cleanup reconciliation succeeds; `reaction_requested` is the
single coalesced demand bit computed across the reports. `Recovered` does not imply that every
contract accepted or had no terminal fault; callers inspect the reports for those facts.

Each worker owns its own prepared Live apply operation, publication operation, settlement intent,
and receipt records. Confirmation scans a worker's receipts in apply order and rollback scans them
in reverse apply order. A failed or indeterminate item does not erase later evidence. A recovery
call never restarts the provider or replays a completed contract; it only drives the retained worker
operations toward a definitive result.

Application owns the only reaction-level continuation. Once every worker is clear, it performs one
post-cleanup reconciliation pass when Component state is dirty, releases the worker set, and then
releases at most one coalesced successor demand. A post-reconcile failure remains fenced and is
preserved in the reports until a later recovery call succeeds; it does not rerun a completed worker.

Dropping the original `react()` waiter after provider handoff cancels every active worker, prevents
successor demand release, and retains any uncertain cleanup behind the fence. If a rejection handler
has not begun, cancellation prevents it from starting. Dropping a
`recover_streaming_attempt()` waiter merely detaches that observer: an enqueued worker command keeps
running, and a later recovery call reports `InFlight` or its resulting contract state.

Prepared operations and receipts are retained before their external futures are polled. A caught
adapter-future panic discards that runtime or publisher instance, recreates one from the retained
factory, and resolves the same immutable operation. If factory recreation fails or an operation
remains indeterminate, the worker stays in `StillRequired` with its evidence intact.

Each recovery pass drives retained Live and publication evidence without resuming XML parsing. A
resolved interrupted apply either proves no effect or produces a receipt that follows the contract's
stored settlement path. Publication uncertainty blocks rollback until it resolves; a NotPublished
resolution rolls back the contract's receipts, while a Published resolution confirms them. A
recovered rejection invokes its handler only if it was not already invoked, and any request is
coalesced with its siblings only after Application's post-cleanup handoff.

While the fence is active, `react()`, `ReactionRequest`, Component reconciliation/remount, and any
successor streaming attempt fail before provider submission. Read-only snapshots remain available.
The implementation adds `ApplicationFaultKind::RecoveryRequired`; this is distinct from today's
`Retryable` and `Terminal` kinds, and `recover_streaming_attempt` is the only mutating operation
allowed through it besides consuming `shutdown`. The rejection continuation runs inside the
supervisor rather than through those public entry points. A durable adapter must also refuse startup
while its own unresolved journal is present, because framework recovery IDs are not process-stable.

## 6. Complete builder shape

The public composition is:

```rust,ignore
XmlStreamingToolCall::new::<Channels>(identity)
    .version(implementation_version) // optional; defaults to "v1"
    .state_with(|start: StreamingToolAttemptStart| -> Result<State, InitError> { ... })
    // Strict fragments are the default.
    // .ignore_unknown_elements() is an opt-in per-contract compatibility policy.
    .element(
        element_contract,
        |handlers: XmlElementHandlers<State, Channels, Head, Value>| {
            handlers
                .on_open(...)
                .on_delta(...)
                .on_complete_validated(..., ...)
        },
    )
    .finish(
        |state: State,
         summary: XmlAttemptSummary<
            '_,
            <Channels as StreamingToolChannels>::Diagnostic,
         >| {
            StreamingToolDecision::<Channels>::Accept(...)
        },
    )
    .live_with(|start| Ok::<_, InitError>(LiveRuntime::new(start)))
    .publish_with(|start| Ok::<_, InitError>(Publisher::new(start)))
    .on_rejected(|report| async move {
        write_feedback(report).await?;
        Ok(StreamingToolRejectionAction::RequestReaction)
    })
    .build()
```

The builder uses nominal typestate; it does not try to infer whether an arbitrary Rust type is
inhabited:

```rust,ignore
pub struct Missing;
pub struct Ready;
pub struct NoState;

pub struct XmlStreamingAttemptBuilder<
    C: StreamingToolChannels,
    State = NoState,
    StateSlot = Missing,
    ElementSlot = Missing,
    FinishSlot = Missing,
    LiveSlot = Missing,
    PublishSlot = Missing,
> {
    marker: PhantomData<
        fn() -> (C, State, StateSlot, ElementSlot, FinishSlot, LiveSlot, PublishSlot),
    >,
}

impl XmlStreamingToolCall {
    pub fn new<C: StreamingToolChannels>(
        identity: &'static str,
    ) -> XmlStreamingAttemptBuilder<C>;
}

impl<C, Elements, Finish, Live, Publish>
    XmlStreamingAttemptBuilder<C, NoState, Missing, Elements, Finish, Live, Publish>
where
    C: StreamingToolChannels,
{
    pub fn state_with<State, Factory, InitError>(
        self,
        factory: Factory,
    ) -> XmlStreamingAttemptBuilder<C, State, Ready, Elements, Finish, Live, Publish>
    where
        State: Send + 'static,
        Factory: Fn(StreamingToolAttemptStart) -> Result<State, InitError>
            + Send
            + Sync
            + 'static,
        InitError: Error + Send + Sync + 'static,
    {
        unimplemented!()
    }
}

impl<C, State, Elements, Finish, Live, Publish>
    XmlStreamingAttemptBuilder<C, State, Ready, Elements, Finish, Live, Publish>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    pub fn element<Head, Value, Configure>(
        self,
        contract: XmlElementContract<Head, Value>,
        configure: Configure,
    ) -> XmlStreamingAttemptBuilder<C, State, Ready, Ready, Finish, Live, Publish>
    where
        Head: Send + Sync + 'static,
        Value: Send + 'static,
        Configure: FnOnce(
            XmlElementHandlers<State, C, Head, Value, MissingCompletion>,
        ) -> XmlElementHandlers<State, C, Head, Value, ReadyCompletion>,
    {
        unimplemented!()
    }
}

impl<C, State, Live, Publish>
    XmlStreamingAttemptBuilder<C, State, Ready, Ready, Missing, Live, Publish>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    pub fn finish<Reducer>(
        self,
        reducer: Reducer,
    ) -> XmlStreamingAttemptBuilder<C, State, Ready, Ready, Ready, Live, Publish>
    where
        Reducer: for<'a> Fn(
                State,
                XmlAttemptSummary<'a, <C as StreamingToolChannels>::Diagnostic>,
            ) -> StreamingToolDecision<C>
            + Send
            + Sync
            + 'static,
    {
        unimplemented!()
    }
}

impl<C, State, StateSlot, Elements, Finish, Publish>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Missing, Publish>
where
    C: StreamingToolChannels,
{
    pub fn live_with<Factory, Runtime, InitError>(
        self,
        factory: Factory,
    ) -> XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Ready, Publish>
    where
        Factory: Fn(StreamingToolAttemptStart) -> Result<Runtime, InitError>
            + Send
            + Sync
            + 'static,
        Runtime: LiveEffectRuntime<C::Live>,
        InitError: Error + Send + Sync + 'static,
    {
        unimplemented!()
    }
}

impl<C, State, StateSlot, Elements, Finish, Publish>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Missing, Publish>
where
    C: StreamingToolChannels<Live = NoStreamingValue>,
{
    pub fn without_live(
        self,
    ) -> XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Ready, Publish> {
        unimplemented!()
    }
}

impl<C, State, StateSlot, Elements, Finish, Live>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Missing>
where
    C: StreamingToolChannels,
{
    pub fn publish_with<Factory, Publisher, InitError>(
        self,
        factory: Factory,
    ) -> XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Ready>
    where
        Factory: Fn(StreamingToolAttemptStart) -> Result<Publisher, InitError>
            + Send
            + Sync
            + 'static,
        Publisher: StreamingToolAttemptPublisher<C>,
        InitError: Error + Send + Sync + 'static,
    {
        unimplemented!()
    }
}

impl<C, State, StateSlot, Elements, Finish, Live>
    XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Missing>
where
    C: StreamingToolChannels<
        Output = NoStreamingValue,
        Commit = NoStreamingValue,
    >,
{
    pub fn without_publication(
        self,
    ) -> XmlStreamingAttemptBuilder<C, State, StateSlot, Elements, Finish, Live, Ready> {
        unimplemented!()
    }
}

impl<C, State, Elements, Finish, Live, Publish>
    XmlStreamingAttemptBuilder<C, State, Ready, Elements, Finish, Live, Publish>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    pub fn version(self, version: &'static str) -> Self {
        unimplemented!()
    }

    pub fn envelope(self, envelope: XmlEnvelope) -> Self {
        unimplemented!()
    }

    pub fn ignore_unknown_elements(self) -> Self {
        unimplemented!()
    }

    pub fn on_rejected<Handler, HandlerFuture, HandlerError>(self, handler: Handler) -> Self
    where
        Handler: Fn(
                RejectedStreamingToolAttempt<
                    <C as StreamingToolChannels>::Diagnostic,
                >,
            ) -> HandlerFuture
            + Send
            + Sync
            + 'static,
        HandlerFuture:
            Future<Output = Result<StreamingToolRejectionAction, HandlerError>> + Send + 'static,
        HandlerError: Error + Send + Sync + 'static,
    {
        unimplemented!()
    }
}

// This is the only build implementation.
impl<C, State>
    XmlStreamingAttemptBuilder<C, State, Ready, Ready, Ready, Ready, Ready>
where
    C: StreamingToolChannels,
    State: Send + 'static,
{
    pub fn build(self) -> Component {
        unimplemented!()
    }
}
```

The elided factory storage is private and type-erased at `build`; the slot transitions and bounds
above describe the required slot transitions; implementation-specific runtime and publisher type
parameters are elided. Compile-fail UI tests cover missing state, zero elements, missing
finish, missing live choice, missing publication choice, and registering an element draft before
`.decode`. A disabled
lane therefore requires an explicit
`.without_live()` or `.without_publication()` call; `.build()` never installs a panic placeholder.
Live and publisher factories are retained for the attempt and may be invoked again after a caught
adapter-future panic; each recreated instance receives the same attempt start context and must
understand operations/receipts produced by its predecessor.
State is initialized before provider submission. The Live factory is invoked lazily before the
first Live preparation, and the publisher factory only after Accept reaches a real publication
barrier; unused lanes and `without_*` lanes invoke no factory. An initial factory error/panic occurs
before that lane's external I/O, selects a framework fault, and follows the normal rollback rules for
any earlier receipts. Only a recreation failure while retained evidence needs resolution returns
StillRequired and is retried by recovery/shutdown.

`without_publication` is a successful no-op publication barrier, not an absent lifecycle step. The
Output/Commit sentinel bounds prove there is nothing to publish; after Accept the supervisor enters
Confirming directly and settles every Live receipt. With no Live receipts it reaches Settled
immediately.

`on_rejected` receives `RejectedStreamingToolAttempt<Diagnostic>` containing the full bounded
attempt context, owned raw output, and the accumulated framework/domain diagnostics. It returns
`StreamingToolRejectionAction`; its supervised async error or panic becomes a terminal framework
fault after cleanup and the callback is not repeated. Omitting the optional handler is equivalent to
one that returns `Complete`. `StreamingToolAttemptStart` and every effect/publication context carry
the same attempt ID. A publisher that crosses a process boundary derives and persists its own stable
request ID; the framework attempt ID is correlation only.

An attempt Component is attempt-local and cannot be placed under `#[system_once]`. Its generated
grammar document follows the surrounding Component placement. Additional natural-language rules
remain ordinary POM siblings.

`listen_to(...)` remains available only for compatibility/permissive routes in the first version.
The strict attempt API defaults to the framework's structured primary-text route; allowing custom
strict structured routes is a later extension.

## 7. Input routing prerequisite

The public `ProviderEvent::Text(TextTurnEvent)` drops `ProviderOutputKey` and `AssistantPhase`
before streaming XML sees it. It also uses `TextComplete` as both full text and the parser's
completion signal. Strict contracts therefore use an internal structured sidecar rather than that
compatibility projection.

The implementation adds a crate-private sidecar rather than changing the public event
enum:

```rust,ignore
pub(crate) enum AdmittedTextFact {
    Delta {
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        delta: String,
    },
    Sealed {
        output: ProviderOutputKey,
        phase: Option<AssistantPhase>,
        text: String,
    },
}

pub(crate) struct AdmittedProviderFact {
    public_event: Option<ProviderEvent>,
    structured_text: Option<AdmittedTextFact>,
    tool_lane: Option<ToolLaneTicket>,
}

pub(crate) struct AdmittedReactionSummary {
    primary_text: Option<ProviderOutputKey>,
}
```

The structured route:

- ignores commentary phases;
- binds the first non-commentary output key and relies on admission's existing uniqueness rule;
- validates the complete value at `TextSealed`;
- validates the selected key against `primary_text` at reaction EOF;
- treats a reaction with no primary text as a valid empty input that can still fail grammar
  cardinality normally.

`AdmittedProviderFact` is an additive sidecar on the existing admission result, not a replacement
for public events. For a delta it carries both the current public `TextDelta` and its keyed sidecar;
for `TextSealed` it carries only the sidecar; `ReactionCompleted` still produces the existing public
`TextComplete`. This preserves every current public handler while preventing the strict parser from
using that compatibility completion event as reaction EOF.

`StreamingSupervisor` owns one worker per strict contract. It broadcasts every admitted selected
text delta or sealed value to all workers; it never shares a scanner between them. Each worker calls
`Parser::push` for deltas and uses sealing only to validate or append the final suffix. Only after
the actual fact stream reaches EOF, `ReactionCompleted` has been validated, and native tool lanes
are closed does `StreamingSupervisor::finish(summary)` call each contract's normal EOF transition.

After every worker has either settled definitively or retained recovery evidence, Application owns
the reaction-level handoff. `complete_streaming_cleanup` runs post-reconciliation once, releases
the worker set, and releases at most one coalesced reaction demand. Cancellation aborts every
worker without invoking its final reducer; once all cleanup is definitive, the Application is
reusable. Any uncertain apply, publication, confirmation, or rollback keeps its worker behind the
recovery fence until `recover_streaming_attempt` reaches a definitive result.

Existing `ProviderEvent::TEXT` handlers keep their current public event values and ordering. The
compatibility `.listen_to(events.select(ProviderEvent::TEXT))` route keeps its legacy mixed-text
semantics and does not claim commentary isolation.

## 8. Parse and dispatch semantics

### 8.1 Normal ordering

For each admitted provider fact:

1. canonical history commits the fact;
2. any public `ProviderEvent` is dispatched to existing raw handlers, preserving the legacy order;
3. the supervisor broadcasts the selected structured-text sidecar to every contract worker;
4. each worker's independent parser queues its own derived events;
5. within each contract, derived XML events run in source order and each synchronous reducer update
   is interpreted in entry order;
6. within each contract, each Live apply is awaited before the next derived event.

Native tool lanes continue to be polled while either a raw handler or a contract Live apply awaits,
so a slow streaming effect does not stall an already admitted native tool completion.

At normal reaction EOF:

1. the selected text output is verified as sealed and primary;
2. every worker independently diagnoses incomplete elements, optionally completes one permitted
   open text element with `ImplicitEof`, and emits minimum-cardinality diagnostics;
3. every contract's synchronous final reducer runs exactly once; it is not a recovery operation;
4. each `Reject` stores its report and rolls back its own Live receipts before its rejection
   continuation, while each `Accept` publishes its own ordered batch or crosses its no-op barrier;
5. known publication success and no-op barriers confirm that contract's Live receipts;
6. an unresolved apply, publish, rollback, or confirmation retains that contract's evidence behind
   the recovery fence;
7. only after all workers are definitive does Application run post-reconciliation once and release
   at most one successor demand before a normally settled `Application::react()` returns.

There is one reaction-level completion loop. It fan-outs normal EOF to the independent contract
attempts, then joins only after their definitive cleanup; it does not share a parser or invent an
EOF from `TextSealed`.

### 8.2 Contract diagnostics

| Condition class | Public representation | Runtime action |
| --- | --- | --- |
| Model XML/schema violation | `StreamingToolDiagnostic::Contract` | Accumulate; final reducer decides Accept/Reject |
| Typed wire rejection | `Decode` | Record and invalidate the occurrence before its mutable complete reducer |
| Domain diagnostic | `Domain` | Accumulate as data; only explicit `XmlOccurrenceValidity::Invalid` invalidates the occurrence |
| Route/protocol, hard limit, reducer/runtime fault | sanitized `ApplicationFault` | Abort and rollback; never model feedback |
| Unknown external side-effect result | `ApplicationFaultKind::RecoveryRequired` | Retain evidence and fence until explicit recovery |

`XmlContractViolation` is non-exhaustive and must distinguish at least:

- malformed or incomplete target element;
- missing, unknown, duplicate, or invalid attribute;
- wrong empty/text element form;
- nested markup in a text-only element;
- unknown top-level element or namespace;
- non-whitespace text outside the fragment grammar;
- minimum/maximum contract cardinality.

Each `XmlContractViolation` carries bounded contract/element identity and kind. Its enclosing record
is the single source of attempt-order sequence, optional occurrence identity, and optional bounded
source span. Model-authored values may be inspected through the raw output plus span, but arbitrary
model text must never be copied into `ApplicationFault` display or debug output.

Parser route/protocol failure, stale mount, reducer panic, live runtime failure, and publisher
failure use structural framework faults. Hard safety limits such as total input bytes, XML depth,
attributes per element, per-content bytes, and total occurrences are also framework `Limit` faults:
they abort parsing and are never copied into retry feedback. Faults terminate or recovery-fence the
application; the implementation extends the current `ApplicationFaultKind` policy with the recovery
kind described in section 5.4.

## 9. Forgotten City mappings

### 9.1 Player selector

```rust,ignore
let select_intent = XmlToolElement::self_closing("select_intent")
    .required_attribute_with("handle", "...", IntentHandle::parse_wire)
    .required_attribute::<Verb>("verb", "ask|tell|greet|leave")
    .optional_attribute::<StorageString>("topic", "topic_id")
    .optional_attribute::<StorageString>("knowledge", "knowledge_id")
    .optional_attribute::<StorageString>("recipient", "pawn_id|public")
    .occurs(XmlCardinality::between(1, 5))
    .decode(decode_select_wire, |_, _| Ok(()));
```

The shared state holds the captured intent map, selected handles, accepted outputs, and next
sequence. Because a self-closing Open and Complete are adjacent, `on_complete_validated` checks the
captured pool and duplicate state, then its valid reducer updates State and emits ordered Output +
Live values in the same dispatch batch. The live runtime owns the tentative selection lease. An
invalid occurrence never reaches the mutable reducer; any earlier scoped lane and lease are removed
before another sibling runs. The final reducer accepts when at least one valid output exists;
invalid siblings remain feedback diagnostics. With no valid output, it rejects, rolls back all
tentative options, updates feedback, and returns a reaction-request action according to its budget.

### 9.2 Phrase

```rust,ignore
let phrase = XmlToolElement::text("phrase")
    .occurs(XmlCardinality::exactly(1))
    .decode(|_| Ok(()), |_, text| Ok(text.to_owned()));
```

Open emits `BeginPhrase`, deltas emit `AppendPhrase`, and a valid complete reducer records the final
string. A complete validator can reject sentence/domain policy without mutating State, which
withdraws the same occurrence's earlier text effects. The live runtime returns receipts containing
the `TextId`/lease. Rejection, provider failure, or cancellation rolls those leases back. Published
acceptance, or a configured no-op publication barrier, enters confirmation; a failed or unknown
acknowledgement is recovery-fenced rather than treated as either cancel or success. The application
never computes its own byte delta.

### 9.3 NPC response

One strict contract registers all six tags. `thought`, `speak`, `act`, and `log_behavior` are text
elements. `share_knowledge` and `update_relationship` are self-closing elements with typed
attributes. They share one contract-attempt state value, order sequence, and final validator.

Visible speech may use Live effects. Relationship and knowledge changes should normally be Commit
values, not irreversible open-handler side effects. The final validator enforces at least one
thought/speak pair, share-before-speak adjacency, known knowledge IDs, and sentence policy. The
per-element complete validators can invalidate a domain-bad item and withdraw only that item's
earlier Live text before a later sibling is dispatched. The
explicit unclosed-text EOF policy may be enabled during compatibility migration and removed after
production evidence shows strict EOF is safe. Under that policy the last open text item receives a
normal final delta plus `XmlComplete { form: ImplicitEof }`, so TextWriter and completed-count
behavior are deterministic.

### 9.4 Chess

The in-repository example uses one `chess.action` contract with a nonempty `thought` text element
followed by one `choose_move` or `resign` self-closing element. Shared State prevents an action from
completing before the thought; missing, invalid, repeated, or late thoughts produce retry feedback.
Its final reducer accepts exactly one decoded action; its publisher applies that action to the
shared Chess Signal synchronously and retains a Published/NotPublished journal for replay.
Rejection writes the existing typed feedback. After streaming cleanup, `use_preparation` waits for
Stockfish and requests the next model attempt only when ready. Terminal preparation stops the
driver before another provider submission.
This example explicitly disables Live and Commit. See
[`chess_action_component.rs`](../examples/chess_agentview/chess_action_component.rs).

A richer application can also add a preview and a durable outbox:

```rust,ignore
let move_element = XmlToolElement::self_closing("move")
    .required_attribute::<StrictUciMove>("uci", "e2e4")
    .occurs(XmlCardinality::exactly(1))
    .decode(decode_move, |head, _| Ok(*head));
```

In that extension, a complete validator checks the captured legal set, its valid reducer emits a
Live preview, and the final reducer emits Output + Commit. An application-owned publisher would
atomically record the move and an outbox item containing the expected ply. This durable adapter is
not part of the current example.

## 10. Compatibility and migration

The implementation and migration boundaries are:

1. **Structured route and contract core (implemented)**: the internal selected-text sidecar,
   independent strict parser, typed builder, declaration validation, cardinality, reducer state, and
   normal EOF handling are present.
2. **Application coordination and managed lanes (implemented)**: one worker starts per strict
   contract, workers receive the same selected input, cleanup is joined before a single
   post-reconcile/demand handoff, and uncertainty remains behind the recovery fence.
3. **Legacy compatibility (current boundary)**: `StreamingXml::tag(...)` and
   `XmlStreamingToolCall::contract(...).empty_element(...)` continue to use their existing
   shared-route permissive parser. They are not contract workers and do not participate in a strict
   contract's state, final decision, managed lanes, or recovery ledger.
4. **Chess consumer (implemented)**: one multi-element contract, thought-before-action validation,
   synchronous publication, rejection feedback, and preparation-driven successor scheduling;
   40 example tests cover the barrier, scripted workflow, and publication journal.
5. **Forgotten City consumers (external follow-up)**: migrate phrase, NPC, and selector to
   `XmlStreamingToolCall::new::<C>(identity)`, then remove that application's `StreamingToolRunner`,
   direct `HermesParser`, and deleted mounted API references. Those consumers are not implemented
   or validated by changes to this repository.

During migration, strict contracts and legacy permissive subscribers may coexist on one admitted
reaction, but they have separate parsers and input projections. Strict contracts consume the keyed,
non-commentary primary-text sidecar; legacy subscribers keep consuming the public mixed-text route.
Multiple strict contracts also coexist: every worker sees the same selected input and makes its own
decision. Contracts that intentionally declare different top-level tags opt into
`.ignore_unknown_elements()` independently; a strict contract that does not opt in still reports a
foreign top-level element as its own diagnostic.

The legacy `streaming_tool::StreamingTool` trait and `StreamingToolRunner` remain available only
until all in-repository consumers migrate. They receive deprecation notices before removal.

## 11. Acceptance suite

Current AgentView coverage includes 19 streaming unit tests, 17 Application integration tests,
7 compile-fail fixtures, and 40 Chess example tests. The default and no-default-feature full
library/integration suites pass; the final sequence and panic changes additionally pass focused
checks. The matrix below also describes consumer-specific and broader fault-injection acceptance
criteria, including external adapters that are not present in this repository.

### Parser and grammar

- Every UTF-8-safe chunk split produces the same non-delta parser structural payloads/spans and
  parser diagnostics after ignoring attempt/order IDs. Individual text-delta boundaries, spans,
  and sequence values may differ, but their ordered raw coverage and concatenated decoded value per
  occurrence match. Reducer emissions/final results are equal only for reducers documented and
  tested as delta-segmentation-invariant.
- Tag names, attributes, quotes, entities, closing prefixes, and multi-byte text may cross chunks.
- Repeated and heterogeneous tags retain source order within each contract parser.
- Two contracts declaring the same tag receive independent parser events, state initialization,
  decisions, and publications on every reaction.
- Contracts declaring different tags compose when each appropriate contract independently opts into
  unknown-element ignoring; a strict sibling still reports the same foreign markup.
- Duplicate element names in one strict grammar fail at mount before provider submission.
- Invalid element names, invalid attribute names, and duplicate attribute declarations within one
  element produce their exact structured `ComponentAttemptFault` before prompt construction or
  provider submission. Required/optional and decoder-type differences do not make a duplicate
  attribute declaration valid; the same attribute name on different elements remains valid.
- Declaration-fault tests also assert sanitized `ApplicationFault` classification, zero provider
  submissions, and zero state/Live/publisher factory calls. A duplicate attribute in actual model
  markup instead reaches the typed parser diagnostic path after submission.
- Self-closing elements produce adjacent open/complete events.
- Text events expose correct decoded delta and accumulated value.
- Open spans cover opening syntax; explicit Complete spans cover closing syntax; self-closing Open
  and Complete share the element span; ImplicitEof Complete has a zero-width EOF span. Entity and
  multi-byte split tests prove delta spans retain exact lexical coverage and UTF-8 boundaries.
- Lax normal EOF emits the final delta and one `ImplicitEof` completion; provider failure does not.
- Unknown/nested/outside content and malformed, mismatched, duplicate, missing, extra, invalid, or
  incomplete markup produce the expected typed violation.
- Input, depth, attribute, content, and occurrence safety limits fail as framework faults.
- Commentary cannot enter the strict parser; primary seal and reaction EOF are independently
  verified.
- Empty/tool-only reactions reach finalization and cardinality checks without a fake TextComplete.

### Reducer and attempt

- State is recreated for every contract on every reaction.
- Compile and runtime tests cover all nine `StreamingToolUpdate` constructors. A mixed
  `output(...).with_live(...).with_diagnostic(...).with_commit(...)` update is interpreted in that
  exact order, and tests use non-`Clone` lane values.
- An invalid opening does not invoke business handlers; later invalidation removes its scoped staged
  entries and rolls back its Live receipts.
- A phrase/NPC occurrence that emitted Live on open/delta and then fails its complete validator does
  not invoke the mutable reducer, withdraws only that occurrence, and leaves sibling State intact.
- With two occurrences in one contract, an indeterminate rollback while invalidating the first
  aborts rather than dispatching the second, discards that contract's staged lanes, and becomes
  Terminal only after its receipt cleanup is definitive.
- A Domain diagnostic by itself remains a warning; explicit `XmlOccurrenceValidity::Invalid` is the
  only handler-level invalidation signal.
- Maximum cardinality emits a violation before decode or an open handler.
- Cross-tag ordering and repeated occurrences of the same declared element are deterministic within
  one contract.
- The final reducer runs once only on normal EOF and sees exact bounded raw output.
- A valid selector occurrence plus an invalid sibling can be accepted by application policy.
- Compile-fail tests prove every required attempt slot, exactly one completion handler per element,
  and both explicit disabled-lane choices.
- State initializes before submit; Reject/no-Live attempts never invoke an unused Live/publisher
  factory, and `without_*` never constructs the disabled adapter.

### Live transaction

- Live applies are awaited in emission order and return Applied/NotApplied/Indeterminate.
- Prepared operation evidence exists before every apply/confirm/rollback future is polled; caught
  future panic cannot consume that evidence.
- Once started, apply/confirm/rollback is completed by the supervisor after the react waiter drops.
- Fault/cancellation is injected after every apply and before/after finalization/publication.
- Every conclusively not-published path settles effects as rolled back in reverse order; idempotent
  recovery may invoke an adapter more than once without applying the external result more than once.
- A four-receipt settlement pass producing two Indeterminate results and one NotSettled result still
  attempts the fourth receipt and retains each receipt/operation independently in stable order.
- Published success enters confirmation, continues after individual errors, and never rolls back.
- Indeterminate apply resolves to NotApplied or to an Applied receipt that is then rolled back.
- A stale queued selector/phrase message cannot revive an aborted attempt.
- Dropped post-handoff reaction plus consuming shutdown completes managed cleanup.
- `Accept + without_publication + Live` crosses the no-op barrier and confirms all receipts; it never
  leaves them applied-but-unsettled.

### Publication and recovery

- Output/Commit are invisible before accepted publication.
- Output and Commit reach each contract's publisher in one contract-local ordered sequence with
  origin metadata. Separate contracts have no fabricated global publication order.
- A complete publication operation is stored before `publish` is polled; dropping or panicking the
  publish/resolve waiter does not lose it or cause a second invocation for the same generation.
- `NotPublished` rolls back Live and cannot leak staged values.
- `Indeterminate` prevents replay, rollback, and successor reactions until `resolve` is definitive.
- Publish-resolved success resumes confirmation; resolved non-publication resumes rollback.
- Publish-resolved success and no-op-barrier recovery run accepted post-reconciliation exactly once
  after confirmation; dropping a waiter after its claim cannot expose Settled early or rerun it.
- A recovery call made while a supervisor generation is already running reports/awaits InFlight and
  never invokes the adapter reentrantly; dropping all waiters does not stop that generation.
- Adapter-future panic rebuilds the adapter before resolution; factory error/repeated panic preserves
  the exact ledger and returns StillRequired without invoking the old instance.
- Recovery preserves every continuation: Published resumes confirmation; NotPublished resumes
  rollback; Reject runs its handler and post-reconciliation exactly once before releasing an
  optional reaction demand; fault/cancel returns to the stored Terminal disposition.
- Rejection fault injection at handler-claim, outcome-save, post-reconcile-claim, demand-stage, and
  Ready boundaries proves no handler/reconcile rerun and no lost or duplicate reaction demand.
  Handler failure still attempts reconciliation once and never stages its action.
- Dropping the original react waiter before handler claim skips it and ends Terminal; dropping it
  after claim finishes the claimed callback/reconcile but suppresses demand. Dropping a recovery
  waiter only detaches and still permits RetryReady.
- An indeterminate contract keeps the Application fenced while an already settled sibling stays
  settled and is never replayed. A dropped recovery waiter does not stop the unresolved worker.
- After all contracts are definitive, Application post-reconciliation runs once and releases at
  most one coalesced demand. A failed post-reconcile retains the fence and does not replay a
  rejection handler on recovery.
- Terminal disposition and RecoveryRequired can coexist; the recovery fence is exposed until every
  receipt is settled, after which the original terminal fault becomes visible.
- Resolution StillIndeterminate/error retains the identical operation and does not replay publish.
- Consuming shutdown waits for in-flight work and keeps retrying every non-Clear cleanup state with
  backoff. A permanently incomplete adapter keeps the future pending and never produces a clean
  shutdown result.
- `recover_streaming_attempt` and consuming `shutdown` are the only mutating entry points accepted
  through the recovery fence.
- Durable publisher tests cover before-write, after-write-before-receipt, duplicate request,
  repeated resolution, process restart, revision conflict, outbox replay, acknowledgement, and
  dead-letter recovery fencing.

### Consumer parity

- Selector covers 1..=5, duplicate handles, captured scope, partial acceptance, reverse option
  withdrawal, and retry exhaustion.
- Phrase covers open, all deltas, completion, provider failure, rejection, and cancellation.
- NPC covers every tag, repeated/interleaved order, share-before-speak, and strict/lax migration EOF.
- Chess covers canonical UCI, captured legality, exactly one move, preview, publication, outbox, and
  recovery fence.
- Cube Stage native tools compile and behave unchanged.

## 12. Rejected alternatives

### Add more methods to the current single-attribute chain

This would add syntax but not shared state, strict envelope ownership, cardinality, finalization,
live compensation, or publication. Retaining its shared-route parser would also force separately
authored strict contracts to compete for one grammar and one effect lifecycle.

### Share one strict parser across all contracts

A reaction-level scanner looks cheaper, but it makes grammar ownership, occurrence order, parser
faults, and unknown-element policy cross-component concerns. Independent parsers preserve local
state and effects, permit the same tag in multiple contracts, and let a contract choose its own
compatibility policy.

### Restore the deleted mounted runtime

It solved more than XML streaming and coupled prompt views, mounted epochs, channels, publication,
and durable ownership into a large parallel runtime. The current Component/Application engine is
the authority; the new API adds only the missing attempt capability to it.

### Let async lifecycle handlers mutate Signals directly

Signal writes are immediate business facts and are not reaction transactions. Cancellation may
drop a pending handler after an earlier write. Such callbacks cannot provide selector withdrawal,
phrase cancellation, or chess preview rollback guarantees.

### Put retry or database commits in the parser

The parser does not own canonical history, reaction scheduling, a domain database, or a durable
outbox. It emits a typed terminal decision and crosses explicit injected runtime boundaries; the
outer application remains the policy owner.

### Treat TextComplete as EOF

A text output can seal before the reaction ends, and a tool-only reaction may have no text output.
Conflating the two loses protocol information and makes correct finalization impossible.

### Use binary effect results or consume receipts during settlement

A binary apply error cannot prove whether an external effect occurred, and consuming a receipt
before confirm/rollback succeeds destroys the only recovery evidence. Three-way outcomes plus
borrowed receipts make uncertainty explicit and keep it fenced.

## 13. Cross-repository completion

The framework implementation is available in AgentView. The wider migration across AgentView,
Forgotten City, and Cube Stage reaches its target state only when:

- all acceptance groups above run against the current `Application<ReactionPort>` path;
- multiple strict contracts can share a reaction input with independent parsers, state, decisions,
  publications, and recovery reports, while Application performs one definitive handoff;
- the legacy `.contract(...).empty_element(...)` route is either deliberately retained with its
  documented shared-parser behavior or explicitly lowered through the new worker with parity tests;
- Forgotten City has one implementation per XML consumer and no references to deleted mounted
  types;
- no Forgotten City consumer manually derives text deltas from cumulative byte lengths;
- cancellation and publication fault injection proves that no tentative Live effect leaks;
- recovery fault injection proves every apply/publish/confirm/rollback uncertainty can be driven to
  a definitive result without admitting a successor reaction;
- durable Commit tests prove the publisher/outbox contract without giving the XML parser database
  authority;
- compile-fail tests prove the builder cannot omit state, elements, finish, or a lane decision;
- `cargo check --all-targets` and the relevant test suites pass in AgentView, Forgotten City, and
  Cube Stage.
