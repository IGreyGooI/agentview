# Incremental stdin delivery captures

Actual credential-free example runs captured on 2026-10-17. The CLI keeps one
daemon connection for successive input batches; each connection uses its own
Frame compiler baseline. The [earlier captures](../current/README.md) remain
unchanged for comparison.

## Successive batches on one open stdin

The script writes each complete batch and waits for its view before sending the
next. The first two batches end with a newline and submit after 100 ms without
new bytes. The last batch submits immediately at EOF.

| Batch | Input | Stdout bytes | Schemas | Fixed context | State roots | Feedback records | Write-to-view |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | [`record`, `inspect`](interactive-frame-01.stdin.json) | 2,589 | 2 | 1 | 1 | 2 | 161.292 ms |
| 2 | [`record`](interactive-frame-02.stdin.json) | 345 | 0 | 0 | 1 | 1 | 123.878 ms |
| 3 | [`inspect`](interactive-frame-03.stdin.json) | 248 | 0 | 0 | 0 | 1 | 14.003 ms |

The [initial stdout](interactive-frame-01.stdout.txt) supplies context, schemas,
state, and both action results. Both actions return equal JSON values and remain
separate feedback records.

The [second stdout](interactive-frame-02.stdout.txt) contains the changed notebook
state and the new feedback batch. It omits the instructions, schemas, and
connection metadata already delivered on this connection. The entire stdout is:

```xml
<view ok="true">
<notebook_state>
  <count>2</count>
  <latest>second interactive note</latest>
</notebook_state>

<command_feedback>
  <sequence>3</sequence>
  <records>
    <command_result sequence="3" command="record" status="completed">{"count":2,"latest":"second interactive note"}</command_result>
  </records>
</command_feedback>
</view>
```

The [third stdout](interactive-frame-03.stdout.txt) contains only the new
`inspect` feedback. The state is unchanged and is omitted; the result JSON equals
the previous action's result but still receives a new record with sequence 4.
There are no extra `<action_result>` copies in any batch.

The [concatenated stdout](interactive.stdout.txt) and [stderr](interactive.stderr.txt)
preserve the whole interaction. The foreground exits with status 0. These timings
are individual end-to-end samples, including input idle and daemon startup when
needed; they do not measure timer precision.

## A new reader receives the complete view

After the interactive frontend exits, the script starts a separate `status`
process against the same daemon. Its [stdout](interactive-status.stdout.txt)
contains the current count of 2, fixed context, both schemas, and the latest
feedback. It is 2,459 bytes. A connection's delivery baseline therefore does not
hide required content from a new reader. The [stop response](interactive-stop.stdout.txt)
confirms cleanup of the retained daemon.

## EOF submits all action results together

The independent [EOF input](stdin.jsonl) contains four top-level objects,
including one multiline object and one invalid typed input. The [stdout](stdin.stdout.txt)
contains one complete initial view, four feedback records in input order, two
schemas, and no direct-result copy. The later valid action still runs after the
typed rejection. Stdout is 2,899 bytes, stderr is empty, and exit status is 2.
The [status response](status.stdout.txt) preserves the state and all four results;
the [stop response](stop.stdout.txt) confirms cleanup.

## Canonical Frame comparison

The [native summary](native/summary.json) and exact JSON/rendered captures in
`native/` also record the existing `DebugProviderPort` example: initial Full,
empty unchanged Delta, a new complete feedback batch, then changed state plus
new feedback. Stdin uses the same compiler rules to obtain the POM it renders.
These examples make no model or provider HTTP calls.

## Reproduce

With the example binaries already built, run from the repository root:

```sh
python3 docs/examples/command-feedback/capture.py --output-dir target/command-feedback-capture
```

Use another output directory to preserve this run. The [capture script](../capture.py)
records raw files and [summary.json](summary.json) without prompt snapshots.
Runtime PIDs, socket paths, and elapsed times vary. See the
[delivery guide](../../../command-feedback-delivery.md#deliver-stdin-frames-to-each-reader)
for the protocol contract.
