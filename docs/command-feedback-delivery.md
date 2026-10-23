# Command feedback and delivery

Use this guide when extending Actions, feedback, or a frontend. The authoritative
runtime contract is [engine.md](engine.md); Component authoring starts in
[HELP.md](../HELP.md).

## Declare complete current state

Every Component render declares the complete current POM that the model needs.
Keep facts and action outcomes in application state. Rendering can happen many
times as state and callback captures change; delivery deduplication does not mean
that a Component renders only once.

The native Frame compiler already compares ordinary POM with the last complete
projection successfully handed off in the same scope. Unchanged ordinary values
can be omitted without adding `#[diff]`. Changed text is delivered as a complete
value; XML follows its existing comparison and field strategies. Rendering or
preparing a candidate does not advance the delivery baseline.

Keep stable context and changing statistics in separate stable POM roots; child
Components can give them separate projection nodes when needed. For example, a
notebook's instructions belong in `notebook_context`, while its count and latest
note belong in `notebook_state`.
This lets a native Frame deliver changed state while omitting unchanged context.
Do not replace complete declarations with application-owned "already sent" flags.

An `Action` already has its own stable projection node containing its description,
availability, and generated JSON input schema. Keep it mounted with the current
callback; ordinary Frame comparison can omit its unchanged POM. Do not put a
copy of every schema inside changing statistics or feedback.

Native provider tools have a different contract: the Frame's tool catalog remains
complete. Omitting unchanged `Action` POM does not permit dropping current native
tool definitions from that catalog.

## Preserve feedback as a complete batch

Use the framework's feedback Component, installed by the root's
`use_wait_for_command()`. It renders ordinary typed POM through `view!`, with one
outer `#[diff(slot = "command_feedback")]`. The unchanged whole snapshot can be
omitted; a changed batch is delivered in full.

Keep the interior records as whole values. Do not introduce keyed per-result
patches: replacing a delivered batch must not generate `<remove>` output that
replays old result bodies, followed by inserts for new results.

All action results accumulated before one Frame handoff belong to that batch,
including ordinary dispatch rejections. Preserve every result, rather than a
fixed number of recent records. Independent action completions remain distinct
even when their JSON results are equal. For native Frames, a successful handoff
marks the boundary for the next batch. Render, prepare, cancellation before
handoff, and submission failure do not mark that boundary. A failure after handoff
does not undo the handoff.

An observation invokes no action and preserves feedback. An empty subsequent
batch does not clear retained or pending results. The next nonempty action batch
replaces the preceding batch as a whole. Input parsing diagnostics also remain
visible across observations and are cleared by the next action.

Keep action names, error codes, diagnostics, and callback JSON results complete.
Feedback and stdin/socket transport do not add 4 KiB, 8 KiB, or 128 KiB caps. Never
slice serialized JSON or substitute a preview for the actual result. Existing
Component, Frame, and provider wire/token budgets still apply when submitting to
a model; exceeding a real budget must fail explicitly rather than truncate data.

## Deliver stdin frames to each reader

`StdinApplication::run(root)` owns the CLI and retained daemon. Connecting
immediately delivers a prepared Full view, without waiting for input. It accepts one
sequence of top-level JSON action objects per input frame. Objects may span
multiple lines; the sequence needs no `actions` wrapper or end marker.
With stdin open, end each intended batch with a newline (`\n`). After 100 ms
without new input bytes, the newline-terminated buffer submits as one frame.
A newline alone does not submit immediately; without a final newline,
even complete JSON remains buffered. EOF submits the tail immediately without
requiring a newline. New bytes reset the idle timer; an empty buffer creates no
frame. Empty stdin receives only the initial view, with no extra response at EOF.
`run_with_options` can configure a positive `idle_refresh_interval`; ordinary
`run` leaves refresh disabled. An idle observation uses the same preparation
barrier, serialized application queue, and native Frame cursor as action responses.
It preserves feedback and partial input, including its independent framing
deadline. Changed external state can arrive without an input frame; unchanged
idle deltas emit no stdout. A refresh never interrupts or retries accepted actions.
The stdin driver waits for ingress before beginning preparation. Its external
reads therefore happen after each observation or action request arrives; it does
not prepare a snapshot and leave it waiting during the previous idle period.
`StdinApplication::mount(root)` exposes the same application owner
to a custom transport. Its `observe`, `submit`, `submit_frame`, and `feedback`
methods return complete snapshots. The standard CLI adds incremental delivery
for its actual reader without changing those programmatic results.

