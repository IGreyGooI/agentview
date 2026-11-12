# System 1 decisions

`Decision` declares one named natural-language question over the ordinary Frame's
model-visible state. Its question determines its answer type. The same Component
works with native `System1Provider` and the ordinary Responses provider.

## Declare state and questions in the same view

Render the information to judge using ordinary Components and POM. Put shared
instructions in their normal placement. Declare each question beside that state:

```rust,ignore
use agentview::component::prelude::*;

#[component]
fn ticket_judge(
    ticket: String,
    output: Signal<Option<QuestionOutcome<NoulAnswer>>>,
) -> Component {
    view! {
        #[developer]
        "Judge using only the displayed ticket."

        ticket { "{ticket}" }

        Decision {
            name: "is_bug",
            question: Noul::new("Does this ticket describe broken product behavior?")
                .criteria("The product behaved incorrectly.", "A question or feature request."),
            on_result: move |_context, outcome: QuestionOutcome<NoulAnswer>| {
                output.set(Some(outcome))
            },
        }
    }
}
```

There is no second context attached to the Decision. Its state is the effective
observation compiled from the submitted Frame, including the System snapshot, causal
history, current view, staged inputs, and ephemeral inputs under their normal rules.
An independent judgment mounts an independent Application with the relevant view; it
does not implicitly import a parent application's state.

Each Decision requires `name`, `question`, and `on_result`. Properties can appear in
any order. Question names must be unique within a Frame; a duplicate is rejected
before submission. Empty names or invalid criteria are also rejected during Frame
preparation with a recoverable `InvalidDecisionDeclaration` error. No HTTP request is
made; update the declaration and explicitly react again to retry.
Several Decision Components become one `questions` map and use
one Provider request. Each keeps its own typed handler.

## Question types and factories

| Question | Constructor | Handler answer type |
| --- | --- | --- |
| Probability of a proposition | `Noul::new(instructions).criteria(when_true, when_false)` | `NoulAnswer` |
| Choice among named criteria | `Choice::new(instructions, BTreeMap<String, Value>)` | `ChoiceAnswer` |
| Continuous position on ordered criteria | `Score::new(instructions, Vec<Value>)` | `ScoreAnswer` |

Noul criteria are optional. `NoulAnswer.noul` retains the native probability; the
caller chooses any business threshold. `ChoiceAnswer` retains the chosen key and
optional probabilities and confidence. `ScoreAnswer` retains its floating-point
score, optional probabilities, legend, and confidence. Score positions start at zero;
they are not rounded into an integer class.

A factory is an ordinary Rust function returning a question. The builder infers the
handler's answer type from that question:

```rust,ignore
use std::collections::BTreeMap;
use serde_json::json;

fn owner_question() -> Choice {
    Choice::new(
        "Which team owns the displayed ticket?",
        BTreeMap::from([
            ("frontend".to_owned(), json!("Rendering and layout problems")),
            ("payments".to_owned(), json!("Checkout and payment problems")),
        ]),
    )
}

Decision::new("owner", owner_question())
    .on_result(move |context, outcome| {
        outcomes.store(context, outcome)
    })
    .build()
```

Factories contain question instructions and criteria; business context stays in the
view. Typed factories and callbacks do not require `Deserialize` or `JsonSchema`
result registration. Mapping a choice into a business enum, combining several answers
into a business record, or applying a threshold are local operations and need no
additional model call. Handlers may return a synchronous `Result` or an asynchronous
future producing one.

The public authoring API currently covers text judgments with Noul, Choice, and Score.
The lower-level native protocol includes bounding boxes, but this release has no
public typed image/assets delivery API and does not claim image judgment support.

## Results and event handling

Each callback receives `QuestionOutcome<T>` containing `result` and `source`:

- `Ok(answer)` is the validated answer for that question.
- `Refused` means the provider declined that question.
- `MissingResult` means the response did not answer that question.
- `InvalidAnswer` means its type, selected option, or numerical data violated the
  submitted question's contract.

A question's failure does not erase independent valid answers. Unknown answer names
do not acquire a Component handler. Transport, malformed JSON/envelopes, stream
protocol, observation, and handler failures remain reaction errors.

The runtime generates batch identity and associates answers with the actual submitted
Frame. Question declarations and local handlers are frozen for that submission;
a later render cannot reinterpret its answer using new criteria or callbacks.
Canonical admission records the actual complete output before dispatching events.
Each Decision stages its answer until normal reaction completion, then runs
`on_result(context, outcome)`. EventContext retains the producing application,
reaction, Frame, and event identity.

