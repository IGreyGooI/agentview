#!/usr/bin/env python3
"""Probe freeform custom-tool calls through OpenRouter and LiteLLM Responses APIs."""

import argparse
import json
import os
import ssl
import sys
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET


TARGET_XML = """<screen>
  <text>Hello world</text>
  <button>Continue</button>
</screen>"""

PROMPT = f"""Call the render_ui tool exactly once. Pass exactly this XML as the raw tool input.
Do not wrap it in JSON and do not add any extra text.

{TARGET_XML}"""

OPENROUTER_MODELS = (
    "qwen/qwen3.8-2.4t-a95b",
    "deepseek/deepseek-v4-pro-0813",
    "z-ai/glm-5.3",
    "moonshotai/kimi-k3",
    "minimax/minimax-m3",
)


def build_payload(model, tool_kind):
    if tool_kind == "custom":
        tool = {
            "type": "custom",
            "name": "render_ui",
            "description": (
                "Render a UI from raw XML. Pass the XML document directly "
                "with no JSON wrapper and no extra text."
            ),
            "format": {"type": "text"},
        }
    elif tool_kind == "function":
        tool = {
            "type": "function",
            "name": "render_ui",
            "description": "Render a UI from an XML document.",
            "parameters": {
                "type": "object",
                "properties": {"xml": {"type": "string"}},
                "required": ["xml"],
                "additionalProperties": False,
            },
            "strict": True,
        }
    else:
        raise ValueError(f"unknown tool kind: {tool_kind}")

    return {
        "model": model,
        "input": PROMPT,
        "tools": [tool],
        "tool_choice": "required",
        "stream": False,
        "store": False,
        "max_output_tokens": 1024,
    }


def _is_well_formed_xml(value):
    if not isinstance(value, str):
        return False
    try:
        ET.fromstring(value)
    except ET.ParseError:
        return False
    return True


def _api_error_message(decoded, raw):
    if isinstance(decoded, dict):
        error = decoded.get("error")
        if isinstance(error, dict) and isinstance(error.get("message"), str):
            return error["message"]
        if isinstance(error, str):
            return error
        if isinstance(decoded.get("message"), str):
            return decoded["message"]
    return raw.strip() or "empty error response"


def analyze_response(status, raw):
    result = {
        "native_tool_call": False,
        "tool_call_count": 0,
        "call_type": None,
        "tool_name": None,
        "tool_input": None,
        "input_kind": None,
        "xml_well_formed": False,
        "xml_exact_match": False,
        "verdict": "INVALID_RESPONSE",
        "error": None,
    }

    try:
        decoded = json.loads(raw)
    except (json.JSONDecodeError, TypeError):
        if not 200 <= status < 300:
            result["verdict"] = "API_ERROR"
            result["error"] = raw.strip() or f"HTTP {status}"
        return result

    if not 200 <= status < 300:
        result["verdict"] = "API_ERROR"
        result["error"] = _api_error_message(decoded, raw)
        return result

    if not isinstance(decoded, dict) or not isinstance(decoded.get("output"), list):
        return result

    calls = [
        item
        for item in decoded["output"]
        if isinstance(item, dict)
        and item.get("type") in {"custom_tool_call", "function_call"}
    ]
    if not calls:
        result["verdict"] = "NO_TOOL_CALL"
        return result

    result["tool_call_count"] = len(calls)
    call = calls[0]
    result["native_tool_call"] = True
    result["call_type"] = call["type"]
    result["tool_name"] = call.get("name")

    if call["type"] == "custom_tool_call":
        tool_input = call.get("input")
        result["input_kind"] = "freeform"
        result["tool_input"] = tool_input
        result["xml_well_formed"] = _is_well_formed_xml(tool_input)
        result["xml_exact_match"] = tool_input == TARGET_XML
        if result["tool_name"] != "render_ui":
            result["verdict"] = "WRONG_TOOL_NAME"
        elif result["xml_exact_match"]:
            result["verdict"] = "FREEFORM_XML_OK"
        else:
            result["verdict"] = "FREEFORM_XML_MISMATCH"
        if result["tool_call_count"] != 1:
            result["verdict"] = "MULTIPLE_TOOL_CALLS"
        return result

    arguments = call.get("arguments")
    parsed_arguments = None
    if isinstance(arguments, str):
        try:
            parsed_arguments = json.loads(arguments)
        except json.JSONDecodeError:
            pass
    elif isinstance(arguments, dict):
        parsed_arguments = arguments

    tool_input = None
    if isinstance(parsed_arguments, dict):
        candidate = parsed_arguments.get("xml")
        if isinstance(candidate, str):
            tool_input = candidate

    result["input_kind"] = "json"
    result["tool_input"] = tool_input if tool_input is not None else arguments
    result["xml_well_formed"] = _is_well_formed_xml(tool_input)
    result["xml_exact_match"] = tool_input == TARGET_XML
    if parsed_arguments is None:
        result["verdict"] = "MALFORMED_JSON_ARGUMENTS"
    elif not result["xml_exact_match"]:
        result["verdict"] = "JSON_ARGUMENTS_MISMATCH"
    else:
        result["verdict"] = "JSON_ARGUMENTS_REQUIRED"
    if result["tool_name"] != "render_ui":
        result["verdict"] = "WRONG_TOOL_NAME"
    if result["tool_call_count"] != 1:
        result["verdict"] = "MULTIPLE_TOOL_CALLS"
    return result