Framing does not inspect JSON completeness. After newline plus idle or EOF fixes
the boundary, the runner decodes the entire frame. Incomplete or malformed JSON,
or an invalid action envelope, rejects the whole frame without executing its
prefix and clears the buffer; later input starts a new frame.
Once the whole sequence decodes, ordinary rejections for invalid typed
input, unknown actions, and disabled actions are recorded individually; later
actions in the frame still run. Callback runtime faults retain their existing
failure rules.

One accepted frame occupies one queue entry until all its actions execute in
order. Another frame or observation cannot interleave with it. A nonempty frame
starts a new feedback batch; the final view retains every outcome in that frame.
Cancelling a caller's waiter does not cancel accepted or remaining actions.
An empty `submit_frame` is an observation and preserves feedback.

The CLI reuses one daemon connection for successive input frames. Each connection
owns a private `FrameSession` and compiles the final projection once after the
whole batch executes. Its first delivery is Full. Subsequent deliveries use the
existing Frame compiler's Delta and omission rules: unchanged context and Action
schemas are omitted, and changed state and new feedback are delivered. System
snapshots, `#[diff]`, `repeat`, and `ephemeral` keep their existing compiler rules;
the adapter does not calculate its own text diff.

stdout does not grant provider role authority. System content is an explicit
`<system_snapshot>` replacement; removing it emits `<system_snapshot />`.
Changing System content can require a Full frame with the existing compiler's
history replay. Ephemeral content is excluded from this connection's compiler
history; an external agent still controls its own tool history.

The runner renders and flushes one outer `<view>` per frame. Connection metadata
is included on the initial delivery. Outcomes appear in feedback without a separate
`<action_result>` copy. The programmatic `StdinResponse.result` remains available:
`submit` returns one direct callback result; `submit_frame` returns an ordered
array of `{action, ok, result}` records. JSON retains normal JSON escaping.

Send the whole intended batch and final newline in one write, wait for its one
view, then send the next batch. The application cannot observe a caller's write
boundaries. Two writes less than 100 ms apart can merge when the caller has not
waited for a response. A pause longer than 100 ms after any newline can split a
batch, including inside a multiline object. The idle interval groups input and
does not limit action execution time.

Business state may be shared through a daemon; delivery baselines belong to each
connection. Two readers can interleave actions without advancing each other's
baselines. A new CLI process receives its complete initial view and schemas.
The daemon commits a connection's baseline only after its response is written
successfully. A failed write closes that connection; accepted business actions
are not rolled back or replayed. A replacement caller starts with a full view.

Waiting on an idle connection for the next request's first byte has no transfer
deadline. Once bytes arrive, socket transfers retain their ten-second deadline.
Queue and async callback waiting retains its separate five-minute deadline until
the first response byte.

## Inspect recorded CLI runs

The native runner records every foreground stdin interaction and every `start`,
`status`, `stop`, or `restart` invocation under a unique run ID. Inspect them with
the same binary:

```sh
target/debug/examples/command_feedback_cli history
target/debug/examples/command_feedback_cli history latest
target/debug/examples/command_feedback_cli history ID
```

`history` lists the most recent 20 runs, including their invocation and completion
status. The other forms display stdin, stdout, and stderr in recorded event order
with stream labels and relative times. A lifecycle command has its own run ID;
after stopping the daemon, use the interaction's ID to inspect its transcript.
History reads local files without connecting to a daemon or dispatching actions.
Reading history and help create no history records.

The root is `$XDG_STATE_HOME/agentview-<executable-name>/history`, falling back to
`$HOME/.local/state/agentview-<executable-name>/history`. Each run directory holds
`session.json` for invocation, socket, timing, and exit metadata; `stdin.raw`,
`stdout.raw`, and `stderr.raw` for exact stream bytes; and `events.jsonl` for
ordered timestamps and byte offsets and lengths into those files. The reader can
show recorded bytes even while a run is open. An unfinished record can also mean
the foreground process was interrupted; it does not establish an action outcome.

