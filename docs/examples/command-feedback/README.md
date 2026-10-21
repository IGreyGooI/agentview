# Real command feedback captures

The files in this directory preserve the original per-action output history.
For the current two-second idle / EOF framing, see
[current captures](current/README.md). Running `capture.py` now writes there and
does not overwrite the original history below.

These files were captured on 2026-10-17 from the working AgentView implementation
using the [shared notebook Component](../../../examples/command_feedback/app.rs).
Both examples run locally without a model or credentials. These are reproducible
example outputs, not prompt snapshots used as unit-test expectations.

## Reproduce

Run from the repository root:

```sh
cargo build --locked --no-default-features --example command_feedback_cli --example command_feedback_frames
python3 docs/examples/command-feedback/capture.py
```

The current script uses fresh temporary sockets, captures stdout and stderr unchanged,
observes the retained session, and stops its daemon in `finally`. It then runs the
native Frame example. Use `--bin-dir PATH` for a custom Cargo target directory or
`--output-dir PATH` to keep a separate capture. Session PID and socket paths in
raw stdout vary between runs. The original counts below are UTF-8 bytes, not token estimates.

## Historical stdin: one complete response per action

[stdin.jsonl](stdin.jsonl) contains four requests:

```jsonl
{"action":"record","input":{"text":"SQLite keeps long-term memory"}}
{"action":"inspect","input":{}}
{"action":"record","input":{"text":42}}
{"action":"inspect","input":{}}
```

The exact [stdout](stdin.stdout.txt) is 8,739 bytes; [stderr](stdin.stderr.txt)
is empty. Exit status is 2 because request 3 has invalid typed arguments.
Request 4 still runs and reports the unchanged notebook, with `count=1`.

| Request | Outcome | Response bytes | Input schemas | Feedback records |
| --- | --- | ---: | ---: | ---: |
| 1: `record` | Completed, count 1 | 2,171 | 2 | 1 |
| 2: `inspect` | Completed, same result JSON | 2,172 | 2 | 1 |
| 3: `record` with numeric text | Rejected, `invalid_arguments` | 2,224 | 2 | 1 |
| 4: `inspect` | Completed, count still 1 | 2,172 | 2 | 1 |

Each response includes the fixed `notebook_context`, current `notebook_state`,
both Action schemas, and current feedback. Successful result JSON appears both
in `<action_result>` and `<command_result>`. The rejected request exposes the
same diagnostic in its direct result and feedback. This is the current stdin
behavior before stdin framing changed; `#[diff]` alone did not make that path incremental.

The [status observation](status.stdout.txt) preserves record 4 without invoking
an action or printing a new `<action_result>`. The [stop response](stop.stdout.txt)
marks the daemon stopped and includes its final snapshot. Their stderr files are
also retained. [summary.json](summary.json) records measured counts and exit codes.

## Native Frames: delivery comparison

The [Frame example](../../../examples/command_feedback_frames.rs) mounts the same
Component with `Application::mount_with_commands` and captures actual handoffs
using `DebugProviderPort`. It uses a separate in-memory notebook from the CLI.

| Frame | Actions since last handoff | Delivered POM | Feedback records | View bytes | Frame JSON bytes |
| --- | --- | --- | ---: | ---: | ---: |
| [1](native/frame-01.view.txt) | `record`, `inspect` | Context, state, both schemas, complete feedback | 2 | 1,697 | 4,231 |
| [2](native/frame-02.view.txt) | None | No projection items | 0 | 0 | 107 |
| [3](native/frame-03.view.txt) | `inspect` | Complete new feedback | 1 | 228 | 884 |
| [4](native/frame-04.view.txt) | `record` | Changed state and complete new feedback | 1 | 371 | 1,436 |

Frame 1 preserves both independent results even though their JSON is equal.
Frame 2 omits the unchanged feedback, state, and schemas; its empty rendered file
still corresponds to a real JSON Frame envelope. Frame 3 returns the same JSON
again but delivers a new result with sequence 3. Frame 4 omits unchanged context
and schemas while delivering the updated state and result 4.

Every changed feedback batch is complete. None of these Frames generates keyed
`insert`/`remove` patches that repeat previous result bodies. Components still
declare complete current state on every render; the native delivery layer owns
the comparison against successfully handed-off state.

Exact canonical payloads are [frame-01.json](native/frame-01.json),
[frame-02.json](native/frame-02.json), [frame-03.json](native/frame-03.json), and
[frame-04.json](native/frame-04.json). The [native summary](native/summary.json)
also lists root names and projection item counts. Rendered POM byte counts differ
from canonical JSON and from provider HTTP payloads; no provider HTTP call was
made by these examples.

For the implementation rules and frontend boundaries, see
[command feedback and delivery](../../command-feedback-delivery.md).