A handler does not provide a transaction guarantee across the entire reaction. An
external caller should publish final success after `ReactionOutcome::Completed` and
successful consuming shutdown. Retain useful answers and errors in application state,
then render them in the receiving application's next submitted Frame when its model
needs to act on them. Writing a Signal alone does not deliver feedback.

## Providers and one-reaction execution

Native `System1Provider` sends the Frame observation as `state` and the question
declarations as `questions`. It uses a complete JSON response from OpenRouter's
`POST /api/alpha/decisions`, or `/api/v1/systemone` with
`System1Endpoint::SystemOne`. It does not send a Responses request or consume text SSE.
The ordinary Responses provider lowers the same questions into its structured-output
request and returns the same logical answers through shared admission and handlers.

```rust,ignore
use agentview::{
    component::execution::ProviderIdentity,
    provider::{
        ModelSpec,
        async_openai::AsyncOpenAiTransportConfig,
        system1::{System1Options, System1Provider},
    },
};

let transport = AsyncOpenAiTransportConfig::new(
    "https://openrouter.ai/api/v1", api_key,
)?;
let options = System1Options::new(ModelSpec::new(model_id, context_window_tokens)?);
let identity = ProviderIdentity::new("openrouter", "system1", 1, "ticket-judge")?;
let provider = System1Provider::try_new(transport, identity, options)?;
```

The default Decisions endpoint targets OpenRouter. For a direct TypeSafe host, select
`System1Endpoint::SystemOne` explicitly. OpenRouter's System One option uses the
`https://openrouter.ai/api/v1` API base shown above.

For Ark's beta System One API, use `https://ark.cn-beijing.volces.com/api` or
`https://ark.cn-beijing.volces.com/api/v1` as the API base, and configure
`System1Options::new(model).with_endpoint(System1Endpoint::SystemOne).with_ark_beta()`.
The explicit beta option sends `ark-beta-systemone-api: true`; it is absent by
default and does not change endpoint selection. Ark's Responses `/api/v3` base
does not address this API. Keep a provider template and use `fresh_session()` for
independent judgments to preserve the HTTP connection pool and beta option.

For a native endpoint requiring the API key directly in `Authorization`, select
`System1Options::with_raw_authorization()`. This explicit option is preserved by
`fresh_session()`; the default remains Bearer authorization and ordinary Responses
requests are unaffected. Native usage can contain only `input_tokens`: missing
output and total counts remain unknown, rather than being reported as zero.

The native Provider shares HTTP transport configuration, authentication, budgets,
request/call/usage observers, and diagnostics. Its options describe the actual native
protocol; Responses generation options and Provider wrappers are not native setup
APIs. Native probabilities and a general language model's reported confidence do not
carry an identical calibration guarantee.

| Provider | Native tools | Text deltas | Named Decisions |
| --- | --- | --- | --- |
| Ordinary Responses | Yes | Yes | Yes |
| Native System 1 | No | No | Yes |

Missing capabilities are rejected before submission. Native System 1 also rejects
ordinary XML action output because it has no text output protocol. A separate
complete-only text Provider can still feed sealed text into XML callbacks without
text-delta capability. Responses keeps its ordinary native/XML interaction behavior.
Active native/XML action declarations cannot share a Decision-output Frame; passive
observers and ordinary POM can accompany any number of uniquely named Decisions.

Drive an independent Application through one reaction and consuming shutdown. All
questions use that reaction's single HTTP request, with no confirmation, repair, or
compaction model call. Byte-based context estimates remain advisory, including when
they exceed the declared capacity; exact request byte limits and actual provider
capacity errors still apply. A failed preparation makes no HTTP request. A fresh
session can share transport and observers while starting a new target.

For a single question, `system1::judge(provider, state, question)` is a convenience
wrapper. It renders the explicit JSON state into an ordinary root view beside the
same Decision Component, drives one reaction, and waits for consuming cleanup:

```rust,ignore
let judgment = agentview::system1::judge(
    provider,
    serde_json::json!({"utterance":"Please open the gate."}),
    Noul::new("Does the displayed utterance request opening the gate?"),
).await?;
let probability = judgment.value.noul;
```

It does not create another context channel or infer a question from a result schema.
Cancelling its caller transfers the Application to its cleanup owner.

## Caching

Responses keeps its existing prompt-cache configuration. Native Decisions has no
Responses cache-key or TTL request fields; its actual
`usage.input_tokens_details.cached_tokens` remains available through shared accounting.
This implementation does not cache results locally.

A future result cache must bind the model, complete effective Frame observation,
questions, and execution configuration, preserve the current Frame/event path, and
publish only after complete validation, reaction success, and cleanup. Frames with
business action handlers cannot be treated as pure cached judgments.
