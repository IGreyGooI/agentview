# Stdin captures before incremental delivery

These are actual credential-free example runs captured on 2026-10-17. The earlier
[per-action output history](../interaction.txt) remains unchanged for comparison.

These captures predate per-connection Frame delivery. Their persistent stdin run
repeats complete context and schemas, preserving the old behavior. The current
CLI sends an initial Full view and later Delta deliveries on the same connection;
see the [current protocol](../../../command-feedback-delivery.md#deliver-stdin-frames-to-each-reader).
The [new incremental captures](../incremental/README.md) record successive views
and a fresh reader separately; the raw evidence here remains unchanged.

## Reproduce

From the repository root:

```sh
cargo build --locked --no-default-features --example command_feedback_cli --example command_feedback_frames
python3 docs/examples/command-feedback/capture.py --output-dir target/command-feedback-capture
```

The script creates independent temporary daemon sessions, records raw I/O, and
stops both daemons. The explicit output directory preserves these historical
files. Running the current binary records current behavior rather than reproducing
the older per-frame complete views. Runtime PID and socket paths vary.

## EOF submits one complete frame

[stdin.jsonl](stdin.jsonl) contains four top-level action objects. The first is
formatted across several lines. The script supplies all input and closes stdin.

The actual [stdout](stdin.stdout.txt) contains **one view and four feedback
records**, including the third action's `invalid_arguments` rejection. The fourth
action still completes. The final count is 1, proving that the invalid action did
not save another note. [stderr](stdin.stderr.txt) is empty; exit status is 2.

| Observation | Measured value |
| --- | ---: |
| Input bytes | 183 |
| Stdout bytes | 2,857 |
| Complete views | 1 |
| Feedback records | 4 |
| Fixed context roots | 1 |
| Input schemas | 2 |
| Additional `action_result` copies | 0 |
| Whole foreground run | 195.320 ms |

The complete view still contains the context, current state, and both Action
schemas once. The CLI no longer prints the same callback output separately in
`action_result`; full JSON results remain in feedback and the programmatic API.
This is complete delivery for this frame, not native Frame delta delivery.

The subsequent [status](status.stdout.txt) preserves all four results and the
application state. The [stop](stop.stdout.txt) output confirms cleanup. Closing
the foreground's stdin did not stop the daemon.

## Persistent stdin submits successive frames

The second run keeps one process and its stdin open. It sends a complete batch
with a final newline, reads its entire view, then sends the next batch. No
additional frame marker is inserted.

| Frame | Submission boundary | Input | Results | Write-to-view elapsed |
| --- | --- | --- | ---: | ---: |
| 1 | Final newline + input idle | [`record` and `inspect`](interactive-frame-01.stdin.json) | 2 | 1,027.673 ms |
| 2 | Final newline + input idle | [`record`](interactive-frame-02.stdin.json) | 1 | 183.417 ms |
| 3 | EOF immediately after input | [`inspect`](interactive-frame-03.stdin.json) | 1 | 10.440 ms |

Full response files are [frame 1](interactive-frame-01.stdout.txt),
[frame 2](interactive-frame-02.stdout.txt), and
[frame 3](interactive-frame-03.stdout.txt). The complete concatenated stdout is
[interactive.stdout.txt](interactive.stdout.txt); its stderr is empty and the
foreground exits with status 0. [summary.json](summary.json) retains the timing
and byte measurements.

These are individual samples of the whole interaction, including fresh process
and daemon startup, rather than measurements of timer precision.

While stdin is open, the buffer's final byte must be a newline and no new bytes
must arrive for 100 ms to submit a frame. New bytes reset the timer. Without the
final newline, even complete JSON remains buffered. The boundary is independent
of JSON validity: after it is fixed, incomplete or malformed JSON rejects the
whole frame before any action runs. An empty buffer does not create repeated
views. EOF submits the remaining frame immediately, without requiring a newline.

A process cannot see `write_stdin` tool-call boundaries in its byte stream. A
long-lived caller must send a complete batch and wait for its view before sending
the next batch. Multiple writes made before the idle boundary may share a frame.
For a short invocation, EOF submits the remaining batch immediately.

## Native delivery remains a separate path

The [native summary](native/summary.json) and exact JSON/rendered files under
`native/` reproduce the four-frame `DebugProviderPort` example. They still show
ordinary unchanged POM omission, complete replacement feedback batches, and no
keyed removal replay. Neither CLI run calls a model or provider HTTP API.

## Functional checks

Build the CLI fixture before running the protocol tests:

```sh
cargo build --locked --no-default-features --example command_feedback_cli
cargo test --locked --no-default-features --test component_api_stdin_actions --test component_api_stdin_application
cargo test --locked --no-default-features --lib component::execution::stdin::
```

These checks cover real long-lived stdin, timer resets, missing final newlines,
EOF flush, incomplete-frame rejection and recovery, typed action rejection,
batch ordering, caller cancellation, and error exit with stdin still open.
They assert functional behavior and structured outcomes;
they do not test prompt wording or use these captures as snapshot expectations.