def _post_json(url, api_key, payload, timeout, verify_tls):
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    context = None
    if url.startswith("https://"):
        context = (
            ssl.create_default_context()
            if verify_tls
            else ssl._create_unverified_context()
        )

    try:
        with urllib.request.urlopen(request, timeout=timeout, context=context) as response:
            return response.status, response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", errors="replace")


def _print_analysis(result):
    yes_no = lambda value: "YES" if value else "NO"
    print("native tool call happened:", yes_no(result["native_tool_call"]))
    print("tool call count:", result["tool_call_count"])
    print("tool call type:", result["call_type"] or "(none)")
    print("tool name:", result["tool_name"] or "(none)")
    print("input transport:", result["input_kind"] or "(none)")
    print("tool input:")
    print(result["tool_input"] if result["tool_input"] is not None else "(none)")
    print("well-formed XML:", yes_no(result["xml_well_formed"]))
    print("exact XML match:", yes_no(result["xml_exact_match"]))
    if result["error"]:
        print("API error:", result["error"])
    print("verdict:", result["verdict"])


def _run_attempt(target, tool_kind, timeout):
    print(f"\n--- {tool_kind.upper()} REQUEST ---")
    payload = build_payload(target["model"], tool_kind)
    print(json.dumps(payload, indent=2, ensure_ascii=False))
    try:
        status, raw = _post_json(
            target["url"],
            target["api_key"],
            payload,
            timeout,
            target["verify_tls"],
        )
    except (OSError, urllib.error.URLError) as error:
        print("\n--- RAW RESPONSE ---")
        print("(no HTTP response)")
        print("transport error:", error)
        return {"verdict": "TRANSPORT_ERROR", "native_tool_call": False}

    print(f"\n--- RAW RESPONSE (HTTP {status}) ---")
    print(raw)
    result = analyze_response(status, raw)
    print("\n--- ANALYSIS ---")
    _print_analysis(result)
    return result


def should_run_json_control(custom_result):
    return (
        custom_result.get("call_type") != "custom_tool_call"
        and custom_result.get("verdict") != "TRANSPORT_ERROR"
    )


def _targets(args):
    targets = []
    if args.gateway in {"all", "openrouter"}:
        models = args.openrouter_model or list(OPENROUTER_MODELS)
        key = os.getenv("OPENROUTER_API_KEY")
        for model in models:
            targets.append(
                {
                    "gateway": "OpenRouter",
                    "model": model,
                    "url": os.getenv(
                        "OPENROUTER_RESPONSES_URL",
                        "https://openrouter.ai/api/v1/responses",
                    ),
                    "api_key": key,
                    "api_key_env": "OPENROUTER_API_KEY",
                    "verify_tls": True,
                }
            )
    if args.gateway in {"all", "litellm"}:
        targets.append(
            {
                "gateway": "LiteLLM reference",
                "model": args.litellm_model,
                "url": os.getenv(
                    "LITELLM_RESPONSES_URL",
                    "https://10.8.8.139:4000/v1/responses",
                ),
                "api_key": os.getenv("LITELLM_API_KEY"),
                "api_key_env": "LITELLM_API_KEY",
                "verify_tls": not args.insecure_litellm,
            }
        )
    return targets


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--gateway",
        choices=("all", "openrouter", "litellm"),
        default="all",
        help="gateways to probe (default: all)",
    )
    parser.add_argument(
        "--openrouter-model",
        action="append",
        help="replace the default OpenRouter set; repeat for multiple models",
    )
    parser.add_argument("--litellm-model", default="gpt-5.6")
    parser.add_argument(
        "--insecure-litellm",
        action="store_true",
        help="disable TLS verification for the private LiteLLM endpoint",
    )
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument(
        "--no-json-control",
        action="store_true",
        help="do not try a JSON function tool after a failed freeform call",
    )
    args = parser.parse_args(argv)

    summaries = []
    for target in _targets(args):
        print("\n" + "=" * 80)
        print(f"gateway: {target['gateway']}")
        print(f"model: {target['model']}")
        print(f"endpoint: {target['url']}")
        if not target["api_key"]:
            verdict = f"SKIPPED_MISSING_{target['api_key_env']}"
            print("verdict:", verdict)
            summaries.append((target["gateway"], target["model"], verdict))
            continue
        if not target["verify_tls"]:
            print("warning: TLS certificate verification is disabled for this endpoint")

        custom = _run_attempt(target, "custom", args.timeout)
        verdict = custom["verdict"]
        if (
            not args.no_json_control
            and should_run_json_control(custom)
        ):
            control = _run_attempt(target, "function", args.timeout)
            if control["verdict"] == "JSON_ARGUMENTS_REQUIRED":
                verdict = "JSON_ONLY"
            else:
                verdict = f"{verdict}; CONTROL_{control['verdict']}"
        summaries.append((target["gateway"], target["model"], verdict))

    print("\n" + "=" * 80)
    print("SUMMARY")
    for gateway, model, verdict in summaries:
        print(f"{gateway} | {model} | {verdict}")

    incomplete = not summaries or any(
        verdict.startswith("SKIPPED_") or "TRANSPORT_ERROR" in verdict
        for _, _, verdict in summaries
    )
    if incomplete:
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
