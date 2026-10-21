#!/usr/bin/env python3
"""Capture real example I/O, without a model, credentials, or prompt snapshots."""

import argparse
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[3]
INPUT = (
    '{\n  "action": "record",\n  "input": {"text": "SQLite keeps long-term memory"}\n}\n'
    '{"action":"inspect","input":{}}\n'
    '{"action":"record","input":{"text":42}}\n'
    '{"action":"inspect","input":{}}\n'
).encode()


def capture(cli, directory, socket, name, arguments, stdin=b""):
    process = subprocess.run(
        [str(cli), "--socket", str(socket), *arguments],
        input=stdin,
        capture_output=True,
        timeout=30,
        check=False,
    )
    (directory / f"{name}.stdout.txt").write_bytes(process.stdout)
    (directory / f"{name}.stderr.txt").write_bytes(process.stderr)
    return process


def interactive_capture(cli, directory, socket):
    """Write one whole batch, await its view, then write the next batch."""
    process = subprocess.Popen(
        [str(cli), "--socket", str(socket)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    events = []
    pending = bytearray()
    all_output = bytearray()
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    started = time.monotonic()

    def receive_view():
        deadline = time.monotonic() + 30
        while True:
            end = pending.find(b"\n</view>\n")
            if end >= 0:
                end += len(b"\n</view>\n")
                view = bytes(pending[:end])
                del pending[:end]
                return view
            if not selector.select(max(0, deadline - time.monotonic())):
                raise TimeoutError("no complete view from retained stdin session")
            chunk = os.read(process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("stdout closed before the expected view")
            pending.extend(chunk)
            all_output.extend(chunk)

    batches = [
        ("idle", '{\n  "action": "record",\n  "input": {"text": "first interactive note"}\n}\n{"action":"inspect","input":{}}\n'),
        ("idle", '{\n  "action": "record",\n  "input": {"text": "second interactive note"}\n}\n'),
        ("eof", '{"action":"inspect","input":{}}'),
    ]
    try:
        for number, (boundary, text) in enumerate(batches, 1):
            data = text.encode()
            (directory / f"interactive-frame-{number:02}.stdin.json").write_bytes(data)
            sent = time.monotonic()
            process.stdin.write(data)
            process.stdin.flush()
            if boundary == "eof":
                process.stdin.close()
            view = receive_view()
            received = time.monotonic()
            (directory / f"interactive-frame-{number:02}.stdout.txt").write_bytes(view)
            events.append({
                "frame": number,
                "boundary": boundary,
                "sent_at_ms": round((sent - started) * 1000, 3),
                "received_at_ms": round((received - started) * 1000, 3),
                "elapsed_ms": round((received - sent) * 1000, 3),
                "input_bytes": len(data),
                "stdout_bytes": len(view),
                "feedback_records": len(re.findall(rb"<command_result(?:\s|>)", view)),
                "direct_action_results": view.count(b"<action_result "),
                "input_schemas": view.count(b"<input_schema>"),
                "static_context_roots": view.count(b"<notebook_context>"),
                "state_roots": view.count(b"<notebook_state>"),
                "connection_metadata": view.count(b"<connection "),
            })
        process.wait(timeout=30)
        trailing = process.stdout.read()
        all_output.extend(trailing)
        if pending or trailing:
            raise RuntimeError("unexpected extra output after the three frame responses")
        stderr = process.stderr.read()
        (directory / "interactive.stdout.txt").write_bytes(all_output)
        (directory / "interactive.stderr.txt").write_bytes(stderr)
        if process.returncode != 0:
            raise RuntimeError(f"interactive CLI exited with {process.returncode}")
        return {"exit_code": process.returncode, "frames": events}
    finally:
        selector.close()
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/debug/examples")
    parser.add_argument("--output-dir", type=Path, default=Path(__file__).resolve().parent / "incremental")
    args = parser.parse_args()
    directory = args.output_dir.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    cli = args.bin_dir.resolve() / "command_feedback_cli"
    frames = args.bin_dir.resolve() / "command_feedback_frames"
    if not cli.is_file() or not frames.is_file():
        parser.error("build command_feedback_cli and command_feedback_frames first")

    (directory / "stdin.jsonl").write_bytes(INPUT)
    with tempfile.TemporaryDirectory(prefix="av-feedback-") as temporary:
        socket = Path(temporary) / "eof/socket"
        try:
            eof_started = time.monotonic()
            actions = capture(cli, directory, socket, "stdin", [], INPUT)
            eof_elapsed_ms = round((time.monotonic() - eof_started) * 1000, 3)
            observed = capture(cli, directory, socket, "status", ["status"])
        finally:
            stopped = capture(cli, directory, socket, "stop", ["stop"])
        interactive_socket = Path(temporary) / "interactive/socket"
        try:
            interactive = interactive_capture(cli, directory, interactive_socket)
            interactive_status = capture(cli, directory, interactive_socket, "interactive-status", ["status"])
        finally:
            interactive_stop = capture(cli, directory, interactive_socket, "interactive-stop", ["stop"])
        if interactive_status.returncode != 0 or interactive_stop.returncode != 0:
            raise RuntimeError("interactive daemon cleanup failed")

    # These are example protocol checks, not tests of prompt wording or layout.
    if actions.returncode != 2 or observed.returncode != 0 or stopped.returncode != 0:
        raise RuntimeError("unexpected CLI exit status; inspect captured stderr")
    native = subprocess.run(
        [str(frames), str(directory / "native")],
        capture_output=True,
        timeout=30,
        check=False,
    )
    (directory / "frames.stdout.txt").write_bytes(native.stdout)
    (directory / "frames.stderr.txt").write_bytes(native.stderr)
    native.check_returncode()

    views = re.findall(rb'^<view ok="(?:true|false)">\n.*?^</view>\n', actions.stdout, re.M | re.S)
    summary = {
        "stdin": {
            "input_bytes": len(INPUT),
            "stdout_bytes": len(actions.stdout),
            "stderr_bytes": len(actions.stderr),
            "exit_code": actions.returncode,
            "elapsed_ms": eof_elapsed_ms,
            "response_count": len(views),
            "responses": [
                {
                    "response": number,
                    "bytes": len(view),
                    "ok": view.startswith(b'<view ok="true">'),
                    "direct_action_results": view.count(b"<action_result "),
                    "feedback_records": len(re.findall(rb"<command_result(?:\s|>)", view)),
                    "input_schemas": view.count(b"<input_schema>"),
                    "static_context_roots": view.count(b"<notebook_context>"),
                }
                for number, view in enumerate(views, 1)
            ],
        },
        "status": {
            "exit_code": observed.returncode,
            "stdout_bytes": len(observed.stdout),
        },
        "stop": {
            "exit_code": stopped.returncode,
            "stdout_bytes": len(stopped.stdout),
        },
        "native": json.loads((directory / "native/summary.json").read_text()),
        "interactive": interactive,
        "interactive_fresh_reader": {
            "exit_code": interactive_status.returncode,
            "stdout_bytes": len(interactive_status.stdout),
            "input_schemas": interactive_status.stdout.count(b"<input_schema>"),
            "static_context_roots": interactive_status.stdout.count(b"<notebook_context>"),
            "state_roots": interactive_status.stdout.count(b"<notebook_state>"),
            "feedback_records": len(re.findall(rb"<command_result(?:\s|>)", interactive_status.stdout)),
        },
    }
    encoded = json.dumps(summary, indent=2) + "\n"
    (directory / "summary.json").write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