These files preserve what the frontend actually read and delivered, including
incremental views and diagnostics. Reading them never re-executes input, rebuilds
a current view, or changes a Frame delivery baseline. Treat POM as opaque text;
use the event records and byte ranges when examining the recording programmatically.

The daemon separately records one session across all of its client connections:

```sh
target/debug/examples/command_feedback_cli history --daemon
target/debug/examples/command_feedback_cli history --daemon latest
target/debug/examples/command_feedback_cli history --daemon SESSION_ID
```

These commands read `daemon-history/SESSION_ID/session.json` and `events.jsonl`
under the same application state root. The first view exposes the daemon session
ID, and each foreground run's metadata associates it with the daemon sessions it
contacted. A restart associates its foreground run with both the old session and
the new one. Daemon history events include client connection IDs, CLI run IDs,
accepted operations, execution order, complete application views and results,
response socket writes or failures, and recorded session completion. The viewer
prints the stored complete view as opaque text, independently of each connection's
diff cursor. It also shows raw CLI history paths found in the reader's state root;
another client's raw files may live under a different `XDG_STATE_HOME`.

Request acceptance order and execution order are separate recorded facts. A
successful socket write records that all response bytes were written; it does not
confirm the client read them. An execution result remains recorded even when its
response write fails. A `request_not_executed` event records a known outcome when
the closed queue or stop prevents execution. A request without a recorded execution
outcome remains unconfirmed. If session completion is absent, the daemon may still be running or was
interrupted; previously recorded execution results remain available. The reader
ignores an incomplete final JSONL line while retaining every complete earlier
event. It never starts a daemon or replays recorded actions.

## Run the examples

Both examples use the same notebook Actions: `record` accepts `{"text":"..."}`;
`inspect` takes `{}`. They run without model credentials.

```sh
cargo build --locked --no-default-features --example command_feedback_cli --example command_feedback_frames
target/debug/examples/command_feedback_frames target/command-feedback
demo_dir=$(mktemp -d /tmp/agentview-feedback-XXXXXX)
target/debug/examples/command_feedback_cli --socket "$demo_dir/socket" <<'JSONL'
{"action":"record","input":{"text":"Keep complete action results."}}
{"action":"inspect","input":{}}
JSONL
target/debug/examples/command_feedback_cli --socket "$demo_dir/socket" status
target/debug/examples/command_feedback_cli --socket "$demo_dir/socket" stop
rmdir "$demo_dir"
```

The [CLI example](../examples/command_feedback_cli.rs) first delivers the complete
initial notebook view, then submits both actions above as one frame at EOF. Its
resulting Delta contains the changed state and both feedback records; unchanged
context and action definitions are omitted. The example also enables idle
observations once per second, so another reader's changes arrive while stdin is
open. Both actions return the same snapshot,
`{"count":1,"latest":"Keep complete action results."}`, and each receives its own
feedback record. Closing stdin leaves the daemon running; the final command
above stops it.

The [Frame example](../examples/command_feedback_frames.rs) captures exact
canonical payloads through `DebugProviderPort`. It writes `frame-01.json` through
`frame-04.json`, their rendered `frame-NN.view.txt` files, and `summary.json`:

| Frame | Actions before handoff | Expected projection delivery |
| --- | --- | --- |
| 1 | `record`, then `inspect` | Full context, state, two schemas, and both feedback records (1 and 2), even though their result JSON is equal. |
| 2 | None | Empty Delta projection items; the rendered projection file is empty. |
| 3 | `inspect` | The complete new feedback batch (record 3), even with the same JSON result again; no insert/remove patches. |
| 4 | `record` | Changed notebook state and the complete feedback batch (record 4); unchanged context and schemas are omitted. |

Compare the captured roots, feedback records, patch operations, and byte counts
in `summary.json`. The stdin adapter uses the same Frame lowering rules and
renders their resulting POM. Provider HTTP encoding remains provider-specific.

See the [incremental stdin captures](examples/command-feedback/incremental/README.md)
for exact input/stdout from three successive batches, measured bytes, and a new
reader's complete initial view. The [earlier real captures](examples/command-feedback/current/README.md) preserve
the full stdin/stdout and all four native Frames from before incremental stdin
delivery. Their raw files remain historical evidence; their repeated stdin views
do not describe the current connection protocol.
