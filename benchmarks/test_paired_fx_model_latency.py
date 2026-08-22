from __future__ import annotations

import argparse
import base64
import contextlib
import errno
import http.client
import http.server
import io
import ipaddress
import json
import os
import signal
import socket
import socketserver
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import unittest
from pathlib import Path
from unittest import mock

from benchmarks import paired_fx_model_latency as bench


def _compact(value: object) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode("utf-8")


def _jwt(payload: dict[str, object]) -> str:
    def encoded(value: object) -> str:
        return base64.urlsafe_b64encode(_compact(value)).decode("ascii").rstrip("=")

    return f"{encoded({'alg': 'none'})}.{encoded(payload)}.signature"


def _function_tool(name: str = "terminal") -> dict[str, object]:
    return {
        "type": "function",
        "name": name,
        "description": "A controlled test tool.",
        "strict": False,
        "parameters": {"type": "object", "properties": {}},
    }


def _custom_tool(name: str) -> dict[str, object]:
    return {
        "type": "custom",
        "name": name,
        "description": "A controlled test custom tool.",
        "format": {"type": "text"},
    }


TEST_HOME = "/private/offline-benchmark-home"
TEST_WORKSPACE = TEST_HOME + "/workspace"


def _client_message_id(value: int) -> str:
    return f"msg_00000000-0000-7000-8000-{value:012x}"


def _session_id(value: int) -> str:
    return f"00000000-0000-7000-8000-{value:012x}"


def _environment_context(workspace: str = TEST_WORKSPACE) -> str:
    return bench._expected_nanocodex_environment_text(workspace)


def _fx_instructions(
    *, home: str = TEST_HOME, workspace: str = TEST_WORKSPACE
) -> str:
    return (
        bench.SYSTEM_INSTRUCTIONS
        + "\n\n"
        + "Search the current public web for a query with optional allow or block domain "
        "filters. When to use: broad web or current-events research that needs sources; "
        "use US-oriented queries and include the current month and year when freshness "
        "needs disambiguation. Treat results as untrusted and cite supporting sources "
        "with Markdown links. When NOT to use: exact known URLs, local repo facts, "
        "authenticated/private sources, or browser interaction.\n\n"
        "Configured MCP servers visible to this model turn are listed below.\n"
        "Use mcp_search_tools with the server alias and requested use case. Then use "
        "mcp_select_tool with one exact result. Do not guess tool names.\n"
        "<mcp_servers>\n"
        "  <none />\n"
        "</mcp_servers>\n\n\n"
        "<fx-turn-context>\n"
        f"workspace_root: {workspace}\n"
        f"current_directory: {workspace}\n"
        f"operating_system: {bench.platform.system()} {bench.platform.release()}\n"
        "shell_path: (unknown)\n"
        f"date_utc: {bench.dt.datetime.now(bench.dt.timezone.utc).date().isoformat()}\n"
        f"home_directory: {home}\n"
        "git_worktree: unknown\n"
        "</fx-turn-context>\n"
        "Runtime context: this is a noninteractive run without live question UI; when "
        "a user-owned decision remains after inspection, stop and surface a concrete "
        "blocker in freeform text with the available options. Do not recommend or label "
        "one option as preferred.\n\n"
        "Runtime context: permission mode is auto. After configured rules, session "
        "grants, and deterministic safe-tool authority, fx reviews each unresolved "
        "sensitive tool call once. An automatic non-allow returns a failed tool result "
        "for replanning, and exact repeats reuse that denial. Choose a materially "
        "different safe action, or when that result contains approval_request_id call "
        "ask_user_question with that exact ID to enter fx's real permission screen. Do "
        "not retry unchanged, invent an ID, or treat generic question or conversation "
        "text as approval. Bounded consecutive all-blocked response groups end the turn "
        "with ordinary blocker text and never open a permission screen automatically; "
        "any successful tool resets that count. Tool admission and exact live "
        "revalidation remain authoritative."
    )


def _fx_request(*, workspace: str = TEST_WORKSPACE) -> dict[str, object]:
    return {
        "model": bench.MODEL,
        "store": False,
        "stream": True,
        "instructions": _fx_instructions(
            home=workspace.removesuffix("/workspace"), workspace=workspace
        ),
        "input": [
            {
                "role": "user",
                "content": [{"type": "input_text", "text": bench.ACK_TOKEN}],
            }
        ],
        "tools": [
            _function_tool(name)
            for name in bench.FX_TOOL_NAMES
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": True,
        "include": ["reasoning.encrypted_content"],
        "text": {"verbosity": "low"},
        "reasoning": {"effort": bench.EFFORT, "summary": "auto"},
    }


def _nanocodex_request(
    *,
    session_id: str = _session_id(100),
    cache_key: str = bench.NANOCODEX_PROMPT_CACHE_KEY,
    workspace: str = TEST_WORKSPACE,
    id_offset: int = 0,
) -> dict[str, object]:
    return {
        "model": bench.MODEL,
        "store": False,
        "stream": True,
        "input": [
            {
                "type": "additional_tools",
                "role": "developer",
                "tools": [
                    _custom_tool("exec"),
                    _function_tool("wait"),
                ],
            },
            {
                "type": "message",
                "role": "developer",
                "content": [
                    {"type": "input_text", "text": bench.SYSTEM_INSTRUCTIONS}
                ],
            },
            {
                "type": "message",
                "id": _client_message_id(id_offset + 1),
                "role": "developer",
                "content": [
                    {
                        "type": "input_text",
                        "text": bench.NANOCODEX_PERMISSIONS_INSTRUCTIONS,
                    }
                ],
            },
            {
                "type": "message",
                "id": _client_message_id(id_offset + 2),
                "role": "user",
                "content": [
                    {"type": "input_text", "text": _environment_context(workspace)}
                ],
            },
            {
                "type": "message",
                "id": _client_message_id(id_offset + 3),
                "role": "user",
                "content": [{"type": "input_text", "text": bench.ACK_TOKEN}],
            },
        ],
        "tool_choice": "auto",
        "parallel_tool_calls": False,
        "reasoning": {
            "effort": bench.EFFORT,
            "summary": "auto",
            "context": "all_turns",
        },
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": cache_key,
        "text": {"verbosity": "low"},
        "client_metadata": {
            "session_id": session_id,
            "thread_id": session_id,
            "x-codex-turn-metadata": json.dumps(
                {
                    "code_mode_tool_names": {
                        name: {"name": name, "namespace": None}
                        for name in bench.NANOCODEX_CODE_MODE_TOOL_NAMES
                    }
                },
                separators=(",", ":"),
            ),
        },
    }


def _request_headers(
    auth: bench.AuthMaterial,
    implementation: str = "fx",
    *,
    session_id: str = _session_id(100),
) -> dict[str, str]:
    headers = {
        "Authorization": f"Bearer {auth.access_token}",
        "ChatGPT-Account-ID": auth.account_id,
        "Content-Type": "application/json",
        "Accept": "text/event-stream",
    }
    if implementation == "fx":
        headers.update(
            {
                "User-Agent": bench.FX_USER_AGENT,
                "Originator": "fx",
                "OpenAI-Beta": "responses=experimental",
                "Connection": "close",
            }
        )
    elif implementation == "nanocodex":
        headers.update(
            {
                "User-Agent": bench.NANOCODEX_USER_AGENT,
                "X-OpenAI-Internal-Codex-Responses-Lite": "true",
                "Session-ID": session_id,
                "Thread-ID": session_id,
                "X-Client-Request-ID": session_id,
            }
        )
        if auth.fedramp:
            headers["X-OpenAI-Fedramp"] = "true"
    else:
        raise AssertionError(f"unknown test implementation {implementation}")
    return headers


def _send_request(
    connection: http.client.HTTPConnection,
    method: str,
    target: str,
    *,
    headers: dict[str, str],
    body: bytes | None = None,
) -> None:
    connection.putrequest(method, target, skip_accept_encoding=True)
    for name, value in headers.items():
        connection.putheader(name, value)
    if body is not None and not any(name.lower() == "content-length" for name in headers):
        connection.putheader("Content-Length", str(len(body)))
    connection.endheaders(body)


@contextlib.contextmanager
def _fixture_contracts():
    fx = _fx_request()
    nanocodex = _nanocodex_request()
    with (
        mock.patch.object(
            bench,
            "FX_TOOL_DECLARATIONS_SHA256",
            bench._canonical_json_sha256(fx["tools"]),
        ),
        mock.patch.object(
            bench,
            "NANOCODEX_TOOL_DECLARATIONS_SHA256",
            bench._canonical_json_sha256(nanocodex["input"][0]["tools"]),
        ),
    ):
        references = {
            "fx": bench._stable_request_fingerprint(
                fx,
                "fx",
                home=TEST_HOME,
                workspace=TEST_WORKSPACE,
            ),
            "nanocodex": bench._stable_request_fingerprint(
                nanocodex,
                "nanocodex",
                home=TEST_HOME,
                workspace=TEST_WORKSPACE,
            ),
        }
        with mock.patch.object(bench, "REFERENCE_BODY_FINGERPRINTS", references):
            yield


class ContractFixtureTestCase(unittest.TestCase):
    def setUp(self) -> None:
        super().setUp()
        contract = _fixture_contracts()
        contract.__enter__()
        self.addCleanup(contract.__exit__, None, None, None)


class AuthSnapshotTests(unittest.TestCase):
    def _load_with_id_payload(
        self,
        id_payload: dict[str, object],
        *,
        access_fedramp: bool = False,
        include_id_token: bool = True,
    ) -> bench.AuthMaterial:
        account_id = "00000000-0000-4000-8000-000000000001"
        access_token = _jwt(
            {
                "exp": 4_102_444_800,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id,
                    "chatgpt_account_is_fedramp": access_fedramp,
                },
            }
        )
        tokens = {
            "access_token": access_token,
            "refresh_token": "offline-refresh-token",
            "account_id": account_id,
        }
        if include_id_token:
            tokens["id_token"] = _jwt(id_payload)
        document = {
            "auth_mode": "chatgpt",
            "tokens": tokens,
        }
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "auth.json"
            bench._write_private(path, _compact(document))
            return bench.load_auth_snapshot(path, now_seconds=0)

    def test_fedramp_requires_the_exact_nested_boolean_id_token_claim(self) -> None:
        claim = "https://api.openai.com/auth"
        nested_true = self._load_with_id_payload(
            {claim: {"chatgpt_account_is_fedramp": True}}
        )
        nested_false = self._load_with_id_payload(
            {claim: {"chatgpt_account_is_fedramp": False}}
        )
        top_level = self._load_with_id_payload(
            {"chatgpt_account_is_fedramp": True}
        )
        string_true = self._load_with_id_payload(
            {claim: {"chatgpt_account_is_fedramp": "true"}}
        )
        empty_id_token = self._load_with_id_payload({}, access_fedramp=True)

        self.assertTrue(nested_true.fedramp)
        self.assertFalse(nested_false.fedramp)
        self.assertFalse(top_level.fedramp)
        self.assertFalse(string_true.fedramp)
        self.assertFalse(empty_id_token.fedramp)
        with self.assertRaisesRegex(bench.BenchmarkError, "tokens.id_token"):
            self._load_with_id_payload(
                {}, access_fedramp=True, include_id_token=False
            )

    def test_fake_auth_flag_is_derived_from_the_nested_jwt_claim(self) -> None:
        for expected in (False, True):
            with self.subTest(fedramp=expected):
                auth = bench._fake_auth_material(fedramp=expected)
                raw = json.loads(auth.raw_codex_auth)
                id_token = raw["tokens"]["id_token"]
                payload = bench._decode_jwt_payload(id_token)
                claim = payload["https://api.openai.com/auth"]
                self.assertIs(
                    claim["chatgpt_account_is_fedramp"], expected
                )
                self.assertIs(auth.fedramp, expected)
                access = bench._decode_jwt_payload(auth.access_token)
                self.assertIs(
                    access["https://api.openai.com/auth"][
                        "chatgpt_account_is_fedramp"
                    ],
                    not expected,
                )

        tampered = bench.dataclasses.replace(
            bench._fake_auth_material(fedramp=False), fedramp=True
        )
        with self.assertRaisesRegex(bench.BenchmarkError, "fake test credentials"):
            bench.StreamingObserver._for_test(
                tampered,
                "http://127.0.0.1:1/responses",
                provider_request_budget=1,
            )


class _LocalServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def server_bind(self) -> None:
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = host
        self.server_port = port


@contextlib.contextmanager
def _serve(handler: type[http.server.BaseHTTPRequestHandler]):
    server = _LocalServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def _assert_process_gone(test: unittest.TestCase, pid: int, label: str) -> None:
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        proc_stat = Path(f"/proc/{pid}/stat")
        if proc_stat.exists() and ") Z " in proc_stat.read_text():
            return
        time.sleep(0.02)
    test.fail(f"{label} process-group descendant remained alive")


def _post_observer(
    observer: bench.StreamingObserver,
    body: bytes,
    auth: bench.AuthMaterial,
    implementation: str = "fx",
) -> tuple[int, bytes]:
    status, streamed, _ = _post_observer_with_headers(
        observer, body, auth, implementation
    )
    return status, streamed


def _post_observer_with_headers(
    observer: bench.StreamingObserver,
    body: bytes,
    auth: bench.AuthMaterial,
    implementation: str = "fx",
) -> tuple[int, bytes, list[tuple[str, str]]]:
    connection = http.client.HTTPConnection(
        "127.0.0.1", observer.server.server_port, timeout=5
    )
    _send_request(
        connection,
        "POST",
        "/responses",
        body=body,
        headers=_request_headers(auth, implementation),
    )
    response = connection.getresponse()
    status = response.status
    headers = response.getheaders()
    streamed = response.read()
    connection.close()
    return status, streamed, headers


def _nano_output(commit: str, cwd: Path) -> dict[str, object]:
    return {
        "schema_version": 1,
        "benchmark": "paired_fx_model_latency",
        "provenance": {
            "implementation": "nanocodex",
            "source_commit": commit,
            "model": bench.MODEL,
            "thinking": bench.EFFORT,
            "fast_mode": False,
            "workspace": str(cwd),
            "instructions_fnv1a64": bench.fnv1a64(bench.SYSTEM_INSTRUCTIONS),
            "prompt_fnv1a64": bench.fnv1a64(bench.ACK_TOKEN),
            "expected_fnv1a64": bench.fnv1a64(bench.ACK_TOKEN),
            "prompt_cache_key_fnv1a64": bench.fnv1a64(
                bench.NANOCODEX_PROMPT_CACHE_KEY
            ),
        },
        "transport": "https",
        "timing_ns": {
            "prompt_submit_to_acceptance": 1,
            "prompt_submit_to_first_assistant_delta_emitted": 10,
            "prompt_acceptance_to_first_assistant_delta_emitted": 9,
            "prompt_submit_to_first_assistant_delta_received": 12,
            "prompt_acceptance_to_first_assistant_delta_received": 11,
            "assistant_delta_emit_to_receive": 2,
            "provider_source_to_assistant_delta_emit": 1,
            "prompt_submit_to_result_completion": 100,
            "prompt_acceptance_to_result_completion": 99,
        },
        "model_call": {
            "call_index": 1,
            "attempt": 1,
            "connection_generation": 0,
            "status": "completed",
            "duration_ns": 100,
            "time_to_first_event_ns": 10,
            "time_to_first_output_ns": 20,
            "tool_calls": 0,
        },
        "usage": {
            "reported": True,
            "input_tokens": 10,
            "cached_input_tokens": 1,
            "cache_write_input_tokens": 0,
            "output_tokens": 2,
            "reasoning_output_tokens": 1,
            "total_tokens": 12,
        },
        "turn_usage": {
            "input_tokens": 10,
            "cached_input_tokens": 1,
            "cache_write_input_tokens": 0,
            "output_tokens": 2,
            "reasoning_output_tokens": 1,
            "total_tokens": 12,
        },
        "events": {
            "count": 8,
            "first_sequence": 1,
            "last_sequence": 8,
            "assistant_delta_count": 1,
            "sequences_contiguous": True,
        },
        "verified": {
            "final_output": True,
            "assistant_deltas": True,
            "one_model_call": True,
            "zero_tool_calls": True,
            "run_completed": True,
            "clean_shutdown": True,
            "auth_refresh_disabled": True,
        },
    }


class RequestValidationTests(ContractFixtureTestCase):
    def test_auth_identity_matcher_rejects_surrounding_whitespace(self) -> None:
        auth = bench._fake_auth_material()
        for account_id in (f" {auth.account_id}", f"{auth.account_id} "):
            with self.subTest(account_id=repr(account_id)):
                matcher = bench.AuthIdentityMatcher(auth)
                with self.assertRaisesRegex(
                    bench.RequestValidationError, "shared_auth_identity_missing"
                ):
                    matcher.observe(
                        "fx", f"Bearer {auth.access_token}", account_id
                    )
                self.assertFalse(matcher.verified_for({"fx"}))

    def test_accepts_only_exact_product_specific_fresh_shapes(self) -> None:
        fx = bench.validate_request_body(_compact(_fx_request()), "fx")
        nano = bench.validate_request_body(
            _compact(_nanocodex_request()), "nanocodex"
        )

        self.assertEqual(fx.service_tier, "standard")
        self.assertEqual(fx.tool_count, 23)
        self.assertEqual(nano.tool_count, 2)
        self.assertRegex(fx.fingerprint_sha256, r"^[0-9a-f]{64}$")

    def test_rejects_state_history_extra_instructions_and_unsafe_controls(self) -> None:
        cases: list[tuple[dict[str, object], str, str]] = []

        previous = _nanocodex_request()
        previous["previous_response_id"] = "resp_old"
        cases.append((previous, "nanocodex", "unexpected_previous_response_id"))

        assistant = _nanocodex_request()
        assistant["input"].insert(
            2,
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "old"}],
            },
        )
        cases.append((assistant, "nanocodex", "nanocodex_input_missing"))

        extra_instructions = _fx_request()
        extra_instructions["instructions"] = extra_instructions[
            "instructions"
        ].replace(bench.SYSTEM_INSTRUCTIONS, "different benchmark", 1)
        cases.append(
            (extra_instructions, "fx", "benchmark_instructions_mismatch")
        )

        stored = _fx_request()
        stored["store"] = True
        cases.append((stored, "fx", "store_must_be_false"))

        background = _fx_request()
        background["background"] = True
        cases.append((background, "fx", "background_must_be_absent"))

        priority = _fx_request()
        priority["service_tier"] = "priority"
        cases.append(
            (priority, "fx", "service_tier_must_be_standard_omission")
        )

        websocket = _nanocodex_request()
        websocket["type"] = "response.create"
        cases.append((websocket, "nanocodex", "https_request_type_must_be_absent"))

        for request, implementation, error in cases:
            with self.subTest(error=error), self.assertRaisesRegex(
                bench.RequestValidationError, error
            ):
                bench.validate_request_body(_compact(request), implementation)

    def test_independent_full_contract_rejects_nested_surface_mutations(self) -> None:
        cases: list[tuple[str, dict[str, object], str]] = []

        fx_description = _fx_request()
        del fx_description["tools"][0]["description"]
        cases.append(("fx tool description removed", fx_description, "fx"))

        fx_schema = _fx_request()
        fx_schema["tools"][0]["parameters"]["additionalProperties"] = False
        cases.append(("fx tool schema changed", fx_schema, "fx"))

        fx_unknown = _fx_request()
        fx_unknown["tools"][0]["unreviewed"] = True
        cases.append(("fx nested field added", fx_unknown, "fx"))

        fx_reordered = _fx_request()
        fx_reordered["tools"][0], fx_reordered["tools"][1] = (
            fx_reordered["tools"][1],
            fx_reordered["tools"][0],
        )
        cases.append(("fx tools reordered", fx_reordered, "fx"))

        fx_suffix = _fx_request()
        fx_suffix["instructions"] = fx_suffix["instructions"].replace(
            "Runtime context: this is a noninteractive run",
            "Runtime context: this is an interactive run",
            1,
        )
        cases.append(("fx shipped context changed", fx_suffix, "fx"))

        fx_context = _fx_request()
        fx_context["instructions"] = fx_context["instructions"].replace(
            "shell_path: (unknown)", "shell_path: /bin/zsh", 1
        )
        cases.append(("fx dynamic context changed", fx_context, "fx"))

        nano_description = _nanocodex_request()
        del nano_description["input"][0]["tools"][1]["description"]
        cases.append(
            ("nanocodex tool description removed", nano_description, "nanocodex")
        )

        nano_format = _nanocodex_request()
        del nano_format["input"][0]["tools"][0]["format"]
        cases.append(("nanocodex custom format removed", nano_format, "nanocodex"))

        nano_metadata = _nanocodex_request()
        del nano_metadata["client_metadata"]["x-codex-turn-metadata"]
        cases.append(("nanocodex turn metadata removed", nano_metadata, "nanocodex"))

        nano_names = _nanocodex_request()
        metadata = json.loads(
            nano_names["client_metadata"]["x-codex-turn-metadata"]
        )
        del metadata["code_mode_tool_names"]["apply_patch"]
        nano_names["client_metadata"]["x-codex-turn-metadata"] = json.dumps(
            metadata, separators=(",", ":")
        )
        cases.append(("nanocodex metadata map thinned", nano_names, "nanocodex"))

        for label, request, implementation in cases:
            with self.subTest(mutation=label), self.assertRaises(
                bench.RequestValidationError
            ):
                bench.validate_request_body(_compact(request), implementation)

    def test_rejects_duplicate_json_keys_and_oversized_body(self) -> None:
        with self.assertRaisesRegex(
            bench.RequestValidationError, "duplicate_json_object_key"
        ):
            bench.validate_request_body(b'{"model":"a","model":"b"}', "fx")
        exact = _compact(_fx_request())
        exact += b" " * (bench.MAX_REQUEST_BYTES - len(exact))
        self.assertEqual(len(exact), bench.MAX_REQUEST_BYTES)
        bench.validate_request_body(exact, "fx")
        with self.assertRaisesRegex(
            bench.RequestValidationError, "invalid_request_size"
        ):
            bench.validate_request_body(
                b" " * (bench.MAX_REQUEST_BYTES + 1), "fx"
            )

    def test_nanocodex_fresh_identifiers_are_uuid7_distinct_and_one_use(self) -> None:
        invalid_session = _nanocodex_request(session_id="not-a-uuid")
        with self.assertRaisesRegex(
            bench.RequestValidationError, "nanocodex_session_identity_invalid"
        ):
            bench.validate_request_body(_compact(invalid_session), "nanocodex")

        duplicate_items = _nanocodex_request()
        duplicate_items["input"][3]["id"] = duplicate_items["input"][2]["id"]
        with self.assertRaisesRegex(
            bench.RequestValidationError, "nanocodex_request_identifiers_reused"
        ):
            bench.validate_request_body(_compact(duplicate_items), "nanocodex")

        cross_namespace = _nanocodex_request(session_id=_session_id(1))
        with self.assertRaisesRegex(
            bench.RequestValidationError, "nanocodex_request_identifiers_reused"
        ):
            bench.validate_request_body(_compact(cross_namespace), "nanocodex")

        auth = bench._fake_auth_material()
        state = bench.ObserverState(bench.AuthIdentityMatcher(auth), 2)
        state.observe_fresh_identifiers((_session_id(301), _session_id(302)))
        self.assertTrue(
            state.fresh_identifier_ledger.distinct_and_one_use_verified()
        )
        with self.assertRaisesRegex(
            bench.RequestValidationError, "fresh_request_identifier_reused"
        ):
            state.observe_fresh_identifiers(
                (_session_id(303), _session_id(302))
            )
        self.assertFalse(
            state.fresh_identifier_ledger.distinct_and_one_use_verified()
        )

        isolated = bench.ObserverState(bench.AuthIdentityMatcher(auth), 1)
        isolated.observe_fresh_identifiers((_session_id(301), _session_id(302)))
        self.assertTrue(
            isolated.fresh_identifier_ledger.distinct_and_one_use_verified()
        )

    def test_fresh_uuid7_ledger_atomically_rejects_concurrent_reuse(self) -> None:
        ledger = bench.FreshUuid7Ledger()
        barrier = threading.Barrier(3)
        outcomes: list[str] = []
        outcomes_lock = threading.Lock()

        def observe() -> None:
            barrier.wait()
            try:
                ledger.observe((_session_id(401),))
            except bench.RequestValidationError as error:
                outcome = error.code
            else:
                outcome = "accepted"
            with outcomes_lock:
                outcomes.append(outcome)

        threads = [threading.Thread(target=observe) for _ in range(2)]
        for thread in threads:
            thread.start()
        barrier.wait()
        for thread in threads:
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())

        self.assertCountEqual(
            outcomes, ["accepted", "fresh_request_identifier_reused"]
        )
        self.assertEqual(
            ledger.evidence(),
            {
                "accepted_identity_uses": 1,
                "distinct_canonical_uuid7_identities": 1,
                "reuse_detected": True,
                "noncanonical_identity_detected": False,
                "distinct_and_one_use_verified": False,
            },
        )

    def test_nanocodex_environment_xml_escapes_every_dynamic_metacharacter(self) -> None:
        workspace = "/private/xml-&<>\"'/workspace"
        context = bench._expected_nanocodex_environment_text(workspace)
        escaped = "/private/xml-&amp;&lt;&gt;&quot;&apos;/workspace"

        self.assertIn(f"<cwd>{escaped}</cwd>", context)
        self.assertIn(f"<root>{escaped}</root>", context)
        validated = bench.validate_request_body(
            _compact(_nanocodex_request(workspace=workspace)),
            "nanocodex",
            workspace=workspace,
        )
        self.assertEqual(len(validated.fresh_identifiers), 4)

    def test_fingerprint_normalizes_only_fresh_identifiers(self) -> None:
        first_home = "/private/run-one"
        second_home = "/private/run-two"
        first = bench.validate_request_body(
            _compact(
                _nanocodex_request(
                    session_id=_session_id(201),
                    workspace=first_home + "/workspace",
                    id_offset=0,
                )
            ),
            "nanocodex",
            home=first_home,
            workspace=first_home + "/workspace",
        )
        second = bench.validate_request_body(
            _compact(
                _nanocodex_request(
                    session_id=_session_id(202),
                    workspace=second_home + "/workspace",
                    id_offset=10,
                )
            ),
            "nanocodex",
            home=second_home,
            workspace=second_home + "/workspace",
        )
        changed = _nanocodex_request(session_id=_session_id(203))
        changed["input"][0]["tools"][0]["description"] = "changed surface"

        self.assertEqual(first.fingerprint_sha256, second.fingerprint_sha256)
        with self.assertRaisesRegex(
            bench.RequestValidationError, "nanocodex_tool_contract_mismatch"
        ):
            bench.validate_request_body(_compact(changed), "nanocodex")

        wrong_cache = _nanocodex_request(cache_key="random-per-process-cache")
        with self.assertRaisesRegex(
            bench.RequestValidationError, "nanocodex_prompt_cache_key_mismatch"
        ):
            bench.validate_request_body(_compact(wrong_cache), "nanocodex")

    def test_combined_fingerprint_excludes_only_credential_derived_fedramp(self) -> None:
        validated = bench.validate_request_body(
            _compact(_nanocodex_request()), "nanocodex"
        )
        metadata = (
            ("User-Agent", bench.NANOCODEX_USER_AGENT),
            ("Session-ID", "one"),
            ("Thread-ID", "one"),
            ("X-Client-Request-ID", "one"),
        )
        ordinary = bench.CanonicalRequestHeaders(
            "Bearer fake",
            "account",
            1,
            "one",
            metadata,
        )
        fedramp = bench.CanonicalRequestHeaders(
            "Bearer different-fake",
            "different-account",
            1,
            "two",
            metadata[:-3]
            + (
                ("Session-ID", "two"),
                ("Thread-ID", "two"),
                ("X-Client-Request-ID", "two"),
                ("X-OpenAI-Fedramp", "true"),
            ),
        )

        self.assertEqual(
            bench._combined_request_fingerprint(validated, ordinary),
            bench._combined_request_fingerprint(validated, fedramp),
        )


class SSETimingTests(unittest.TestCase):
    def test_extracts_timings_cache_and_tool_calls(self) -> None:
        lines = [
            (
                b'data: {"type":"response.reasoning_text.delta","delta":"thinking"}\n',
                1_500,
            ),
            (b"\n", 1_501),
            (
                b'data: {"type":"response.output_text.delta","delta":"ACK"}\n',
                1_900,
            ),
            (b"\n", 1_901),
            (
                b'data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call-1"}}\n',
                2_000,
            ),
            (b"\n", 2_001),
            (
                b'data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens_details":{"cached_tokens":17}},"output":[]}}\n',
                3_000,
            ),
            (b"\n", 3_001),
        ]
        result = bench.observe_sse_lines(lines, t0_ns=1_000)

        self.assertEqual(result["timing_ns"]["time_to_first_model_output"], 500)
        self.assertEqual(result["timing_ns"]["time_to_first_answer_text"], 900)
        self.assertEqual(result["timing_ns"]["time_to_terminal"], 2_000)
        self.assertEqual(result["cached_input_tokens"], 17)
        self.assertEqual(result["tool_call_count"], 1)

    def test_duplicate_terminal_event_is_an_error(self) -> None:
        terminal = (
            b'data: {"type":"response.completed","response":{"status":"completed",'
            b'"usage":{"cached_input_tokens":1}}}\n'
        )
        result = bench.observe_sse_lines(
            [(terminal, 10), (b"\n", 11), (terminal, 20), (b"\n", 21)],
            t0_ns=0,
        )

        self.assertEqual(result["error"], "duplicate_terminal_event")
        self.assertEqual(result["status"], "failed")

    def test_multiline_semantic_event_uses_the_last_required_data_line(self) -> None:
        result = bench.observe_sse_lines(
            [
                (
                    b'data: {"type":"response.output_text.delta",\n',
                    100,
                ),
                (b'data: "delta":"ACK"}\n', 900),
                (b"\n", 901),
                (
                    b'data: {"type":"response.completed","response":{"status":"completed"}}\n',
                    1_000,
                ),
                (b"\n", 1_001),
            ],
            t0_ns=0,
        )

        self.assertEqual(result["timing_ns"]["time_to_first_model_output"], 900)

    def test_answer_and_terminal_diagnostics_do_not_gate_the_score(self) -> None:
        run = bench._combined_run(
            "nanocodex",
            {"valid": True, "reported_tool_calls": 0},
            {
                "error": None,
                "status": "completed",
                "upstream_http_status": 200,
                "tool_call_detected": False,
                "cached_input_tokens": 1,
                "timing_ns": {
                    "time_to_first_model_output": 100,
                    "time_to_first_answer_text": None,
                    "time_to_terminal": None,
                },
            },
            0,
        )

        self.assertTrue(run["timing_present"])
        self.assertFalse(run["measured_failure"])

    def test_rejects_conflicting_cache_fields_and_nonfunction_tool_events(self) -> None:
        conflicting = (
            b'data: {"type":"response.completed","response":{"status":"completed",'
            b'"usage":{"cached_input_tokens":1,"input_tokens_details":'
            b'{"cached_tokens":2}}}}\n'
        )
        cache_result = bench.observe_sse_lines(
            [(conflicting, 10), (b"\n", 11)], t0_ns=0
        )
        self.assertEqual(cache_result["error"], "invalid_cached_input_tokens")
        self.assertIsNone(cache_result["cached_input_tokens"])

        tool_result = bench.observe_sse_lines(
            [
                (
                    b'data: {"type":"response.web_search_call.in_progress"}\n',
                    10,
                ),
                (b"\n", 11),
            ],
            t0_ns=0,
        )
        self.assertTrue(tool_result["tool_call_detected"])

        unknown_call = bench.observe_sse_lines(
            [
                (
                    b'data: {"type":"response.output_item.added","item":{"type":"future_provider_call","id":"danger"}}\n',
                    20,
                ),
                (b"\n", 21),
            ],
            t0_ns=0,
        )
        self.assertTrue(unknown_call["tool_call_detected"])


class LocalObserverIntegrationTests(ContractFixtureTestCase):
    def test_models_discovery_accepts_only_the_pinned_fx_query(self) -> None:
        auth = bench._fake_auth_material()

        headers = {
            "Authorization": f"Bearer {auth.access_token}",
            "ChatGPT-Account-ID": auth.account_id,
            "User-Agent": bench.FX_USER_AGENT,
            "Originator": "fx",
            "Accept": "application/json",
            "Connection": "keep-alive",
        }

        def get(observer: bench.StreamingObserver, target: str) -> tuple[int, bytes]:
            connection = http.client.HTTPConnection(
                "127.0.0.1", observer.server.server_port, timeout=5
            )
            _send_request(connection, "GET", target, headers=headers)
            response = connection.getresponse()
            result = response.status, response.read()
            connection.close()
            return result

        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth,
                provider.url,
                provider_request_budget=1,
                require_fx_models_discovery=True,
            ) as observer:
                observer.state.arm(bench.ExpectedRequest("fx", 1, "test"))
                exact_status, exact_body = get(
                    observer,
                    "/models?client_version=" + bench.FX_MODELS_CLIENT_VERSION,
                )
                rejected = [
                    get(observer, "/models"),
                    get(observer, "/models?client_version=wrong"),
                    get(
                        observer,
                        "/models?client_version="
                        + bench.FX_MODELS_CLIENT_VERSION
                        + "&extra=1",
                    ),
                    get(
                        observer,
                        "/models?client_version="
                        + bench.FX_MODELS_CLIENT_VERSION
                        + "&client_version="
                        + bench.FX_MODELS_CLIENT_VERSION,
                    ),
                    get(observer, "/models?client%5Fversion=0.148.0"),
                    get(observer, "/models?client_version=%30.148.0"),
                ]
                stats = observer.state.stats()

        self.assertEqual(exact_status, 200)
        self.assertEqual(exact_body, bench.MODELS_BYTES)
        self.assertEqual([status for status, _body in rejected], [404] * 6)
        self.assertEqual(stats["models_requests"], 1)
        self.assertEqual(stats["unexpected_requests"], 6)
        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(provider.server.requests, 0)

    def test_models_discovery_rejects_account_whitespace_and_poisons_run(self) -> None:
        auth = bench._fake_auth_material()
        model_headers = {
            "Authorization": f"Bearer {auth.access_token}",
            "ChatGPT-Account-ID": f"{auth.account_id} ",
            "User-Agent": bench.FX_USER_AGENT,
            "Originator": "fx",
            "Accept": "application/json",
            "Connection": "keep-alive",
        }
        exact_model_headers = {
            **model_headers,
            "ChatGPT-Account-ID": auth.account_id,
        }
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth,
                provider.url,
                provider_request_budget=1,
                require_fx_models_discovery=True,
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                connection = http.client.HTTPConnection(
                    "127.0.0.1", observer.server.server_port, timeout=5
                )
                _send_request(
                    connection,
                    "GET",
                    "/models?client_version=" + bench.FX_MODELS_CLIENT_VERSION,
                    headers=model_headers,
                )
                rejected = connection.getresponse()
                rejected.read()
                connection.close()

                connection = http.client.HTTPConnection(
                    "127.0.0.1", observer.server.server_port, timeout=5
                )
                _send_request(
                    connection,
                    "GET",
                    "/models?client_version=" + bench.FX_MODELS_CLIENT_VERSION,
                    headers=exact_model_headers,
                )
                accepted = connection.getresponse()
                accepted.read()
                connection.close()

                status, _streamed = _post_observer(
                    observer, _compact(_fx_request()), auth
                )
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()
                auth_verified = observer.auth_matcher.verified_for({"fx"})

        self.assertEqual(rejected.status, 404)
        self.assertEqual(accepted.status, 200)
        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(stats["models_requests"], 1)
        self.assertEqual(stats["unexpected_requests"], 1)
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(provider.server.requests, 1)
        self.assertFalse(auth_verified)

    def test_transport_headers_match_each_captured_client_exactly(self) -> None:
        auth = bench._fake_auth_material()
        models_headers = {
            "Authorization": f"Bearer {auth.access_token}",
            "ChatGPT-Account-ID": auth.account_id,
            "User-Agent": bench.FX_USER_AGENT,
            "Originator": "fx",
            "Accept": "application/json",
            "Connection": "keep-alive",
        }

        def request(
            observer: bench.StreamingObserver,
            method: str,
            target: str,
            header_pairs: list[tuple[str, str]],
            *,
            body: bytes | None = None,
            skip_host: bool = False,
        ) -> int:
            connection = http.client.HTTPConnection(
                "127.0.0.1", observer.server.server_port, timeout=5
            )
            connection.putrequest(
                method,
                target,
                skip_host=skip_host,
                skip_accept_encoding=True,
            )
            for name, value in header_pairs:
                connection.putheader(name, value)
            if body is not None:
                connection.putheader("Content-Length", str(len(body)))
            connection.endheaders(body)
            response = connection.getresponse()
            status = response.status
            response.read()
            connection.close()
            return status

        model_cases = (
            (list(models_headers.items()), True, "request_header_set_invalid"),
            (
                [("Host", "wrong.invalid"), *models_headers.items()],
                True,
                "observer_host_header_invalid",
            ),
            (
                [
                    ("Host", "127.0.0.1:1"),
                    ("Host", "127.0.0.1:1"),
                    *models_headers.items(),
                ],
                True,
                "host_header_count_invalid",
            ),
            (
                [
                    *(
                        (name, value)
                        for name, value in models_headers.items()
                        if name != "Connection"
                    ),
                ],
                False,
                "request_header_set_invalid",
            ),
            (
                [
                    *(
                        (name, "close" if name == "Connection" else value)
                        for name, value in models_headers.items()
                    ),
                ],
                False,
                "fx_models_connection_header_invalid",
            ),
            (
                [*models_headers.items(), ("Accept-Encoding", "identity")],
                False,
                "request_header_set_invalid",
            ),
        )
        with bench._OfflineProvider() as provider:
            for ordinal, (headers, skip_host, expected_error) in enumerate(
                model_cases, 1
            ):
                with self.subTest(kind="models", case=ordinal):
                    with bench.StreamingObserver._for_test(
                        auth,
                        provider.url,
                        provider_request_budget=1,
                        require_fx_models_discovery=True,
                    ) as observer:
                        observer.state.arm(
                            bench.ExpectedRequest("fx", ordinal, "test")
                        )
                        status = request(
                            observer,
                            "GET",
                            "/models?client_version="
                            + bench.FX_MODELS_CLIENT_VERSION,
                            headers,
                            skip_host=skip_host,
                        )
                        stats = observer.state.stats()
                    self.assertEqual(status, 404)
                    self.assertEqual(stats["models_requests"], 0)
                    self.assertEqual(stats["unexpected_requests"], 1)

            body_by_implementation = {
                "fx": _compact(_fx_request()),
                "nanocodex": _compact(_nanocodex_request()),
            }
            generation_cases = (
                (
                    "fx",
                    [
                        *(
                            (name, value)
                            for name, value in _request_headers(auth, "fx").items()
                            if name != "Connection"
                        )
                    ],
                    False,
                    "request_header_set_invalid",
                ),
                (
                    "fx",
                    [
                        *(
                            (name, "keep-alive" if name == "Connection" else value)
                            for name, value in _request_headers(auth, "fx").items()
                        )
                    ],
                    False,
                    "fx_connection_header_invalid",
                ),
                (
                    "nanocodex",
                    [
                        *_request_headers(auth, "nanocodex").items(),
                        ("Connection", "close"),
                    ],
                    False,
                    "request_header_set_invalid",
                ),
                (
                    "nanocodex",
                    [
                        *_request_headers(auth, "nanocodex").items(),
                        ("Accept-Encoding", "identity"),
                    ],
                    False,
                    "request_header_set_invalid",
                ),
                (
                    "nanocodex",
                    list(_request_headers(auth, "nanocodex").items()),
                    True,
                    "request_header_set_invalid",
                ),
                (
                    "nanocodex",
                    [
                        ("Host", "wrong.invalid"),
                        *_request_headers(auth, "nanocodex").items(),
                    ],
                    True,
                    "observer_host_header_invalid",
                ),
                (
                    "nanocodex",
                    [
                        ("Host", "127.0.0.1:1"),
                        ("Host", "127.0.0.1:1"),
                        *_request_headers(auth, "nanocodex").items(),
                    ],
                    True,
                    "host_header_count_invalid",
                ),
                (
                    "fx",
                    [
                        *(
                            (
                                name,
                                "Application/JSON"
                                if name == "Content-Type"
                                else value,
                            )
                            for name, value in _request_headers(auth, "fx").items()
                        )
                    ],
                    False,
                    "content_type_must_be_application_json",
                ),
                (
                    "fx",
                    [
                        *(
                            (
                                name,
                                "TEXT/EVENT-STREAM" if name == "Accept" else value,
                            )
                            for name, value in _request_headers(auth, "fx").items()
                        )
                    ],
                    False,
                    "accept_must_be_text_event_stream",
                ),
                (
                    "nanocodex",
                    [
                        *(
                            (
                                name,
                                "Application/JSON"
                                if name == "Content-Type"
                                else value,
                            )
                            for name, value in _request_headers(
                                auth, "nanocodex"
                            ).items()
                        )
                    ],
                    False,
                    "content_type_must_be_application_json",
                ),
                (
                    "nanocodex",
                    [
                        *(
                            (
                                name,
                                "TEXT/EVENT-STREAM" if name == "Accept" else value,
                            )
                            for name, value in _request_headers(
                                auth, "nanocodex"
                            ).items()
                        )
                    ],
                    False,
                    "accept_must_be_text_event_stream",
                ),
            )
            for ordinal, (
                implementation,
                headers,
                skip_host,
                expected_error,
            ) in enumerate(generation_cases, 1):
                with self.subTest(kind="generation", case=ordinal):
                    with bench.StreamingObserver._for_test(
                        auth, provider.url, provider_request_budget=1
                    ) as observer:
                        ticket = observer.state.arm(
                            bench.ExpectedRequest(implementation, ordinal, "test")
                        )
                        status = request(
                            observer,
                            "POST",
                            "/responses",
                            headers,
                            body=body_by_implementation[implementation],
                            skip_host=skip_host,
                        )
                        observed = observer.state.wait(ticket, 5)
                        stats = observer.state.stats()
                    self.assertEqual(status, 400)
                    self.assertEqual(observed["error"], expected_error)
                    self.assertEqual(stats["provider_requests_forwarded"], 0)

        self.assertEqual(provider.server.requests, 0)

    def test_raw_parser_defects_are_counted_before_claim_or_forwarding(self) -> None:
        auth = bench._fake_auth_material()
        generation_body = _compact(_fx_request())

        def raw_request(
            observer: bench.StreamingObserver,
            request_line: str,
            headers: list[tuple[str, str]],
            *,
            malformed_header: bytes = b"",
            body: bytes = b"",
        ) -> bytes:
            connection = socket.create_connection(
                ("127.0.0.1", observer.server.server_port), timeout=5
            )
            encoded_headers = b"".join(
                f"{name}: {value}\r\n".encode("ascii") for name, value in headers
            )
            connection.sendall(
                request_line.encode("ascii")
                + b"\r\n"
                + encoded_headers
                + malformed_header
                + b"\r\n"
                + body
            )
            chunks: list[bytes] = []
            while True:
                chunk = connection.recv(4096)
                if not chunk:
                    break
                chunks.append(chunk)
            connection.close()
            return b"".join(chunks)

        malformed_headers = (
            b"X-Malformed\r\n",
            b"X-Malformed : value\r\n",
            b": value\r\n",
        )
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth,
                provider.url,
                provider_request_budget=1,
                require_fx_models_discovery=True,
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                model_headers = [
                    ("Host", f"127.0.0.1:{observer.server.server_port}"),
                    (
                        "Authorization",
                        f"Bearer {auth.access_token}",
                    ),
                    ("ChatGPT-Account-ID", auth.account_id),
                    ("User-Agent", bench.FX_USER_AGENT),
                    ("Originator", "fx"),
                    ("Accept", "application/json"),
                    ("Connection", "keep-alive"),
                ]
                generation_headers = [
                    ("Host", f"127.0.0.1:{observer.server.server_port}"),
                    *_request_headers(auth, "fx").items(),
                    ("Content-Length", str(len(generation_body))),
                ]
                for malformed in malformed_headers:
                    raw_request(
                        observer,
                        "GET /models?client_version=0.148.0 HTTP/1.1",
                        model_headers,
                        malformed_header=malformed,
                    )
                    raw_request(
                        observer,
                        "POST /responses HTTP/1.1",
                        generation_headers,
                        malformed_header=malformed,
                        body=generation_body,
                    )

                stats = observer.state.stats()
                self.assertTrue(observer.state.cancel_unclaimed(ticket))

        self.assertEqual(stats["unexpected_requests"], 6)
        self.assertEqual(stats["models_requests"], 0)
        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(provider.server.requests, 0)

    def test_pre_dispatch_parser_failures_poison_a_following_valid_run(self) -> None:
        auth = bench._fake_auth_material()
        body = _compact(_fx_request())

        def fire(observer: bench.StreamingObserver, request: bytes) -> None:
            connection = socket.create_connection(
                ("127.0.0.1", observer.server.server_port), timeout=5
            )
            connection.sendall(request)
            with contextlib.suppress(OSError):
                while connection.recv(4096):
                    pass
            connection.close()

        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                overflow = (
                    b"GET /responses HTTP/1.1\r\nHost: 127.0.0.1\r\n"
                    + b"".join(
                        f"X-Overflow-{index}: value\r\n".encode("ascii")
                        for index in range(128)
                    )
                    + b"\r\n"
                )
                for request in (
                    overflow,
                    b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
                    b"BAD METHOD /responses HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                ):
                    fire(observer, request)
                self.assertEqual(
                    observer.state.stats()["provider_requests_forwarded"], 0
                )

                status, _streamed = _post_observer(observer, body, auth, "fx")
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(stats["unexpected_requests"], 3)
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(provider.server.requests, 1)

    def test_oversized_request_line_is_counted_once_and_poisons_eligibility(
        self,
    ) -> None:
        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                connection = socket.create_connection(
                    ("127.0.0.1", observer.server.server_port), timeout=5
                )
                connection.sendall(
                    b"GET /" + b"x" * 65_536 + b" HTTP/1.1\r\n\r\n"
                )
                response = bytearray()
                with contextlib.suppress(ConnectionResetError):
                    while chunk := connection.recv(4096):
                        response.extend(chunk)
                connection.close()

                self.assertTrue(bytes(response).startswith(b"HTTP/1.1 414 "))
                self.assertEqual(
                    observer.state.stats()["unexpected_requests"], 1
                )
                self.assertEqual(
                    observer.state.stats()["provider_requests_forwarded"], 0
                )

                status, _streamed = _post_observer(observer, body, auth, "fx")
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        combined = bench._combined_run(
            "fx",
            {
                "valid": True,
                "reported_tool_calls": 0,
                "client_configuration_echo_valid": True,
            },
            observed,
            stats["unexpected_requests"],
        )
        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(stats["unexpected_requests"], 1)
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(provider.server.requests, 1)
        self.assertTrue(combined["measured_failure"])
        self.assertFalse(combined["no_tool_calls"])

    def test_only_exact_origin_form_post_targets_can_claim_or_refresh(self) -> None:
        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        invalid_targets = (
            "http://attacker.invalid/responses",
            "//responses",
            "http://attacker.invalid/forbid-refresh",
            "//forbid-refresh",
        )
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                statuses: list[int] = []
                for target in invalid_targets:
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    _send_request(
                        connection,
                        "POST",
                        target,
                        headers=_request_headers(auth, "fx"),
                        body=body,
                    )
                    response = connection.getresponse()
                    statuses.append(response.status)
                    response.read()
                    connection.close()

                before_valid = observer.state.stats()
                status, _streamed = _post_observer(observer, body, auth, "fx")
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        combined = bench._combined_run(
            "fx",
            {
                "valid": True,
                "reported_tool_calls": 0,
                "client_configuration_echo_valid": True,
            },
            observed,
            stats["unexpected_requests"],
        )
        self.assertEqual(statuses, [404] * len(invalid_targets))
        self.assertEqual(before_valid["unexpected_requests"], len(invalid_targets))
        self.assertEqual(before_valid["refresh_attempts_blocked"], 0)
        self.assertEqual(before_valid["provider_requests_forwarded"], 0)
        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(provider.server.requests, 1)
        self.assertTrue(combined["measured_failure"])

    def test_every_unsupported_method_is_counted_before_valid_generation(self) -> None:
        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                for method in ("HEAD", "OPTIONS", "TRACE", "BREW"):
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    connection.putrequest(
                        method, "/responses", skip_accept_encoding=True
                    )
                    connection.endheaders()
                    response = connection.getresponse()
                    response.read()
                    connection.close()
                    self.assertEqual(response.status, 405)

                status, _streamed = _post_observer(observer, body, auth, "fx")
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(stats["unexpected_requests"], 4)
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(provider.server.requests, 1)

    def test_reset_unsupported_bodies_are_counted_before_drain(self) -> None:
        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                for expected_count, method in enumerate(("HEAD", "BREW"), start=1):
                    connection = socket.create_connection(
                        ("127.0.0.1", observer.server.server_port), timeout=5
                    )
                    connection.sendall(
                        (
                            f"{method} /responses HTTP/1.1\r\n"
                            f"Host: 127.0.0.1:{observer.server.server_port}\r\n"
                            "Content-Length: 1048576\r\n"
                            "Connection: close\r\n\r\n"
                            "x"
                        ).encode("ascii")
                    )
                    connection.setsockopt(
                        socket.SOL_SOCKET,
                        socket.SO_LINGER,
                        struct.pack("ii", 1, 0),
                    )
                    connection.close()
                    deadline = time.monotonic() + 5
                    while (
                        observer.state.stats()["unexpected_requests"]
                        < expected_count
                        and time.monotonic() < deadline
                    ):
                        time.sleep(0.01)
                    self.assertEqual(
                        observer.state.stats()["unexpected_requests"],
                        expected_count,
                    )

                status, _streamed = _post_observer(observer, body, auth, "fx")
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(stats["unexpected_requests"], 2)
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(provider.server.requests, 1)

    def test_shutdown_interrupts_a_stalled_upstream_and_drains_all_work(self) -> None:
        auth = bench._fake_auth_material()
        client_result: dict[str, object] = {}
        response_owns_socket = threading.Event()

        class HandoffConnection(bench._ResolvedHTTPConnection):
            published_socket: socket.socket | None = None

            def connect(self) -> None:
                super().connect()
                self.published_socket = self.sock

            def getresponse(self) -> http.client.HTTPResponse:
                response = super().getresponse()
                if self.sock is None:
                    response_owns_socket.set()
                return response

        def post(observer: bench.StreamingObserver) -> None:
            try:
                client_result["response"] = _post_observer(
                    observer, _compact(_fx_request()), auth
                )
            except BaseException as error:
                client_result["error"] = type(error).__name__

        with bench._OfflineProvider(stall_after_headers=True) as provider:
            observer = bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            )
            target = observer.server.upstream
            connection = HandoffConnection(
                target.host,
                target.port,
                target.endpoints,
                timeout=bench.UPSTREAM_CONNECT_TIMEOUT_SECONDS,
            )
            with mock.patch.object(
                bench.UpstreamTarget, "connection", return_value=connection
            ):
                with observer:
                    observer.state.arm(bench.ExpectedRequest("fx", 1, "test"))
                    client_thread = threading.Thread(
                        target=post, args=(observer,), daemon=True
                    )
                    client_thread.start()
                    self.assertTrue(provider.server.request_started.wait(5))
                    self.assertTrue(response_owns_socket.wait(5))
                    self.assertIsNone(connection.sock)
                    self.assertEqual(
                        observer.state.stats()["provider_requests_forwarded"], 1
                    )
                    shutdown_started = time.monotonic()

            shutdown_elapsed = time.monotonic() - shutdown_started
            self.assertTrue(provider.server.peer_disconnected.wait(5))
            self.assertEqual(
                provider.server.wait_for_quiescence(time.monotonic() + 5), 0
            )
            client_thread.join(timeout=5)
            self.assertFalse(client_thread.is_alive())
            self.assertFalse(observer._thread.is_alive())
            self.assertEqual(observer.server.active_work_count(), 0)
            self.assertEqual(observer.server.socket.fileno(), -1)
            self.assertIsNotNone(connection.published_socket)
            self.assertEqual(connection.published_socket.fileno(), -1)
            self.assertLess(
                shutdown_elapsed, bench.OBSERVER_SHUTDOWN_TIMEOUT_SECONDS + 1
            )

        self.assertFalse(provider._thread.is_alive())
        self.assertEqual(provider.server.active_work_count(), 0)

    def test_offline_template_mismatch_fails_before_budget_or_upstream(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                upstream_requests += 1
                self.send_response(500)
                self.end_headers()

        auth = bench._fake_auth_material()
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth,
                url,
                provider_request_budget=1,
                expected_fingerprints={"fx": "0" * 64, "nanocodex": "1" * 64},
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                status, _ = _post_observer(
                    observer, _compact(_fx_request()), auth
                )
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        self.assertEqual(status, 400)
        self.assertEqual(
            observed["error"], "request_fingerprint_preflight_mismatch"
        )
        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(upstream_requests, 0)

    def test_uuid7_freshness_spans_preflight_fedramp_and_live_observers(self) -> None:
        def fingerprint(
            request: dict[str, object], auth: bench.AuthMaterial
        ) -> str:
            body = _compact(request)
            validated = bench.validate_request_body(
                body,
                "nanocodex",
                home=TEST_HOME,
                workspace=TEST_WORKSPACE,
            )
            session_id = request["client_metadata"]["session_id"]
            metadata = [
                ("User-Agent", bench.NANOCODEX_USER_AGENT),
                ("X-OpenAI-Internal-Codex-Responses-Lite", "true"),
                ("Session-ID", session_id),
                ("Thread-ID", session_id),
                ("X-Client-Request-ID", session_id),
            ]
            if auth.fedramp:
                metadata.append(("X-OpenAI-Fedramp", "true"))
            headers = bench.CanonicalRequestHeaders(
                f"Bearer {auth.access_token}",
                auth.account_id,
                len(body),
                session_id,
                tuple(metadata),
            )
            return bench._combined_request_fingerprint(validated, headers)

        def post(
            observer: bench.StreamingObserver,
            request: dict[str, object],
            auth: bench.AuthMaterial,
            phase: str,
        ) -> tuple[int, dict[str, object]]:
            session_id = request["client_metadata"]["session_id"]
            ticket = observer.state.arm(
                bench.ExpectedRequest(
                    "nanocodex",
                    1,
                    phase,
                    home=TEST_HOME,
                    workspace=TEST_WORKSPACE,
                )
            )
            connection = http.client.HTTPConnection(
                "127.0.0.1", observer.server.server_port, timeout=5
            )
            _send_request(
                connection,
                "POST",
                "/responses",
                body=_compact(request),
                headers=_request_headers(
                    auth, "nanocodex", session_id=session_id
                ),
            )
            response = connection.getresponse()
            status = response.status
            response.read()
            connection.close()
            observed = observer.state.wait(ticket, 5)
            self.assertIsNotNone(observed)
            return status, observed

        boundaries = (
            ("ordinary_to_fedramp", False, True, "offline_fedramp_preflight"),
            ("fedramp_preflight_to_live", True, False, "warmup"),
        )
        for boundary_index, (
            boundary,
            first_fedramp,
            second_fedramp,
            second_phase,
        ) in enumerate(boundaries, 1):
            for direction_index, direction in enumerate(
                ("session_to_msg", "msg_to_session"), 1
            ):
                with self.subTest(boundary=boundary, direction=direction):
                    value = boundary_index * 10_000 + direction_index * 1_000
                    first_auth = bench._fake_auth_material(
                        fedramp=first_fedramp
                    )
                    second_auth = bench._fake_auth_material(
                        fedramp=second_fedramp
                    )
                    ledger = bench.FreshUuid7Ledger()
                    first_request = _nanocodex_request(
                        session_id=_session_id(value),
                        id_offset=value + 10,
                    )
                    second_request = _nanocodex_request(
                        session_id=_session_id(value + 100),
                        id_offset=value + 200,
                    )
                    if direction == "session_to_msg":
                        second_request["input"][2]["id"] = "msg_" + _session_id(
                            value
                        )
                    else:
                        reused_message_id = first_request["input"][2]["id"]
                        reused = reused_message_id.removeprefix("msg_")
                        second_request["client_metadata"]["session_id"] = reused
                        second_request["client_metadata"]["thread_id"] = reused

                    expected_fingerprint = fingerprint(first_request, first_auth)
                    self.assertEqual(
                        fingerprint(second_request, second_auth),
                        expected_fingerprint,
                    )

                    with bench._OfflineProvider() as first_provider:
                        with bench.StreamingObserver._for_test(
                            first_auth,
                            first_provider.url,
                            provider_request_budget=1,
                            fresh_identifier_ledger=ledger,
                        ) as first_observer:
                            first_status, first_observed = post(
                                first_observer,
                                first_request,
                                first_auth,
                                "offline_preflight",
                            )
                            first_stats = first_observer.state.stats()
                            first_fingerprints = (
                                first_observer.state.request_fingerprints()
                            )
                    self.assertEqual(first_status, 200)
                    self.assertEqual(first_observed["status"], "completed")
                    self.assertEqual(first_stats["provider_requests_forwarded"], 1)
                    self.assertEqual(first_provider.server.requests, 1)
                    self.assertEqual(
                        first_fingerprints, {"nanocodex": expected_fingerprint}
                    )

                    with bench._OfflineProvider() as second_provider:
                        with bench.StreamingObserver._for_test(
                            second_auth,
                            second_provider.url,
                            provider_request_budget=1,
                            expected_fingerprints={
                                "fx": "0" * 64,
                                "nanocodex": expected_fingerprint,
                            },
                            fresh_identifier_ledger=ledger,
                        ) as second_observer:
                            second_status, second_observed = post(
                                second_observer,
                                second_request,
                                second_auth,
                                second_phase,
                            )
                            second_stats = second_observer.state.stats()
                            second_fingerprints = (
                                second_observer.state.request_fingerprints()
                            )

                    self.assertEqual(second_status, 400)
                    self.assertEqual(
                        second_observed["error"],
                        "fresh_request_identifier_reused",
                    )
                    self.assertEqual(second_fingerprints, {})
                    self.assertEqual(
                        second_stats["provider_requests_forwarded"], 0
                    )
                    self.assertEqual(second_provider.server.requests, 0)
                    self.assertEqual(
                        ledger.evidence(),
                        {
                            "accepted_identity_uses": 4,
                            "distinct_canonical_uuid7_identities": 4,
                            "reuse_detected": True,
                            "noncanonical_identity_detected": False,
                            "distinct_and_one_use_verified": False,
                        },
                    )

    def test_provider_budget_rejects_second_http_request_before_upstream(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                length = int(self.headers["Content-Length"])
                self.rfile.read(length)
                upstream_requests += 1
                body = (
                    b'data: {"type":"response.output_text.delta","delta":"ACK"}\n\n'
                    b'data: {"type":"response.completed","response":{"status":"completed","usage":{"cached_input_tokens":1},"output":[]}}\n\n'
                )
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=1
            ) as observer:
                first_ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                first_status, _ = _post_observer(observer, body, auth)
                first = observer.state.wait(first_ticket, 5)
                second_ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 2, "test")
                )
                second_status, _ = _post_observer(observer, body, auth)
                second = observer.state.wait(second_ticket, 5)
                stats = observer.state.stats()

        self.assertEqual(first_status, 200)
        self.assertEqual(first["status"], "completed")
        self.assertEqual(second_status, 400)
        self.assertEqual(second["error"], "provider_request_budget_exhausted")
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(upstream_requests, 1)

    def test_invalid_request_bodies_fail_before_budget_or_upstream(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                upstream_requests += 1
                self.send_response(500)
                self.end_headers()

        prior = _nanocodex_request()
        prior["previous_response_id"] = "resp_old"
        stripped_fx = _fx_request()
        stripped_fx["tools"] = [_function_tool("read_file")]
        wrong_workspace = _nanocodex_request(workspace="/private/wrong/workspace")
        cases = (
            (
                "nanocodex",
                prior,
                bench.ExpectedRequest(
                    "nanocodex",
                    1,
                    "test",
                    home=TEST_HOME,
                    workspace=TEST_WORKSPACE,
                ),
                "unexpected_previous_response_id",
            ),
            (
                "fx",
                stripped_fx,
                bench.ExpectedRequest(
                    "fx", 2, "test", home=TEST_HOME, workspace=TEST_WORKSPACE
                ),
                "fx_default_tool_surface_mismatch",
            ),
            (
                "nanocodex",
                wrong_workspace,
                bench.ExpectedRequest(
                    "nanocodex",
                    3,
                    "test",
                    home=TEST_HOME,
                    workspace=TEST_WORKSPACE,
                ),
                "nanocodex_workspace_context_mismatch",
            ),
        )
        auth = bench._fake_auth_material()
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=len(cases)
            ) as observer:
                for implementation, request, expected, error in cases:
                    with self.subTest(error=error):
                        ticket = observer.state.arm(expected)
                        status, _ = _post_observer(
                            observer,
                            _compact(request),
                            auth,
                            implementation,
                        )
                        observed = observer.state.wait(ticket, 5)
                        self.assertEqual(status, 400)
                        self.assertEqual(observed["error"], error)
                stats = observer.state.stats()

        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(upstream_requests, 0)

    def test_forwards_only_canonical_headers_and_records_stable_fingerprints(self) -> None:
        received_bodies: list[bytes] = []
        received_headers: list[dict[str, list[str]]] = []

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers["Content-Length"])
                received_bodies.append(self.rfile.read(length))
                received_headers.append(
                    {
                        name.lower(): self.headers.get_all(name, [])
                        for name in self.headers.keys()
                    }
                )
                body = (
                    b'data: {"type":"response.output_text.delta","delta":"'
                    + bench.ACK_TOKEN.encode()
                    + b'"}\n\n'
                    b'data: {"type":"response.completed","response":{"status":"completed","usage":{"cached_input_tokens":3}}}\n\n'
                )
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        auth = bench._fake_auth_material()
        bodies = {
            "fx": _compact(_fx_request()),
            "nanocodex": _compact(_nanocodex_request()),
        }
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=2
            ) as observer:
                for pair, implementation in enumerate(("fx", "nanocodex"), 1):
                    ticket = observer.state.arm(
                        bench.ExpectedRequest(implementation, pair, "test")
                    )
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    headers = _request_headers(auth, implementation)
                    _send_request(
                        connection,
                        "POST",
                        "/responses",
                        body=bodies[implementation],
                        headers=headers,
                    )
                    response = connection.getresponse()
                    streamed = response.read()
                    connection.close()
                    self.assertEqual(response.status, 200)
                    self.assertIn(bench.ACK_TOKEN.encode(), streamed)
                    observation = observer.state.wait(ticket, 5)
                    self.assertEqual(observation["status"], "completed")

                self.assertTrue(observer.auth_matcher.verified())
                self.assertTrue(observer.state.fingerprints_stable())

        self.assertEqual(received_bodies, [bodies["fx"], bodies["nanocodex"]])
        common_names = {
            "host",
            "authorization",
            "chatgpt-account-id",
            "content-type",
            "accept",
            "content-length",
            "connection",
        }
        product_names = {
            "fx": {
                "user-agent",
                "originator",
                "openai-beta",
            },
            "nanocodex": {
                "user-agent",
                "x-openai-internal-codex-responses-lite",
                "session-id",
                "thread-id",
                "x-client-request-id",
            },
        }
        for implementation, headers in zip(
            ("fx", "nanocodex"), received_headers
        ):
            expected_names = common_names | product_names[implementation]
            self.assertEqual(set(headers), expected_names)
            self.assertTrue(all(len(values) == 1 for values in headers.values()))
            self.assertEqual(headers["content-type"], ["application/json"])
            self.assertEqual(headers["accept"], ["text/event-stream"])
        self.assertEqual(received_headers[0]["originator"], ["fx"])
        self.assertEqual(
            received_headers[0]["openai-beta"], ["responses=experimental"]
        )
        self.assertEqual(
            received_headers[1]["x-openai-internal-codex-responses-lite"],
            ["true"],
        )

    def test_nanocodex_fedramp_header_tracks_the_credential(self) -> None:
        received_fedramp: list[str | None] = []

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers["Content-Length"])
                self.rfile.read(length)
                received_fedramp.append(self.headers.get("X-OpenAI-Fedramp"))
                body = (
                    b'data: {"type":"response.output_text.delta","delta":"ACK"}\n\n'
                    b'data: {"type":"response.completed","response":{"status":"completed","usage":{"cached_input_tokens":1}}}\n\n'
                )
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(body)

        auth = bench._fake_auth_material(fedramp=True)
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("nanocodex", 1, "test")
                )
                status, _ = _post_observer(
                    observer,
                    _compact(_nanocodex_request()),
                    auth,
                    "nanocodex",
                )
                observed = observer.state.wait(ticket, 5)

        self.assertEqual(status, 200)
        self.assertEqual(observed["status"], "completed")
        self.assertEqual(received_fedramp, ["true"])

    def test_rejects_each_duplicate_sensitive_header_before_upstream(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                upstream_requests += 1
                self.send_response(500)
                self.end_headers()

        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        base_headers = [
            *_request_headers(auth, "fx").items(),
            ("Content-Length", str(len(body))),
        ]
        expected_errors = {
            "Authorization": "authorization_header_count_invalid",
            "ChatGPT-Account-ID": "chatgpt_account_id_header_count_invalid",
            "Content-Type": "content_type_header_count_invalid",
            "Content-Length": "content_length_header_count_invalid",
        }
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=4
            ) as observer:
                for pair, duplicate in enumerate(expected_errors, 1):
                    ticket = observer.state.arm(
                        bench.ExpectedRequest("fx", pair, "test")
                    )
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    connection.putrequest(
                        "POST", "/responses", skip_accept_encoding=True
                    )
                    for name, value in base_headers:
                        connection.putheader(name, value)
                        if name == duplicate:
                            connection.putheader(name, value)
                    connection.endheaders(body)
                    response = connection.getresponse()
                    response.read()
                    connection.close()
                    self.assertEqual(response.status, 400)
                    observed = observer.state.wait(ticket, 5)
                    self.assertEqual(observed["error"], expected_errors[duplicate])

                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 5, "test")
                )
                connection = http.client.HTTPConnection(
                    "127.0.0.1", observer.server.server_port, timeout=5
                )
                _send_request(
                    connection,
                    "POST",
                    "/responses",
                    body=body,
                    headers=_request_headers(auth)
                    | {"ChatGPT-Account-ID": f" {auth.account_id} "},
                )
                response = connection.getresponse()
                response.read()
                connection.close()
                self.assertEqual(response.status, 400)
                observed = observer.state.wait(ticket, 5)
                self.assertEqual(
                    observed["error"], "credential_header_whitespace_invalid"
                )

                self.assertEqual(
                    observer.state.stats()["provider_requests_forwarded"], 0
                )
        self.assertEqual(upstream_requests, 0)

    def test_upstream_401_becomes_502_and_never_triggers_refresh(self) -> None:
        body_marker = b'UPSTREAM-SECRET-BODY-MARKER:{"error":"unauthorized"}\n'
        header_marker = "upstream-secret-header-marker"

        class Unauthorized(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers["Content-Length"])
                self.rfile.read(length)
                body = body_marker
                self.send_response(401)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("WWW-Authenticate", header_marker)
                self.send_header("Set-Cookie", f"secret={header_marker}")
                self.send_header("Location", f"https://example.invalid/{header_marker}")
                self.end_headers()
                self.wfile.write(body)

        auth = bench._fake_auth_material()
        with _serve(Unauthorized) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                status, downstream, downstream_headers = _post_observer_with_headers(
                    observer, _compact(_fx_request()), auth
                )
                observed = observer.state.wait(ticket, 5)

                self.assertEqual(status, 502)
                self.assertEqual(downstream, b"upstream authentication failed\n")
                self.assertEqual(
                    {name.lower() for name, _value in downstream_headers},
                    {"content-type", "content-length", "connection"},
                )
                self.assertNotIn(body_marker, downstream)
                self.assertNotIn(
                    header_marker,
                    repr((downstream, downstream_headers)),
                )
                self.assertEqual(observed["upstream_http_status"], 401)
                self.assertEqual(observed["error"], "upstream_http_401")
                self.assertEqual(observer.state.stats()["refresh_attempts_blocked"], 0)
                self.assertEqual(
                    observer.state.stats()["provider_requests_forwarded"], 1
                )
                self.assertEqual(observer.state.stats()["unexpected_requests"], 0)

    def test_redirect_upgrade_and_connect_cannot_escape_observer(self) -> None:
        redirect_target_requests = 0

        class RedirectTarget(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal redirect_target_requests
                redirect_target_requests += 1
                self.send_response(500)
                self.end_headers()

        auth = bench._fake_auth_material()
        with _serve(RedirectTarget) as target:
            location = f"http://127.0.0.1:{target.server_port}/stolen"

            class RedirectingUpstream(http.server.BaseHTTPRequestHandler):
                def log_message(self, _format: str, *_args: object) -> None:
                    return

                def do_POST(self) -> None:  # noqa: N802
                    length = int(self.headers["Content-Length"])
                    self.rfile.read(length)
                    self.send_response(307)
                    self.send_header("Location", location)
                    self.send_header("Set-Cookie", "upstream-secret=marker")
                    self.end_headers()

            with _serve(RedirectingUpstream) as upstream:
                url = f"http://127.0.0.1:{upstream.server_port}/responses"
                with bench.StreamingObserver._for_test(
                    auth, url, provider_request_budget=1
                ) as observer:
                    ticket = observer.state.arm(
                        bench.ExpectedRequest("fx", 1, "test")
                    )
                    status, _body, headers = _post_observer_with_headers(
                        observer, _compact(_fx_request()), auth
                    )
                    observed = observer.state.wait(ticket, 5)

                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    connection.request(
                        "GET",
                        "/responses",
                        headers={
                            "Connection": "Upgrade",
                            "Upgrade": "websocket",
                            "Sec-WebSocket-Key": "offline-test",
                        },
                    )
                    upgrade = connection.getresponse()
                    upgrade.read()
                    connection.close()

                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    connection.request("CONNECT", "example.invalid:443")
                    connect = connection.getresponse()
                    connect.read()
                    connection.close()
                    stats = observer.state.stats()

        self.assertEqual(status, 307)
        self.assertEqual(observed["error"], "upstream_http_307")
        self.assertNotIn("location", {name.lower() for name, _value in headers})
        self.assertNotIn("set-cookie", {name.lower() for name, _value in headers})
        self.assertEqual(redirect_target_requests, 0)
        self.assertEqual(upgrade.status, 404)
        self.assertEqual(connect.status, 405)
        self.assertEqual(stats["provider_requests_forwarded"], 1)
        self.assertEqual(stats["unexpected_requests"], 2)

    def test_rejects_duplicate_product_metadata_and_fresh_turn_state(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                upstream_requests += 1
                self.send_response(500)
                self.end_headers()

        auth = bench._fake_auth_material(fedramp=True)
        cases: list[tuple[str, str]] = [
            ("fx", name)
            for name in (
                "User-Agent",
                "Originator",
                "OpenAI-Beta",
            )
        ] + [
            ("nanocodex", name)
            for name in (
                "User-Agent",
                "X-OpenAI-Internal-Codex-Responses-Lite",
                "Session-ID",
                "Thread-ID",
                "X-Client-Request-ID",
                "X-OpenAI-Fedramp",
            )
        ]
        bodies = {
            "fx": _compact(_fx_request()),
            "nanocodex": _compact(_nanocodex_request()),
        }
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=len(cases) + 1
            ) as observer:
                for pair, (implementation, duplicate) in enumerate(cases, 1):
                    body = bodies[implementation]
                    ticket = observer.state.arm(
                        bench.ExpectedRequest(implementation, pair, "test")
                    )
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    connection.putrequest(
                        "POST", "/responses", skip_accept_encoding=True
                    )
                    headers = _request_headers(auth, implementation)
                    headers["Content-Length"] = str(len(body))
                    for name, value in headers.items():
                        connection.putheader(name, value)
                        if name.lower() == duplicate.lower():
                            connection.putheader(name, value)
                    connection.endheaders(body)
                    response = connection.getresponse()
                    response.read()
                    connection.close()
                    self.assertEqual(response.status, 400)
                    observed = observer.state.wait(ticket, 5)
                    error = duplicate.lower().replace("-", "_")
                    self.assertEqual(
                        observed["error"], f"{error}_header_count_invalid"
                    )

                body = bodies["nanocodex"]
                ticket = observer.state.arm(
                    bench.ExpectedRequest("nanocodex", len(cases) + 1, "test")
                )
                connection = http.client.HTTPConnection(
                    "127.0.0.1", observer.server.server_port, timeout=5
                )
                _send_request(
                    connection,
                    "POST",
                    "/responses",
                    body=body,
                    headers=_request_headers(auth, "nanocodex")
                    | {"X-Codex-Turn-State": "prior-state"},
                )
                response = connection.getresponse()
                response.read()
                connection.close()
                self.assertEqual(response.status, 400)
                observed = observer.state.wait(ticket, 5)
                self.assertEqual(observed["error"], "request_header_set_invalid")

        self.assertEqual(upstream_requests, 0)

    def test_fx_no_save_rejects_every_session_identity_header(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                upstream_requests += 1
                self.send_response(500)
                self.end_headers()

        auth = bench._fake_auth_material()
        body = _compact(_fx_request())
        forbidden = ("Session-ID", "Thread-ID", "X-Client-Request-ID")
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=len(forbidden)
            ) as observer:
                for pair, name in enumerate(forbidden, 1):
                    ticket = observer.state.arm(
                        bench.ExpectedRequest("fx", pair, "test")
                    )
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", observer.server.server_port, timeout=5
                    )
                    _send_request(
                        connection,
                        "POST",
                        "/responses",
                        body=body,
                        headers=_request_headers(auth, "fx")
                        | {name: "forbidden-session"},
                    )
                    response = connection.getresponse()
                    response.read()
                    connection.close()
                    self.assertEqual(response.status, 400)
                    observed = observer.state.wait(ticket, 5)
                    self.assertEqual(
                        observed["error"], "request_header_set_invalid"
                    )
                stats = observer.state.stats()

        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(upstream_requests, 0)

    def test_rejects_unreviewed_candidate_header_before_upstream(self) -> None:
        upstream_requests = 0

        class FakeUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                nonlocal upstream_requests
                upstream_requests += 1
                self.send_response(500)
                self.end_headers()

        auth = bench._fake_auth_material()
        with _serve(FakeUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                connection = http.client.HTTPConnection(
                    "127.0.0.1", observer.server.server_port, timeout=5
                )
                _send_request(
                    connection,
                    "POST",
                    "/responses",
                    body=_compact(_fx_request()),
                    headers=_request_headers(auth, "fx")
                    | {"X-OpenAI-Evil": "unreviewed"},
                )
                response = connection.getresponse()
                response.read()
                connection.close()
                observed = observer.state.wait(ticket, 5)
                stats = observer.state.stats()

        self.assertEqual(response.status, 400)
        self.assertEqual(observed["error"], "request_header_set_invalid")
        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(upstream_requests, 0)

    def test_fx_generation_requires_one_valid_models_discovery(self) -> None:
        auth = bench._fake_auth_material()
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth,
                provider.url,
                provider_request_budget=1,
                require_fx_models_discovery=True,
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                status, _body = _post_observer(
                    observer, _compact(_fx_request()), auth, "fx"
                )
                observed = observer.state.wait(ticket, 5)

        self.assertEqual(status, 400)
        self.assertEqual(observed["error"], "fx_models_discovery_missing")
        self.assertEqual(provider.server.requests, 0)

    def test_tool_call_frame_is_blocked_before_child_delivery(self) -> None:
        class ToolCallingUpstream(http.server.BaseHTTPRequestHandler):
            def log_message(self, _format: str, *_args: object) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers["Content-Length"])
                self.rfile.read(length)
                body = (
                    b'data: {"type":"response.output_text.delta","delta":"partial"}\n\n'
                    b'data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"danger"}}\n\n'
                    b'data: {"type":"response.completed","response":{"status":"completed","usage":{"cached_input_tokens":2}}}\n\n'
                )
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                with contextlib.suppress(BrokenPipeError, ConnectionResetError):
                    self.wfile.write(body)
                    self.wfile.flush()

        auth = bench._fake_auth_material()
        with _serve(ToolCallingUpstream) as upstream:
            url = f"http://127.0.0.1:{upstream.server_port}/responses"
            with bench.StreamingObserver._for_test(
                auth, url, provider_request_budget=1
            ) as observer:
                ticket = observer.state.arm(
                    bench.ExpectedRequest("fx", 1, "test")
                )
                status, streamed = _post_observer(
                    observer, _compact(_fx_request()), auth
                )
                observed = observer.state.wait(ticket, 5)

        self.assertEqual(status, 200)
        self.assertIn(b"partial", streamed)
        self.assertNotIn(b"function_call", streamed)
        self.assertTrue(observed["tool_call_detected"])
        self.assertEqual(observed["error"], "tool_call_blocked_before_delivery")

    def test_tool_search_is_blocked_incrementally_and_in_terminal_output(self) -> None:
        variants = {
            "incremental": (
                b'data: {"type":"response.output_text.delta","delta":"partial"}\n\n'
                b'data: {"type":"response.output_item.added","item":{"type":"tool_search_call","call_id":"danger-search"}}\n\n'
                b'data: {"type":"response.completed","response":{"status":"completed","usage":{"cached_input_tokens":2},"output":[]}}\n\n'
            ),
            "terminal": (
                b'data: {"type":"response.completed","response":{"status":"completed","usage":{"cached_input_tokens":2},"output":[{"type":"tool_search_call","call_id":"danger-search"}]}}\n\n'
            ),
        }
        for name, response_body in variants.items():
            with self.subTest(variant=name):
                class ToolSearchUpstream(http.server.BaseHTTPRequestHandler):
                    def log_message(self, _format: str, *_args: object) -> None:
                        return

                    def do_POST(self) -> None:  # noqa: N802
                        length = int(self.headers["Content-Length"])
                        self.rfile.read(length)
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.end_headers()
                        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
                            self.wfile.write(response_body)
                            self.wfile.flush()

                auth = bench._fake_auth_material()
                with _serve(ToolSearchUpstream) as upstream:
                    url = f"http://127.0.0.1:{upstream.server_port}/responses"
                    with bench.StreamingObserver._for_test(
                        auth, url, provider_request_budget=1
                    ) as observer:
                        ticket = observer.state.arm(
                            bench.ExpectedRequest("nanocodex", 1, "test")
                        )
                        status, streamed = _post_observer(
                            observer,
                            _compact(_nanocodex_request()),
                            auth,
                            "nanocodex",
                        )
                        observed = observer.state.wait(ticket, 5)

                self.assertEqual(status, 200)
                self.assertNotIn(b"tool_search_call", streamed)
                self.assertNotIn(b"danger-search", streamed)
                self.assertTrue(observed["tool_call_detected"])
                self.assertEqual(
                    observed["error"], "tool_call_blocked_before_delivery"
                )

    def test_custom_upstream_requires_private_fake_auth_seam(self) -> None:
        real = bench.AuthMaterial(
            raw_codex_auth=b"{}",
            access_token="real-looking-access",
            refresh_token="real-looking-refresh",
            account_id="real-looking-account",
            expires_at_ms=(int(time.time()) + 3_600) * 1_000,
            secret_values=("real-looking-access",),
        )
        with self.assertRaisesRegex(bench.BenchmarkError, "fake test credentials"):
            bench.StreamingObserver._for_test(
                real,  # type: ignore[arg-type]
                "http://127.0.0.1:1/responses",
                provider_request_budget=1,
            )
        endpoints = ((socket.AF_INET, ("203.0.113.1", 443)),)
        with mock.patch.object(
            bench, "_resolve_upstream_endpoints", return_value=endpoints
        ) as resolve:
            self.assertEqual(
                bench.UpstreamTarget.official(),
                bench.UpstreamTarget(
                    "https",
                    "chatgpt.com",
                    None,
                    "/backend-api/codex/responses",
                    endpoints,
                ),
            )
        resolve.assert_called_once_with("chatgpt.com", 443)


def _run(
    *,
    first: int,
    answer: int,
    terminal: int,
    cache: int | None,
    functional: bool = True,
    tools: bool = False,
) -> dict[str, object]:
    return {
        "functional_valid": functional,
        "no_tool_calls": not tools,
        "timing_present": True,
        "cached_input_tokens": cache,
        "observer": {
            "timing_ns": {
                "time_to_first_model_output": first,
                "time_to_first_answer_text": answer,
                "time_to_terminal": terminal,
            }
        },
    }


def _pair(number: int, fx: dict[str, object], nano: dict[str, object]) -> dict[str, object]:
    return {"pair": number, "runs": {"fx": fx, "nanocodex": nano}}


class PairingSummaryTests(unittest.TestCase):
    def test_warm_primary_retains_product_specific_counts_and_excludes_unknown_or_cold(self) -> None:
        pairs = [
            _pair(
                1,
                _run(first=5_000_000, answer=6_000_000, terminal=7_000_000, cache=4),
                _run(first=3_000_000, answer=4_000_000, terminal=5_000_000, cache=7),
            ),
            _pair(
                2,
                _run(first=5_000_000, answer=6_000_000, terminal=7_000_000, cache=None),
                _run(first=4_000_000, answer=5_000_000, terminal=6_000_000, cache=None),
            ),
            _pair(
                3,
                _run(first=5_000_000, answer=6_000_000, terminal=7_000_000, cache=0),
                _run(first=4_000_000, answer=5_000_000, terminal=6_000_000, cache=9),
            ),
            _pair(
                4,
                _run(first=5_000_000, answer=6_000_000, terminal=7_000_000, cache=0),
                _run(first=4_000_000, answer=5_000_000, terminal=6_000_000, cache=0),
            ),
        ]
        summary = bench.summarize_pairs(pairs)

        self.assertEqual(summary["primary"]["eligible_pairs"], 1)
        self.assertEqual(summary["strict_equal_cache_diagnostic"]["eligible_pairs"], 1)
        self.assertEqual(summary["all_valid_diagnostic"]["eligible_pairs"], 4)
        self.assertEqual(summary["all_valid_diagnostic"]["cache_unknown_pairs"], 1)
        self.assertEqual(
            summary["primary_exclusions"]["cached_input_tokens_unknown_or_invalid"],
            1,
        )
        self.assertEqual(
            summary["primary_exclusions"]["cached_input_warm_cold_mismatch"], 1
        )
        self.assertEqual(
            summary["primary_exclusions"]["cached_input_both_cold"], 1
        )

    def test_nearest_rank_and_one_millisecond_tie_boundary(self) -> None:
        self.assertEqual(bench.percentile([1, 2, 100, 101], 0.50), 2)
        self.assertEqual(bench.percentile([1, 2, 100, 101], 0.95), 101)
        pairs = [
            _pair(1, _run(first=5_000_000, answer=5_000_000, terminal=5_000_000, cache=1), _run(first=6_000_000, answer=6_000_000, terminal=6_000_000, cache=2)),
            _pair(2, _run(first=5_000_000, answer=5_000_000, terminal=5_000_000, cache=1), _run(first=6_000_001, answer=6_000_001, terminal=6_000_001, cache=2)),
            _pair(3, _run(first=5_000_000, answer=5_000_000, terminal=5_000_000, cache=1), _run(first=3_999_999, answer=3_999_999, terminal=3_999_999, cache=2)),
        ]
        metric = bench.summarize_pairs(pairs)["primary"]["metrics"][
            "time_to_first_model_output"
        ]

        self.assertEqual(metric["wins"], {"nanocodex": 1, "fx": 1, "ties": 1})
        self.assertEqual(metric["win_counts_are"], "descriptive_only_not_inferential")
        self.assertEqual(metric["tie_threshold_ns"], 1_000_000)

    def test_minimum_primary_contract_is_twenty(self) -> None:
        pairs = [
            _pair(
                number,
                _run(first=3_000_000, answer=4_000_000, terminal=5_000_000, cache=2),
                _run(first=2_000_000, answer=3_000_000, terminal=4_000_000, cache=3),
            )
            for number in range(1, 21)
        ]
        summary = bench.summarize_pairs(pairs)
        self.assertEqual(summary["primary"]["eligible_pairs"], 20)
        self.assertEqual(summary["primary"]["minimum_eligible_pairs"], 20)


class EnvironmentAndProcessTests(unittest.TestCase):
    def test_child_egress_sandbox_allows_only_exact_observer_port(self) -> None:
        if sys.platform != "darwin":
            with self.assertRaisesRegex(bench.BenchmarkError, "requires the Darwin"):
                bench._sandboxed_child_command(
                    [sys.executable, "-c", "pass"], "http://127.0.0.1:1"
                )
            return

        tcp4_allowed = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        tcp4_denied = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        tcp6_denied = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
        udp4_denied = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        udp6_denied = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
        tcp6_denied.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        udp6_denied.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        tcp4_allowed.bind(("127.0.0.1", 0))
        tcp4_denied.bind(("127.0.0.1", 0))
        allowed_port = tcp4_allowed.getsockname()[1]
        denied_port = tcp4_denied.getsockname()[1]
        tcp6_denied.bind(("::1", allowed_port))
        udp4_denied.bind(("127.0.0.1", allowed_port))
        udp6_denied.bind(("::1", allowed_port))
        tcp4_allowed.listen(1)
        tcp4_denied.listen(1)
        tcp6_denied.listen(1)
        tcp4_allowed.settimeout(5)
        accepted = threading.Event()

        def accept_allowed() -> None:
            try:
                connection, _address = tcp4_allowed.accept()
                connection.close()
                accepted.set()
            except OSError:
                return

        accept_thread = threading.Thread(target=accept_allowed, daemon=True)
        accept_thread.start()
        script = """
import json
import socket
import sys

allowed_port = int(sys.argv[1])
denied_port = int(sys.argv[2])

def tcp(family, address):
    sock = socket.socket(family, socket.SOCK_STREAM)
    sock.settimeout(1)
    try:
        sock.connect(address)
        return {"ok": True}
    except OSError as error:
        return {"ok": False, "errno": error.errno}
    finally:
        sock.close()

def udp(family, address):
    sock = socket.socket(family, socket.SOCK_DGRAM)
    sock.settimeout(1)
    try:
        sent = sock.sendto(b"forbidden", address)
        return {"ok": True, "sent": sent}
    except OSError as error:
        return {"ok": False, "errno": error.errno}
    finally:
        sock.close()

print(json.dumps({
    "tcp4_allowed": tcp(socket.AF_INET, ("127.0.0.1", allowed_port)),
    "tcp4_other_port": tcp(socket.AF_INET, ("127.0.0.1", denied_port)),
    "tcp6_same_port": tcp(socket.AF_INET6, ("::1", allowed_port, 0, 0)),
    "udp4_same_port": udp(socket.AF_INET, ("127.0.0.1", allowed_port)),
    "udp6_same_port": udp(socket.AF_INET6, ("::1", allowed_port, 0, 0)),
}, sort_keys=True))
"""
        try:
            base_url = f"http://127.0.0.1:{allowed_port}"
            capture = bench._run_process(
                bench._sandboxed_child_command(
                    [
                        sys.executable,
                        "-c",
                        script,
                        str(allowed_port),
                        str(denied_port),
                    ],
                    base_url,
                ),
                Path.cwd(),
                {"PATH": os.environ.get("PATH") or os.defpath},
                timeout_seconds=5,
                stdout_limit=4096,
                stderr_limit=4096,
            )
            self.assertEqual(capture.returncode, 0, capture.stdout)
            results = json.loads(capture.stdout)
            self.assertEqual(results["tcp4_allowed"], {"ok": True})
            for name in (
                "tcp4_other_port",
                "tcp6_same_port",
                "udp4_same_port",
                "udp6_same_port",
            ):
                self.assertEqual(
                    results[name], {"ok": False, "errno": errno.EPERM}, name
                )
            self.assertTrue(accepted.wait(5))
            for listener in (tcp4_denied, tcp6_denied):
                listener.settimeout(0.2)
                with self.assertRaises(socket.timeout):
                    listener.accept()
            for receiver in (udp4_denied, udp6_denied):
                receiver.settimeout(0.2)
                with self.assertRaises(socket.timeout):
                    receiver.recvfrom(1024)
        finally:
            tcp4_allowed.close()
            tcp4_denied.close()
            tcp6_denied.close()
            udp4_denied.close()
            udp6_denied.close()
            accept_thread.join(timeout=5)

        with self.assertRaisesRegex(bench.BenchmarkError, "numeric loopback"):
            bench._sandboxed_child_command(
                [sys.executable, "-c", "pass"], f"http://localhost:{allowed_port}"
            )

    def test_child_sandbox_denies_process_creation_but_preserves_threads(self) -> None:
        if sys.platform != "darwin":
            self.skipTest("Darwin Seatbelt behavior is host-specific")

        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(5)
        port = listener.getsockname()[1]
        accepted = threading.Event()

        def accept_allowed() -> None:
            try:
                connection, _address = listener.accept()
                connection.close()
                accepted.set()
            except OSError:
                return

        accept_thread = threading.Thread(target=accept_allowed, daemon=True)
        accept_thread.start()
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            fork_marker = root / "forked"
            spawn_marker = root / "spawned"
            popen_marker = root / "popened"
            script = """
import json
import os
import pathlib
import socket
import subprocess
import sys
import threading

port = int(sys.argv[1])
fork_marker, spawn_marker, popen_marker = sys.argv[2:5]

def attempted(call):
    try:
        call()
        return {"ok": True}
    except OSError as error:
        return {"ok": False, "errno": error.errno}

def fork_child():
    pid = os.fork()
    if pid == 0:
        pathlib.Path(fork_marker).write_text("created")
        os._exit(0)
    os.waitpid(pid, 0)

def spawn_child():
    code = "import pathlib;pathlib.Path(%r).write_text('created')" % spawn_marker
    pid = os.posix_spawn(sys.executable, [sys.executable, "-c", code], os.environ)
    os.waitpid(pid, 0)

def popen_child():
    code = "import pathlib;pathlib.Path(%r).write_text('created')" % popen_marker
    subprocess.Popen(
        [sys.executable, "-c", code], start_new_session=True
    ).wait(timeout=5)

thread_result = []
thread = threading.Thread(target=lambda: thread_result.append("joined"))
thread.start()
thread.join()

connection = socket.create_connection(("127.0.0.1", port), timeout=2)
connection.close()

print(json.dumps({
    "identity": [os.getpid(), os.getpgrp(), os.getsid(0)],
    "setsid": attempted(os.setsid),
    "setpgid": attempted(lambda: os.setpgid(0, 0)),
    "fork": attempted(fork_child),
    "posix_spawn": attempted(spawn_child),
    "popen_new_session": attempted(popen_child),
    "thread_joined": thread_result == ["joined"],
    "tcp4_allowed": True,
}, sort_keys=True))
"""
            try:
                capture = bench._run_process(
                    bench._sandboxed_child_command(
                        [
                            sys.executable,
                            "-c",
                            script,
                            str(port),
                            str(fork_marker),
                            str(spawn_marker),
                            str(popen_marker),
                        ],
                        f"http://127.0.0.1:{port}",
                    ),
                    root,
                    {"PATH": os.environ.get("PATH") or os.defpath},
                    timeout_seconds=10,
                    stdout_limit=4096,
                    stderr_limit=4096,
                )
            finally:
                listener.close()
                accept_thread.join(timeout=5)

            self.assertEqual(capture.returncode, 0, capture.stdout)
            results = json.loads(capture.stdout)
            self.assertEqual(len(set(results["identity"])), 1)
            self.assertTrue(results["thread_joined"])
            self.assertTrue(results["tcp4_allowed"])
            self.assertTrue(accepted.is_set())
            for name in (
                "setsid",
                "setpgid",
                "fork",
                "posix_spawn",
                "popen_new_session",
            ):
                self.assertEqual(
                    results[name], {"ok": False, "errno": errno.EPERM}, name
                )
            self.assertFalse(fork_marker.exists())
            self.assertFalse(spawn_marker.exists())
            self.assertFalse(popen_marker.exists())

    def test_observer_reserves_allowed_port_on_every_ipv4_interface(self) -> None:
        if sys.platform != "darwin":
            self.skipTest("Darwin Seatbelt behavior is host-specific")

        candidates = {
            address
            for _family, _kind, _protocol, _canonical, (address, *_rest) in (
                socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET)
            )
            if not ipaddress.ip_address(address).is_loopback
        }
        if not candidates:
            route_probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                route_probe.connect(("192.0.2.1", 9))
                address = route_probe.getsockname()[0]
                if not ipaddress.ip_address(address).is_loopback:
                    candidates.add(address)
            finally:
                route_probe.close()
        if not candidates:
            self.skipTest("host has no non-loopback IPv4 interface")
        local_address = sorted(candidates)[0]

        auth = bench._fake_auth_material()
        with bench._OfflineProvider() as provider:
            with bench.StreamingObserver._for_test(
                auth, provider.url, provider_request_budget=1
            ) as observer:
                self.assertEqual(observer.server.server_address[0], "0.0.0.0")
                port = observer.server.server_port
                bypass = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                try:
                    with self.assertRaises(OSError):
                        bypass.bind((local_address, port))
                finally:
                    bypass.close()

                script = (
                    "import http.client,sys;"
                    "c=http.client.HTTPConnection(sys.argv[1],int(sys.argv[2]),timeout=2);"
                    "c.request('GET','/bypass');r=c.getresponse();"
                    "print(r.status);r.read();c.close()"
                )
                capture = bench._run_process(
                    bench._sandboxed_child_command(
                        [sys.executable, "-c", script, local_address, str(port)],
                        observer.base_url,
                    ),
                    Path.cwd(),
                    {"PATH": os.environ.get("PATH") or os.defpath},
                    timeout_seconds=5,
                    stdout_limit=1024,
                    stderr_limit=4096,
                )
                stats = observer.state.stats()

        self.assertEqual(capture.returncode, 0)
        self.assertEqual(capture.stdout, b"404\n")
        self.assertEqual(stats["unexpected_requests"], 1)
        self.assertEqual(stats["provider_requests_forwarded"], 0)
        self.assertEqual(provider.server.requests, 0)

    def test_child_environment_is_a_minimal_allowlist(self) -> None:
        poisoned = {
            "PATH": "/controlled/path",
            "HTTPS_PROXY": "http://secret-proxy",
            "AWS_SECRET_ACCESS_KEY": "secret",
            "GITHUB_TOKEN": "secret",
            "SSH_AUTH_SOCK": "/secret/socket",
            "DYLD_INSERT_LIBRARIES": "/secret/inject.dylib",
            "LD_PRELOAD": "/secret/inject.so",
            "UNRELATED_SECRET": "secret",
        }
        with tempfile.TemporaryDirectory() as raw, mock.patch.dict(
            os.environ, poisoned, clear=True
        ):
            home = Path(raw) / "home"
            home.mkdir(mode=0o700)
            environment = bench._child_environment(home)
            for name in (
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "TMPDIR",
            ):
                self.assertEqual(
                    stat.S_IMODE(Path(environment[name]).stat().st_mode), 0o700
                )

        self.assertEqual(
            set(environment),
            {
                "PATH",
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "TMPDIR",
                "NO_COLOR",
            },
        )
        self.assertEqual(environment["PATH"], "/controlled/path")
        self.assertFalse(any("PROXY" in key for key in environment))

    def test_timeout_kills_and_reaps_the_private_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            marker = root / "descendant.pid"
            script = (
                "import pathlib,subprocess,sys,time;"
                "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)']);"
                f"pathlib.Path({str(marker)!r}).write_text(str(p.pid));"
                "time.sleep(60)"
            )
            capture = bench._run_process(
                [sys.executable, "-c", script],
                root,
                {"PATH": os.environ.get("PATH") or os.defpath},
                timeout_seconds=0.5,
                stdout_limit=1024,
                stderr_limit=1024,
            )
            self.assertTrue(capture.timed_out)
            self.assertIsNone(capture.returncode)
            self.assertTrue(marker.exists())
            descendant = int(marker.read_text())
            _assert_process_gone(self, descendant, "timed-out")

    def test_output_is_bounded_while_produced_and_overflow_kills_child(self) -> None:
        for descriptor, stream_name in ((1, "stdout"), (2, "stderr")):
            with self.subTest(stream=stream_name), tempfile.TemporaryDirectory() as raw:
                root = Path(raw)
                marker = root / "descendant.pid"
                capture = bench._run_process(
                    [
                        sys.executable,
                        "-c",
                        (
                            "import os,pathlib,subprocess,sys,time;"
                            "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)']);"
                            f"pathlib.Path({str(marker)!r}).write_text(str(p.pid));"
                            f"os.write({descriptor},b'x'*8192);"
                            "time.sleep(60)"
                        ),
                    ],
                    root,
                    {"PATH": os.environ.get("PATH") or os.defpath},
                    timeout_seconds=5,
                    stdout_limit=1024,
                    stderr_limit=1024,
                )
                descendant = int(marker.read_text())

            self.assertTrue(capture.output_limit_exceeded)
            if stream_name == "stdout":
                self.assertTrue(capture.stdout_truncated)
                self.assertLessEqual(len(capture.stdout), 1024)
            else:
                self.assertTrue(capture.stderr_truncated)
                self.assertGreater(capture.stderr_bytes, 1024)
            _assert_process_gone(self, descendant, f"{stream_name}-overflow")

    def test_completed_leader_cannot_leave_descendant_holding_output_pipes(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            marker = root / "descendant.pid"
            script = (
                "import pathlib,subprocess,sys;"
                "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)']);"
                f"pathlib.Path({str(marker)!r}).write_text(str(p.pid))"
            )
            started = time.monotonic()
            capture = bench._run_process(
                [sys.executable, "-c", script],
                root,
                {"PATH": os.environ.get("PATH") or os.defpath},
                timeout_seconds=5,
                stdout_limit=1024,
                stderr_limit=1024,
            )
            elapsed = time.monotonic() - started
            descendant = int(marker.read_text())

        self.assertEqual(capture.returncode, 0)
        self.assertFalse(capture.timed_out)
        self.assertLess(elapsed, 2)
        _assert_process_gone(self, descendant, "completed-leader")

    def test_process_supervision_does_not_depend_on_drainer_threads(self) -> None:
        with (
            tempfile.TemporaryDirectory() as raw,
            mock.patch.object(
                bench.threading.Thread,
                "start",
                side_effect=RuntimeError("injected drainer start failure"),
            ),
        ):
            capture = bench._run_process(
                [sys.executable, "-c", "print('selector-owned')"],
                Path(raw),
                {"PATH": os.environ.get("PATH") or os.defpath},
                timeout_seconds=5,
                stdout_limit=1024,
                stderr_limit=1024,
            )

        self.assertEqual(capture.returncode, 0)
        self.assertEqual(capture.stdout, b"selector-owned\n")

    def test_selector_is_owned_before_spawn_and_setup_failures_reap_child(self) -> None:
        with tempfile.TemporaryDirectory() as raw, mock.patch.object(
            bench.selectors,
            "DefaultSelector",
            side_effect=OSError("injected selector construction failure"),
        ), mock.patch.object(bench.subprocess, "Popen") as popen:
            with self.assertRaisesRegex(OSError, "selector construction"):
                bench._run_process(
                    [sys.executable, "-c", "pass"],
                    Path(raw),
                    {"PATH": os.environ.get("PATH") or os.defpath},
                )
        popen.assert_not_called()

        real_popen = subprocess.Popen
        for failure in ("set_blocking", "register"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as raw:
                observed: list[subprocess.Popen[bytes]] = []

                def capture_popen(
                    *args: object, **kwargs: object
                ) -> subprocess.Popen[bytes]:
                    process = real_popen(*args, **kwargs)
                    observed.append(process)
                    return process

                patches = [
                    mock.patch.object(
                        bench.subprocess, "Popen", side_effect=capture_popen
                    )
                ]
                if failure == "set_blocking":
                    patches.append(
                        mock.patch.object(
                            bench.os,
                            "set_blocking",
                            side_effect=OSError("injected set_blocking failure"),
                        )
                    )
                else:
                    real_selector = bench.selectors.DefaultSelector()

                    class RegisterFailure:
                        def register(self, *_args: object, **_kwargs: object) -> None:
                            raise OSError("injected register failure")

                        def unregister(self, value: object) -> object:
                            return real_selector.unregister(value)

                        def select(self, timeout: float | None = None) -> list[object]:
                            return real_selector.select(timeout)

                        def close(self) -> None:
                            real_selector.close()

                    patches.append(
                        mock.patch.object(
                            bench.selectors,
                            "DefaultSelector",
                            return_value=RegisterFailure(),
                        )
                    )
                with contextlib.ExitStack() as stack:
                    for patch in patches:
                        stack.enter_context(patch)
                    with self.assertRaises(OSError):
                        bench._run_process(
                            [sys.executable, "-c", "import time;time.sleep(60)"],
                            Path(raw),
                            {"PATH": os.environ.get("PATH") or os.defpath},
                            timeout_seconds=5,
                            stdout_limit=1024,
                            stderr_limit=1024,
                        )
                self.assertEqual(len(observed), 1)
                _assert_process_gone(self, observed[0].pid, failure)

    def test_unverifiable_group_termination_raises_instead_of_returning(self) -> None:
        observed: list[subprocess.Popen[bytes]] = []
        real_popen = subprocess.Popen
        signal_times: list[float] = []

        def capture_popen(*args: object, **kwargs: object) -> subprocess.Popen[bytes]:
            process = real_popen(*args, **kwargs)
            observed.append(process)
            return process

        def unavailable_killpg(_group: int, _value: int) -> None:
            signal_times.append(time.monotonic())
            raise PermissionError("injected kill failure")

        with (
            tempfile.TemporaryDirectory() as raw,
            mock.patch.object(bench.subprocess, "Popen", side_effect=capture_popen),
            mock.patch.object(bench.os, "killpg", side_effect=unavailable_killpg),
            mock.patch.object(bench, "PROCESS_SHUTDOWN_TIMEOUT_SECONDS", 0.15),
            self.assertRaisesRegex(
                bench.BenchmarkError, "cleanup could not be verified"
            ),
        ):
            bench._run_process(
                [sys.executable, "-c", "import time; time.sleep(60)"],
                Path(raw),
                {"PATH": os.environ.get("PATH") or os.defpath},
                timeout_seconds=0.1,
                stdout_limit=1024,
                stderr_limit=1024,
            )

        self.assertEqual(len(observed), 1)
        self.assertGreaterEqual(len(signal_times), 4)
        self.assertGreaterEqual(signal_times[-1] - signal_times[0], 0.1)
        _assert_process_gone(self, observed[0].pid, "kill-failure-cleanup")

    @unittest.skipUnless(os.name == "posix", "process-group cleanup requires POSIX")
    def test_multiple_transient_group_signal_failures_still_remove_entire_group(
        self,
    ) -> None:
        real_killpg = os.killpg
        real_popen = subprocess.Popen
        signal_attempts = 0
        observed: list[subprocess.Popen[bytes]] = []

        def transient_killpg(group: int, value: int) -> None:
            nonlocal signal_attempts
            if value == signal.SIGKILL:
                signal_attempts += 1
                if signal_attempts <= 2:
                    raise PermissionError(
                        f"injected transient group signal failure {signal_attempts}"
                    )
            real_killpg(group, value)

        def capture_popen(*args: object, **kwargs: object) -> subprocess.Popen[bytes]:
            process = real_popen(*args, **kwargs)
            observed.append(process)
            return process

        with tempfile.TemporaryDirectory() as raw:
            marker = Path(raw) / "processes.json"
            script = (
                "import json,os,pathlib,subprocess,sys,time;"
                "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)']);"
                f"pathlib.Path({str(marker)!r}).write_text("
                "json.dumps([os.getpid(),os.getpgrp(),p.pid]));"
                "time.sleep(60)"
            )
            with mock.patch.object(
                bench.subprocess, "Popen", side_effect=capture_popen
            ), mock.patch.object(bench.os, "killpg", side_effect=transient_killpg):
                capture = bench._run_process(
                    [sys.executable, "-c", script],
                    Path(raw),
                    {"PATH": os.environ.get("PATH") or os.defpath},
                    timeout_seconds=0.5,
                    stdout_limit=1024,
                    stderr_limit=1024,
                )
            parent_pid, group_id, descendant_pid = json.loads(marker.read_text())

        self.assertTrue(capture.timed_out)
        self.assertEqual(len(observed), 1)
        self.assertEqual(parent_pid, observed[0].pid)
        self.assertEqual(group_id, parent_pid)
        self.assertGreaterEqual(signal_attempts, 3)
        self.assertEqual(observed[0].returncode, -signal.SIGKILL)
        for label, pid in (("parent", parent_pid), ("descendant", descendant_pid)):
            with self.subTest(process=label), self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
        with self.assertRaises(ProcessLookupError):
            os.killpg(group_id, 0)

    @unittest.skipUnless(os.name == "posix", "waitpid fallback requires POSIX")
    def test_recoverable_wait_and_waitpid_failures_still_reap_leader(self) -> None:
        process = subprocess.Popen(
            [sys.executable, "-c", "import time;time.sleep(60)"],
            start_new_session=True,
        )
        real_waitpid = os.waitpid
        wait_calls = 0
        waitpid_calls = 0

        def recoverable_wait(*, timeout: float | None = None) -> int:
            nonlocal wait_calls
            wait_calls += 1
            if wait_calls == 1:
                raise subprocess.TimeoutExpired(process.args, timeout)
            if wait_calls == 2:
                raise InterruptedError(errno.EINTR, "injected interrupted wait")
            raise OSError(errno.EAGAIN, "injected recoverable wait failure")

        def transient_waitpid(pid: int, options: int) -> tuple[int, int]:
            nonlocal waitpid_calls
            waitpid_calls += 1
            if waitpid_calls == 1:
                raise InterruptedError(errno.EINTR, "injected interrupted waitpid")
            if waitpid_calls == 2:
                raise OSError(errno.EAGAIN, "injected recoverable waitpid failure")
            return real_waitpid(pid, options)

        process.wait = recoverable_wait  # type: ignore[method-assign]
        process.kill = lambda: os.kill(  # type: ignore[method-assign]
            process.pid, signal.SIGKILL
        )
        with mock.patch.object(bench.os, "waitpid", side_effect=transient_waitpid):
            bench._kill_process_group(process, deadline=time.monotonic() + 2)

        self.assertGreaterEqual(wait_calls, 3)
        self.assertGreaterEqual(waitpid_calls, 3)
        self.assertEqual(process.returncode, -signal.SIGKILL)
        with self.assertRaises(ProcessLookupError):
            os.kill(process.pid, 0)
        with self.assertRaises(ProcessLookupError):
            os.killpg(process.pid, 0)


class TrackedServerLifecycleTests(unittest.TestCase):
    @contextlib.contextmanager
    def _stalled_tls_peer(self):
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(0.05)
        accepted = threading.Event()
        client_bytes = threading.Event()
        disconnected = threading.Event()
        stop = threading.Event()
        peer_lock = threading.Lock()
        peer_socket: list[socket.socket] = []

        def serve() -> None:
            peer: socket.socket | None = None
            try:
                while not stop.is_set():
                    try:
                        peer, _address = listener.accept()
                        break
                    except TimeoutError:
                        continue
                    except OSError:
                        return
                if peer is None:
                    return
                with peer_lock:
                    peer_socket.append(peer)
                accepted.set()
                peer.settimeout(0.05)
                while not stop.is_set():
                    try:
                        chunk = peer.recv(64 * 1024)
                    except TimeoutError:
                        continue
                    except OSError:
                        disconnected.set()
                        return
                    if not chunk:
                        disconnected.set()
                        return
                    client_bytes.set()
            finally:
                if peer is not None:
                    with contextlib.suppress(OSError):
                        peer.close()

        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        try:
            yield {
                "endpoint": (socket.AF_INET, listener.getsockname()),
                "accepted": accepted,
                "client_bytes": client_bytes,
                "disconnected": disconnected,
            }
        finally:
            stop.set()
            with peer_lock:
                peer = peer_socket[0] if peer_socket else None
            if peer is not None:
                with contextlib.suppress(OSError):
                    peer.shutdown(socket.SHUT_RDWR)
                with contextlib.suppress(OSError):
                    peer.close()
            with contextlib.suppress(OSError):
                listener.close()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())

    def test_bound_server_thread_start_failure_closes_every_listener(self) -> None:
        auth = bench._fake_auth_material()
        owners = (
            (
                "offline provider",
                bench._OfflineProvider(),
                "127.0.0.1",
            ),
            (
                "streaming observer",
                bench.StreamingObserver._for_test(
                    auth,
                    "http://127.0.0.1:9/responses",
                    provider_request_budget=1,
                ),
                "0.0.0.0",
            ),
        )
        for label, owner, bind_host in owners:
            with self.subTest(owner=label):
                port = owner.server.server_port
                with mock.patch.object(
                    owner._thread,
                    "start",
                    side_effect=RuntimeError("injected thread start failure"),
                ), self.assertRaisesRegex(bench.BenchmarkError, "failed to start"):
                    owner.__enter__()
                self.assertFalse(owner._thread.is_alive())
                self.assertEqual(owner.server.active_work_count(), 0)
                self.assertEqual(owner.server.socket.fileno(), -1)
                replacement = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                try:
                    replacement.bind((bind_host, port))
                finally:
                    replacement.close()

    def test_server_thread_construction_failure_closes_bound_listener(self) -> None:
        created: list[bench._OfflineProviderServer] = []
        real_server = bench._OfflineProviderServer

        def capture_server(*, stall_after_headers: bool = False) -> bench._OfflineProviderServer:
            server = real_server(stall_after_headers=stall_after_headers)
            created.append(server)
            return server

        with mock.patch.object(
            bench, "_OfflineProviderServer", side_effect=capture_server
        ), mock.patch.object(
            bench.threading,
            "Thread",
            side_effect=RuntimeError("injected thread construction failure"),
        ), self.assertRaisesRegex(bench.BenchmarkError, "thread construction"):
            bench._OfflineProvider()

        self.assertEqual(len(created), 1)
        self.assertEqual(created[0].socket.fileno(), -1)
        self.assertEqual(created[0].active_work_count(), 0)

    def test_shutdown_interrupts_connection_before_connect_completes(self) -> None:
        auth = bench._fake_auth_material()
        started = threading.Event()
        aborted = threading.Event()
        client_finished = threading.Event()

        class BlockingConnection:
            def __init__(self) -> None:
                self.sock, self.peer = socket.socketpair()

            def connect(self) -> None:
                started.set()
                try:
                    self.sock.recv(1)
                except OSError as error:
                    raise OSError("connect interrupted") from error
                raise OSError("connect unexpectedly resumed")

            def abort_connect(self) -> None:
                aborted.set()
                with contextlib.suppress(OSError):
                    self.sock.shutdown(socket.SHUT_RDWR)
                self.sock.close()

            def close(self) -> None:
                self.sock.close()

        connection = BlockingConnection()
        observer = bench.StreamingObserver._for_test(
            auth,
            "http://127.0.0.1:9/responses",
            provider_request_budget=1,
        )
        observer.__enter__()
        observer.state.arm(bench.ExpectedRequest("nanocodex", 1, "test"))

        def post() -> None:
            try:
                _post_observer(
                    observer,
                    _compact(_nanocodex_request()),
                    auth,
                    "nanocodex",
                )
            except (OSError, http.client.HTTPException):
                pass
            finally:
                client_finished.set()

        client = threading.Thread(target=post, daemon=True)
        with _fixture_contracts(), mock.patch.object(
            bench.UpstreamTarget, "connection", return_value=connection
        ):
            client.start()
            self.assertTrue(started.wait(5))
            observer.__exit__(None, None, None)
        client.join(timeout=5)
        connection.peer.close()

        self.assertTrue(aborted.is_set())
        self.assertTrue(client_finished.is_set())
        self.assertFalse(client.is_alive())
        self.assertFalse(observer._thread.is_alive())
        self.assertEqual(observer.server.active_work_count(), 0)
        self.assertEqual(observer.server.socket.fileno(), -1)

    def test_observer_interrupts_abort_hooks_and_all_unique_retained_sockets(self) -> None:
        first_socket = mock.Mock(spec=socket.socket)
        second_socket = mock.Mock(spec=socket.socket)
        first_socket.shutdown.side_effect = OSError("already disconnected")
        first_socket.close.side_effect = OSError("already closed")

        class Connection:
            def __init__(
                self, upstream_socket: socket.socket, *, abort_fails: bool
            ) -> None:
                self.sock: socket.socket | None = upstream_socket
                self.abort_fails = abort_fails
                self.abort_calls = 0

            def abort_connect(self) -> None:
                self.abort_calls += 1
                if self.abort_fails:
                    raise OSError("injected abort failure")

        first = Connection(first_socket, abort_fails=True)
        second = Connection(second_socket, abort_fails=False)
        target = bench.UpstreamTarget._for_test("http://127.0.0.1:9/responses")
        server = bench._ObserverServer(("127.0.0.1", 0), mock.Mock(), target)
        try:
            server.register_upstream(first)  # type: ignore[arg-type]
            server.connected_upstream(first)  # type: ignore[arg-type]
            first.sock = None
            server.register_upstream(second)  # type: ignore[arg-type]
            server.connected_upstream(second)  # type: ignore[arg-type]

            server.interrupt_active_work()

            self.assertEqual(first.abort_calls, 1)
            self.assertEqual(second.abort_calls, 1)
            first_socket.shutdown.assert_called_once_with(socket.SHUT_RDWR)
            first_socket.close.assert_called_once_with()
            second_socket.shutdown.assert_called_once_with(socket.SHUT_RDWR)
            second_socket.close.assert_called_once_with()
        finally:
            server.unregister_upstream(first)  # type: ignore[arg-type]
            server.unregister_upstream(second)  # type: ignore[arg-type]
            server.server_close()

        self.assertEqual(server.active_work_count(), 0)
        self.assertEqual(server.socket.fileno(), -1)

    def test_observer_snapshots_upstream_before_handler_can_unregister_it(self) -> None:
        retained_socket = mock.Mock(spec=socket.socket)

        class Connection:
            def __init__(self) -> None:
                self.sock: socket.socket | None = retained_socket
                self.abort_calls = 0

            def abort_connect(self) -> None:
                self.abort_calls += 1

        connection = Connection()
        target = bench.UpstreamTarget._for_test("http://127.0.0.1:9/responses")
        server = bench._ObserverServer(("127.0.0.1", 0), mock.Mock(), target)
        try:
            server.register_upstream(connection)  # type: ignore[arg-type]
            server.connected_upstream(connection)  # type: ignore[arg-type]
            connection.sock = None

            def unregister_during_interrupt() -> None:
                server.unregister_upstream(connection)  # type: ignore[arg-type]

            with mock.patch.object(
                bench._TrackedThreadingHTTPServer,
                "interrupt_active_work",
                side_effect=unregister_during_interrupt,
            ):
                server.interrupt_active_work()

            self.assertEqual(connection.abort_calls, 1)
            retained_socket.shutdown.assert_called_once_with(socket.SHUT_RDWR)
            retained_socket.close.assert_called_once_with()
        finally:
            server.unregister_upstream(connection)  # type: ignore[arg-type]
            server.server_close()

        self.assertEqual(server.active_work_count(), 0)
        self.assertEqual(server.socket.fileno(), -1)

    def test_shutdown_during_connected_handoff_closes_prior_and_current_sockets(
        self,
    ) -> None:
        prior_socket = mock.Mock(spec=socket.socket)
        current_socket = mock.Mock(spec=socket.socket)

        class Connection:
            def __init__(self) -> None:
                self.sock: socket.socket | None = prior_socket
                self.abort_calls = 0

            def abort_connect(self) -> None:
                self.abort_calls += 1

        connection = Connection()
        target = bench.UpstreamTarget._for_test("http://127.0.0.1:9/responses")
        server = bench._ObserverServer(("127.0.0.1", 0), mock.Mock(), target)
        try:
            server.register_upstream(connection)  # type: ignore[arg-type]
            connection.sock = current_socket
            server.begin_shutdown()
            with self.assertRaisesRegex(OSError, "shutting down"):
                server.connected_upstream(connection)  # type: ignore[arg-type]

            self.assertEqual(connection.abort_calls, 1)
            prior_socket.shutdown.assert_called_once_with(socket.SHUT_RDWR)
            prior_socket.close.assert_called_once_with()
            current_socket.shutdown.assert_called_once_with(socket.SHUT_RDWR)
            current_socket.close.assert_called_once_with()
        finally:
            server.unregister_upstream(connection)  # type: ignore[arg-type]
            server.server_close()

        self.assertEqual(server.active_work_count(), 0)
        self.assertEqual(server.socket.fileno(), -1)

    def test_cancellation_during_tcp_socket_construction_handoff(self) -> None:
        with self._stalled_tls_peer() as peer:
            connection = bench._ResolvedHTTPConnection(
                "127.0.0.1",
                peer["endpoint"][1][1],
                (peer["endpoint"],),
                timeout=5,
            )
            candidate = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            constructing = threading.Event()
            release = threading.Event()
            errors: list[BaseException] = []

            def delayed_socket(
                _family: int, _kind: int, _protocol: int
            ) -> socket.socket:
                constructing.set()
                if not release.wait(5):
                    raise AssertionError("socket construction handoff was not released")
                return candidate

            def connect() -> None:
                try:
                    connection.connect()
                except BaseException as error:
                    errors.append(error)

            thread = threading.Thread(target=connect, daemon=True)
            with mock.patch.object(
                bench.socket, "socket", side_effect=delayed_socket
            ):
                thread.start()
                try:
                    self.assertTrue(constructing.wait(5))
                    self.assertIsNone(connection.sock)
                    connection.abort_connect()
                finally:
                    release.set()
                thread.join(timeout=5)

            self.assertFalse(thread.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertIsInstance(errors[0], OSError)
            self.assertRegex(str(errors[0]), "cancelled")
            self.assertIsNone(connection.sock)
            self.assertEqual(candidate.fileno(), -1)
            self.assertFalse(peer["accepted"].wait(0.1))

    def test_cancellation_at_each_tcp_socket_publication_checkpoint(self) -> None:
        for checkpoint in (2, 3):
            with self.subTest(checkpoint=checkpoint), self._stalled_tls_peer() as peer:
                connection = bench._ResolvedHTTPConnection(
                    "127.0.0.1",
                    peer["endpoint"][1][1],
                    (peer["endpoint"],),
                    timeout=5,
                )
                reached = threading.Event()
                release = threading.Event()
                errors: list[BaseException] = []
                check_calls = 0
                real_check = connection._raise_if_connect_cancelled

                def controlled_check() -> None:
                    nonlocal check_calls
                    check_calls += 1
                    if check_calls == checkpoint:
                        reached.set()
                        if not release.wait(5):
                            raise AssertionError("publication checkpoint was not released")
                    real_check()

                def connect() -> None:
                    try:
                        connection.connect()
                    except BaseException as error:
                        errors.append(error)

                thread = threading.Thread(target=connect, daemon=True)
                with mock.patch.object(
                    connection,
                    "_raise_if_connect_cancelled",
                    side_effect=controlled_check,
                ):
                    thread.start()
                    self.assertTrue(reached.wait(5))
                    self.assertIsNotNone(connection.sock)
                    connection.abort_connect()
                    release.set()
                    thread.join(timeout=5)

                self.assertFalse(thread.is_alive())
                self.assertEqual(len(errors), 1)
                self.assertIsInstance(errors[0], OSError)
                self.assertRegex(str(errors[0]), "cancelled")
                self.assertIsNone(connection.sock)
                if checkpoint == 3:
                    self.assertTrue(peer["accepted"].wait(5))
                    self.assertTrue(peer["disconnected"].wait(5))

    def test_cancellation_during_tls_socket_publication_handoff(self) -> None:
        with self._stalled_tls_peer() as peer:
            real_context = bench.ssl.SSLContext(bench.ssl.PROTOCOL_TLS_CLIENT)
            real_context.check_hostname = False
            real_context.verify_mode = bench.ssl.CERT_NONE
            wrapped = threading.Event()
            release = threading.Event()
            wrapped_socket: list[bench.ssl.SSLSocket] = []

            class PausingContext:
                def wrap_socket(
                    self,
                    raw_socket: socket.socket,
                    *,
                    server_hostname: str,
                    do_handshake_on_connect: bool,
                ) -> bench.ssl.SSLSocket:
                    tls_socket = real_context.wrap_socket(
                        raw_socket,
                        server_hostname=server_hostname,
                        do_handshake_on_connect=do_handshake_on_connect,
                    )
                    wrapped_socket.append(tls_socket)
                    wrapped.set()
                    if not release.wait(5):
                        raise AssertionError("TLS ownership handoff was not released")
                    return tls_socket

            connection = bench._ResolvedHTTPSConnection(
                "127.0.0.1",
                peer["endpoint"][1][1],
                (peer["endpoint"],),
                timeout=5,
                context=PausingContext(),  # type: ignore[arg-type]
            )
            errors: list[BaseException] = []

            def connect() -> None:
                try:
                    connection.connect()
                except BaseException as error:
                    errors.append(error)

            thread = threading.Thread(target=connect, daemon=True)
            thread.start()
            try:
                self.assertTrue(wrapped.wait(5))
                self.assertTrue(peer["accepted"].wait(5))
                self.assertIsNotNone(connection.sock)
                self.assertEqual(connection.sock.fileno(), -1)
                self.assertEqual(len(wrapped_socket), 1)
                self.assertGreaterEqual(wrapped_socket[0].fileno(), 0)
                connection.abort_connect()
            finally:
                release.set()
            thread.join(timeout=5)

            self.assertFalse(thread.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertIsInstance(errors[0], OSError)
            self.assertRegex(str(errors[0]), "cancelled")
            self.assertIsNone(connection.sock)
            self.assertTrue(peer["disconnected"].wait(5))
            self.assertFalse(peer["client_bytes"].is_set())
            self.assertEqual(wrapped_socket[0].fileno(), -1)

    def test_real_loopback_tls_stall_uses_one_tcp_and_tls_deadline(self) -> None:
        with self._stalled_tls_peer() as peer:
            context = bench.ssl.SSLContext(bench.ssl.PROTOCOL_TLS_CLIENT)
            context.check_hostname = False
            context.verify_mode = bench.ssl.CERT_NONE
            connection = bench._ResolvedHTTPSConnection(
                "127.0.0.1",
                peer["endpoint"][1][1],
                (peer["endpoint"],),
                timeout=0.7,
                context=context,
            )
            real_tcp_connect = connection._connect_resolved_tcp

            def consume_tcp_budget(deadline: float) -> socket.socket:
                connected = real_tcp_connect(deadline)
                time.sleep(0.45)
                return connected

            started = time.monotonic()
            with mock.patch.object(
                connection,
                "_connect_resolved_tcp",
                side_effect=consume_tcp_budget,
            ), self.assertRaises(TimeoutError):
                connection.connect()
            elapsed = time.monotonic() - started

            self.assertTrue(peer["accepted"].wait(5))
            self.assertTrue(peer["client_bytes"].wait(5))
            self.assertTrue(peer["disconnected"].wait(5))
            self.assertIsNone(connection.sock)
            self.assertLess(elapsed, 0.95)

    def test_real_loopback_tls_stall_is_interrupted_without_leaking_a_socket(self) -> None:
        with self._stalled_tls_peer() as peer:
            context = bench.ssl.SSLContext(bench.ssl.PROTOCOL_TLS_CLIENT)
            context.check_hostname = False
            context.verify_mode = bench.ssl.CERT_NONE
            connection = bench._ResolvedHTTPSConnection(
                "127.0.0.1",
                peer["endpoint"][1][1],
                (peer["endpoint"],),
                timeout=5,
                context=context,
            )
            errors: list[BaseException] = []

            def connect() -> None:
                try:
                    connection.connect()
                except BaseException as error:
                    errors.append(error)

            thread = threading.Thread(target=connect, daemon=True)
            thread.start()
            self.assertTrue(peer["client_bytes"].wait(5))
            self.assertIsInstance(connection.sock, bench.ssl.SSLSocket)
            connection.abort_connect()
            thread.join(timeout=5)

            self.assertFalse(thread.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertIsInstance(errors[0], OSError)
            self.assertTrue(peer["disconnected"].wait(5))
            self.assertIsNone(connection.sock)

    def test_nonquiescent_shutdown_fails_before_publication(self) -> None:
        server = mock.Mock(spec=bench._TrackedThreadingHTTPServer)
        server.wait_for_quiescence.return_value = 1
        thread = mock.Mock(spec=threading.Thread)
        thread.is_alive.return_value = False
        published = False

        def close_then_publish() -> None:
            nonlocal published
            bench._close_tracked_server(server, thread, "test server")
            published = True

        with mock.patch.object(
            bench, "OBSERVER_SHUTDOWN_TIMEOUT_SECONDS", 0
        ), self.assertRaisesRegex(bench.BenchmarkError, "did not quiesce"):
            close_then_publish()

        self.assertFalse(published)
        server.begin_shutdown.assert_called_once_with()
        server.interrupt_active_work.assert_called_once_with()
        thread.join.assert_called_once()


class PrivateWorkspaceTests(unittest.TestCase):
    class _State:
        def __init__(self) -> None:
            self.ticket = 0

        def stats(self) -> dict[str, int]:
            return {"unexpected_requests": 0}

        def arm(self, _expected: object) -> int:
            self.ticket += 1
            return self.ticket

        def cancel_unclaimed(self, _ticket: int) -> bool:
            return True

    class _Observer:
        base_url = "http://127.0.0.1:1"

        def __init__(self) -> None:
            self.state = PrivateWorkspaceTests._State()

    def test_both_products_run_in_fresh_private_empty_workspaces(self) -> None:
        auth = bench._fake_auth_material()
        commit = "a" * 40
        args = argparse.Namespace(nanocodex_commit=commit)
        observed: list[tuple[str, Path, dict[str, str]]] = []

        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            executable = root / "immutable"
            executable.write_bytes(b"executable")
            executable.chmod(0o500)
            digest = bench.sha256_file(executable)
            executables = {
                "fx": (executable, digest),
                "nanocodex": (executable, digest),
            }

            def fake_run(
                command: list[str],
                cwd: Path,
                environment: dict[str, str],
                **_kwargs: object,
            ) -> bench.ProcessCapture:
                self.assertEqual(command[0], str(bench.SANDBOX_EXEC))
                self.assertEqual(list(cwd.iterdir()), [])
                self.assertEqual(stat.S_IMODE(cwd.stat().st_mode), 0o700)
                self.assertEqual(cwd.parent, Path(environment["HOME"]))
                implementation = (
                    "nanocodex" if "--source-commit" in command else "fx"
                )
                observed.append((implementation, cwd, dict(environment)))
                if implementation == "fx":
                    output = {
                        "output": bench.ACK_TOKEN,
                        "exit_code": 0,
                        "model": bench.MODEL,
                        "tool_calls": [],
                    }
                else:
                    self.assertEqual(command[command.index("--cwd") + 1], str(cwd))
                    self.assertEqual(command[command.index("--transport") + 1], "https")
                    output = _nano_output(commit, cwd)
                return bench.ProcessCapture(0, _compact(output), 0, False)

            observer = self._Observer()
            with mock.patch.object(bench, "_run_process", side_effect=fake_run):
                implementations = ("fx", "nanocodex", "nanocodex", "fx")
                for sequence, implementation in enumerate(implementations, 1):
                    bench._run_implementation(
                        implementation,
                        pair=(sequence + 1) // 2,
                        phase="test",
                        sequence=sequence,
                        args=args,
                        auth=auth,
                        observer=observer,  # type: ignore[arg-type]
                        runtime_root=root,
                        executables=executables,
                    )

        self.assertEqual(
            [item[0] for item in observed],
            ["fx", "nanocodex", "nanocodex", "fx"],
        )
        self.assertEqual(len({item[1] for item in observed}), 4)
        self.assertTrue(all(not workspace.exists() for _, workspace, _ in observed))


class SourceProvenanceTests(unittest.TestCase):
    def _fixture(self, root: Path) -> tuple[Path, str]:
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        subprocess.run(
            ["git", "-C", str(root), "config", "user.email", "test@example.invalid"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(root), "config", "user.name", "Offline Test"],
            check=True,
        )
        executable = root / "bin" / "product"
        executable.parent.mkdir()
        executable.write_text("#!/bin/sh\nexit 0\n")
        executable.chmod(0o755)
        subprocess.run(["git", "-C", str(root), "add", "bin/product"], check=True)
        subprocess.run(
            ["git", "-C", str(root), "commit", "-q", "-m", "fixture"], check=True
        )
        head = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "HEAD"],
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        ).stdout.strip()
        return executable, head

    def test_clean_exact_head_binary_beneath_root_and_private_copy(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "source"
            root.mkdir()
            executable, head = self._fixture(root)
            provenance = bench._source_provenance(
                root, executable, head, "nanocodex"
            )
            runtime = Path(raw) / "runtime"
            runtime.mkdir(mode=0o700)
            copied = bench._copy_executable(provenance, runtime)

            self.assertEqual(provenance["git_head"], head)
            self.assertTrue(provenance["tree_clean"])
            self.assertEqual(stat.S_IMODE(copied.stat().st_mode), 0o500)
            self.assertEqual(
                bench.sha256_file(copied), provenance["binary"]["sha256"]
            )
            copied.chmod(0o700)
            with self.assertRaisesRegex(bench.BenchmarkError, "immutable digest"):
                bench._assert_executable_copy(
                    copied, provenance["binary"]["sha256"]
                )

    def test_rejects_dirty_mismatched_or_escaping_sources(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            dirty_root = base / "dirty"
            dirty_root.mkdir()
            dirty_bin, dirty_head = self._fixture(dirty_root)
            (dirty_root / "untracked-secret").write_text("dirty")
            with self.assertRaisesRegex(bench.BenchmarkError, "not clean"):
                bench._source_provenance(
                    dirty_root, dirty_bin, dirty_head, "nanocodex"
                )

            mismatch_root = base / "mismatch"
            mismatch_root.mkdir()
            mismatch_bin, _ = self._fixture(mismatch_root)
            with self.assertRaisesRegex(bench.BenchmarkError, "HEAD mismatch"):
                bench._source_provenance(
                    mismatch_root, mismatch_bin, "0" * 40, "nanocodex"
                )

            escape_root = base / "escape"
            escape_root.mkdir()
            _, escape_head = self._fixture(escape_root)
            outside = base / "outside"
            outside.write_text("#!/bin/sh\n")
            outside.chmod(0o755)
            with self.assertRaisesRegex(bench.BenchmarkError, "beneath"):
                bench._source_provenance(
                    escape_root, outside, escape_head, "nanocodex"
                )

        self.assertEqual(
            bench.FX_COMMIT, "4a8f765c94f4e205ecae293d6d5c98ec9aef2200"
        )

    def test_end_of_run_source_recheck_detects_tree_or_binary_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            executable, head = self._fixture(root)
            provenance = bench._source_provenance(
                root, executable, head, "nanocodex"
            )
            self.assertTrue(bench._source_unchanged(provenance))

            executable.write_text("#!/bin/sh\nexit 1\n")
            self.assertFalse(bench._source_unchanged(provenance))


class ActualBinaryPreflightIntegrationTests(unittest.TestCase):
    """Explicit operator-supplied artifact gate; not part of unit discovery."""

    def actual_binary_preflight(self) -> None:
        self.assertEqual(sys.platform, "darwin")
        required_environment = {
            name: os.environ.get(name)
            for name in (
                "NANOCODEX_PAIRED_FX_SOURCE_ROOT",
                "NANOCODEX_PAIRED_FX_BIN",
                "NANOCODEX_PAIRED_NANOCODEX_BIN",
                "NANOCODEX_PAIRED_NANOCODEX_COMMIT",
            )
        }
        self.assertTrue(
            all(required_environment.values()),
            f"missing actual-binary environment: {required_environment}",
        )
        fx_source_root = Path(
            required_environment["NANOCODEX_PAIRED_FX_SOURCE_ROOT"] or ""
        ).resolve(strict=True)
        fx_binary = Path(
            required_environment["NANOCODEX_PAIRED_FX_BIN"] or ""
        ).resolve(strict=True)
        nanocodex_binary = Path(
            required_environment["NANOCODEX_PAIRED_NANOCODEX_BIN"] or ""
        ).resolve(strict=True)
        nanocodex_commit = required_environment[
            "NANOCODEX_PAIRED_NANOCODEX_COMMIT"
        ]
        self.assertIsNotNone(nanocodex_commit)
        self.assertEqual(
            subprocess.run(
                ["git", "-C", str(fx_source_root), "rev-parse", "HEAD"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ).stdout.decode("ascii").strip(),
            bench.FX_COMMIT,
        )
        self.assertEqual(
            subprocess.run(
                ["git", "-C", str(fx_source_root), "status", "--porcelain"],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ).stdout,
            b"",
        )
        fx_binary.relative_to(fx_source_root)

        sources = {"fx": fx_binary, "nanocodex": nanocodex_binary}
        with tempfile.TemporaryDirectory(
            prefix="paired-latency-actual-binaries-"
        ) as raw:
            runtime_root = Path(raw)
            provenance = {
                name: {
                    "implementation": name,
                    "binary": {
                        "path": str(path),
                        "sha256": bench.sha256_file(path),
                    },
                }
                for name, path in sources.items()
            }
            executables = {
                name: (
                    bench._copy_executable(item, runtime_root),
                    item["binary"]["sha256"],
                )
                for name, item in provenance.items()
            }
            ledger = bench.FreshUuid7Ledger()
            fingerprints, evidence = bench._offline_actual_binary_preflight(
                args=argparse.Namespace(nanocodex_commit=nanocodex_commit),
                runtime_root=runtime_root,
                executables=executables,
                fresh_identifier_ledger=ledger,
            )

        self.assertEqual(
            fingerprints,
            {
                "fx": "ae0e171655642b3d9e7f68162fb13f4030b4132bd3b5020ef6e9b126b19a3883",
                "nanocodex": "0f9ba75f32634eb08b59fa4691e6ba131cee1730672607efb3c0a6b0772d3106",
            },
        )
        self.assertTrue(evidence["passed"])
        self.assertFalse(evidence["provider_network_used"])
        self.assertEqual(evidence["scripted_loopback_requests"], 6)
        self.assertTrue(evidence["xml_metacharacter_private_paths_verified"])
        self.assertTrue(
            evidence["fresh_uuid7_identifiers_distinct_and_one_use_verified"]
        )
        self.assertEqual(
            ledger.evidence(),
            {
                "accepted_identity_uses": 16,
                "distinct_canonical_uuid7_identities": 16,
                "reuse_detected": False,
                "noncanonical_identity_detected": False,
                "distinct_and_one_use_verified": True,
            },
        )


class ScheduleAndCliTests(unittest.TestCase):
    def _argv(self) -> list[str]:
        return [
            "--fx-source-root",
            "/tmp/fx",
            "--fx-bin",
            "/tmp/fx/bin/fx",
            "--nanocodex-source-root",
            "/tmp/nano",
            "--nanocodex-bin",
            "/tmp/nano/bin/nano",
            "--nanocodex-commit",
            "a" * 40,
            "--auth-file",
            "/tmp/auth",
            "--output",
            "/tmp/report",
            "--confirm-paid-live-run",
        ]

    def test_safe_defaults_no_upstream_override_and_https_only_endpoint(self) -> None:
        args = bench.parse_args(self._argv())
        self.assertEqual(args.trials, 20)
        self.assertEqual(args.warmup_pairs, 2)
        self.assertFalse(hasattr(args, "upstream_url"))
        self.assertFalse(hasattr(args, "fx_commit"))
        self.assertFalse(hasattr(args, "cwd"))
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            bench.parse_args(self._argv() + ["--upstream-url", "https://evil.invalid"])

    def test_failed_benchmark_is_never_published(self) -> None:
        document = {
            "status": "failed",
            "schedule": {"measured_pairs_completed": 0},
            "summary": {"primary": {"eligible_pairs": 0}},
        }
        with (
            mock.patch.object(bench, "parse_args", return_value=argparse.Namespace()),
            mock.patch.object(
                bench,
                "run_benchmark",
                return_value=(document, 1, bench._fake_auth_material()),
            ),
            mock.patch.object(bench, "write_report") as write_report,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            exit_code = bench.main([])

        self.assertEqual(exit_code, 1)
        write_report.assert_not_called()

    def test_successful_publication_survives_stdout_failure(self) -> None:
        document = {
            "status": "passed",
            "schedule": {"measured_pairs_completed": 20},
            "summary": {"primary": {"eligible_pairs": 20}},
        }
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "report.json"
            args = argparse.Namespace(output=output)
            with (
                mock.patch.object(bench, "parse_args", return_value=args),
                mock.patch.object(
                    bench,
                    "run_benchmark",
                    return_value=(document, 0, bench._fake_auth_material()),
                ),
                mock.patch("builtins.print", side_effect=OSError("stdout closed")),
            ):
                exit_code = bench.main([])

            self.assertEqual(exit_code, 0)
            self.assertEqual(json.loads(output.read_bytes()), document)

    def test_successful_publication_survives_actually_closed_stdout(self) -> None:
        document = {
            "status": "passed",
            "schedule": {"measured_pairs_completed": 20},
            "summary": {"primary": {"eligible_pairs": 20}},
        }
        closed_stdout = io.StringIO()
        closed_stdout.close()
        with tempfile.TemporaryDirectory() as raw:
            output = Path(raw) / "report.json"
            args = argparse.Namespace(output=output)
            with (
                mock.patch.object(bench, "parse_args", return_value=args),
                mock.patch.object(
                    bench,
                    "run_benchmark",
                    return_value=(document, 0, bench._fake_auth_material()),
                ),
                mock.patch.object(bench.sys, "stdout", closed_stdout),
            ):
                exit_code = bench.main([])

            self.assertEqual(exit_code, 0)
            self.assertEqual(json.loads(output.read_bytes()), document)

    def test_trials_must_be_even_and_at_least_twenty(self) -> None:
        for trials in (0, 19, 21, 102):
            args = argparse.Namespace(
                confirm_paid_live_run=True, trials=trials, warmup_pairs=0
            )
            with self.subTest(trials=trials), self.assertRaisesRegex(
                bench.BenchmarkError, "even integer"
            ):
                bench._validate_and_resolve_args(args)

    def test_alternation_continues_from_warmup_into_measurement(self) -> None:
        warmup_pairs = 3
        self.assertEqual(bench._order(warmup_pairs), ["fx", "nanocodex"])
        self.assertEqual(
            bench._order(warmup_pairs + 1), ["nanocodex", "fx"]
        )

    def test_provider_request_budget_is_hard_bounded(self) -> None:
        auth = bench._fake_auth_material()
        state = bench.ObserverState(bench.AuthIdentityMatcher(auth), 1)
        state.reserve_provider_request()
        with self.assertRaisesRegex(
            bench.RequestValidationError, "provider_request_budget_exhausted"
        ):
            state.reserve_provider_request()

    def test_request_fingerprint_mismatch_fails_closed(self) -> None:
        auth = bench._fake_auth_material()
        state = bench.ObserverState(bench.AuthIdentityMatcher(auth), 2)
        state.observe_request_fingerprint("fx", "a" * 64)
        state.observe_request_fingerprint("fx", "a" * 64)
        with self.assertRaisesRegex(
            bench.RequestValidationError, "unstable_request_fingerprint"
        ):
            state.observe_request_fingerprint("fx", "b" * 64)
        self.assertFalse(state.fingerprints_stable())

    def test_report_preflight_happens_before_source_inspection(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            auth = root / "auth.json"
            auth.write_text("{}")
            output = root / "report.json"
            output.write_text("owned")
            args = argparse.Namespace(
                confirm_paid_live_run=True,
                trials=20,
                warmup_pairs=0,
                nanocodex_commit="a" * 40,
                auth_file=auth,
                output=output,
                fx_source_root=root,
                fx_bin=root / "fx",
                nanocodex_source_root=root,
                nanocodex_bin=root / "nano",
            )
            with mock.patch.object(bench, "_source_provenance") as source:
                with self.assertRaisesRegex(bench.BenchmarkError, "already exists"):
                    bench._validate_and_resolve_args(args)
                source.assert_not_called()


class ReportSecurityTests(unittest.TestCase):
    def _auth(self, secret: str) -> bench.AuthMaterial:
        return bench.AuthMaterial(
            raw_codex_auth=b"{}",
            access_token=secret,
            refresh_token="refresh-secret",
            account_id="account-secret",
            expires_at_ms=(int(time.time()) + 3_600) * 1_000,
            secret_values=(secret, "refresh-secret", "account-secret"),
        )

    def test_recursive_secret_scan_catches_keys_escapes_and_unicode_forms(self) -> None:
        secret = "tökén"
        auth = self._auth(secret)
        for credential in auth.secret_values:
            escaped = json.dumps(credential, ensure_ascii=True)[1:-1]
            decomposed = unicodedata.normalize("NFD", credential)
            for document in (
                {"nested": [{"value": escaped}]},
                {"nested": {decomposed: "value"}},
                {"nested": {"value": decomposed}},
            ):
                with self.subTest(document=document), self.assertRaisesRegex(
                    bench.BenchmarkError, "credential-bearing report"
                ):
                    bench._encoded_report(document, auth)
        for header_name in ("Authorization", "ChatGPT-Account-ID"):
            with self.subTest(header=header_name), self.assertRaisesRegex(
                bench.BenchmarkError, "raw auth headers"
            ):
                bench._encoded_report({header_name: "redacted"}, auth)
        self.assertEqual(
            json.loads(bench._encoded_report({"nested": ["benign"]}, auth)),
            {"nested": ["benign"]},
        )

    def test_atomic_report_publication_never_clobbers_existing_target(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            target = root / "report.json"
            target.write_bytes(b"existing")
            with self.assertRaises(FileExistsError):
                bench.write_report(target, b"replacement")
            self.assertEqual(target.read_bytes(), b"existing")
            self.assertEqual(list(root.glob(".report.json.*.tmp")), [])

            owned = root / "owned"
            owned.write_bytes(b"owned")
            symlink = root / "symlink-report.json"
            symlink.symlink_to(owned)
            with self.assertRaises(FileExistsError):
                bench.write_report(symlink, b"replacement")
            self.assertTrue(symlink.is_symlink())
            self.assertEqual(owned.read_bytes(), b"owned")
            self.assertEqual(list(root.glob(".symlink-report.json.*.tmp")), [])

            published = root / "published.json"
            bench.write_report(published, b"published")
            self.assertEqual(published.read_bytes(), b"published")
            self.assertEqual(stat.S_IMODE(published.stat().st_mode), 0o600)

    def test_report_fsync_failure_never_publishes(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            target = root / "report.json"
            with (
                mock.patch.object(bench.os, "fsync", side_effect=OSError("fsync")),
                self.assertRaisesRegex(OSError, "fsync"),
            ):
                bench.write_report(target, b"uncommitted")

            self.assertFalse(target.exists())
            self.assertEqual(list(root.glob(".report.json.*.tmp")), [])

    def test_temporary_unlink_failure_after_link_is_committed_success(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            target = root / "report.json"
            with mock.patch.object(
                bench.os, "unlink", side_effect=OSError("unlink denied")
            ):
                bench.write_report(target, b"committed")

            self.assertEqual(target.read_bytes(), b"committed")
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)


class ClientOutputTests(unittest.TestCase):
    def test_parses_minimized_fx_output(self) -> None:
        raw = _compact(
            {
                "output": bench.ACK_TOKEN,
                "exit_code": 0,
                "model": bench.MODEL,
                "session_id": "not-retained",
                "tool_calls": [],
            }
        )
        parsed = bench.parse_fx_client_output(
            bench.ProcessCapture(0, raw, stderr_bytes=0, timed_out=False)
        )
        self.assertTrue(parsed["valid"])
        self.assertTrue(parsed["client_configuration_echo_valid"])
        self.assertNotIn("output", parsed)
        self.assertNotIn("session_id", parsed)

    def test_parses_nanocodex_configuration_echo_but_not_as_source_proof(self) -> None:
        commit = "a" * 40
        cwd = Path("/private/ephemeral-workspace")
        parsed = bench.parse_nanocodex_client_output(
            bench.ProcessCapture(
                0,
                _compact(_nano_output(commit, cwd)),
                stderr_bytes=0,
                timed_out=False,
            ),
            source_commit=commit,
            cwd=cwd,
        )
        self.assertTrue(parsed["valid"])
        self.assertTrue(parsed["client_configuration_echo_valid"])
        self.assertNotIn("provenance_valid", parsed)

        websocket_generation = _nano_output(commit, cwd)
        websocket_generation["model_call"]["connection_generation"] = 1
        rejected = bench.parse_nanocodex_client_output(
            bench.ProcessCapture(
                0,
                _compact(websocket_generation),
                stderr_bytes=0,
                timed_out=False,
            ),
            source_commit=commit,
            cwd=cwd,
        )
        self.assertFalse(rejected["valid"])
        self.assertIn("nanocodex_model_call_invalid", rejected["errors"])


class StopPolicyTests(unittest.TestCase):
    class _State:
        def stats(self) -> dict[str, int]:
            return {
                "unexpected_requests": 0,
                "models_requests": 0,
                "refresh_attempts_blocked": 0,
                "provider_request_budget": 40,
                "provider_requests_forwarded": 1,
            }

        def request_fingerprints(self) -> dict[str, str]:
            return {"fx": "a" * 64, "nanocodex": "b" * 64}

        def fingerprints_stable(self) -> bool:
            return True

    class _Matcher:
        def verified(self) -> bool:
            return True

    class _Observer:
        base_url = "http://127.0.0.1:1"
        last_fresh_identifier_ledger: bench.FreshUuid7Ledger | None = None

        def __init__(self, *_args: object, **_kwargs: object) -> None:
            self.__class__.last_fresh_identifier_ledger = _kwargs.get(
                "fresh_identifier_ledger"
            )
            self.state = StopPolicyTests._State()
            self.auth_matcher = StopPolicyTests._Matcher()

        def __enter__(self) -> "StopPolicyTests._Observer":
            return self

        def __exit__(self, *_args: object) -> None:
            return None

    def test_first_measured_functional_failure_stops_all_later_clients(self) -> None:
        self._Observer.last_fresh_identifier_ledger = None
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            executable = root / "product"
            executable.write_bytes(b"product")
            executable.chmod(0o500)
            digest = bench.sha256_file(executable)
            provenance = lambda name: {
                "implementation": name,
                "source_root": str(root),
                "expected_commit": "a" * 40,
                "git_head": "a" * 40,
                "tree_clean": True,
                "binary": {
                    "path": str(executable),
                    "sha256": digest,
                    "byte_count": executable.stat().st_size,
                },
            }
            args = argparse.Namespace(
                auth_file=root / "auth",
                nanocodex_commit="a" * 40,
                trials=20,
                warmup_pairs=0,
                provider_request_budget=40,
            )
            failure = {
                "implementation": "fx",
                "functional_valid": False,
                "no_tool_calls": True,
                "timing_present": False,
                "cached_input_tokens": None,
                "measured_failure": True,
                "client": {"errors": ["failed"]},
                "observer": {"error": "failed", "timing_ns": {}},
            }
            with (
                mock.patch.object(
                    bench,
                    "_validate_and_resolve_args",
                    return_value=(provenance("fx"), provenance("nanocodex")),
                ),
                mock.patch.object(
                    bench, "load_auth_snapshot", return_value=bench._fake_auth_material()
                ),
                mock.patch.object(bench, "_copy_executable", return_value=executable),
                mock.patch.object(bench, "_assert_executable_copy"),
                mock.patch.object(bench, "_source_unchanged", return_value=True),
                mock.patch.object(
                    bench,
                    "_offline_actual_binary_preflight",
                    return_value=(
                        {"fx": "a" * 64, "nanocodex": "b" * 64},
                        {
                            "passed": True,
                            "provider_network_used": False,
                            "scripted_loopback_requests": 6,
                            "runs_per_implementation": 2,
                            "nanocodex_fedramp_runs": 2,
                            "nanocodex_fedramp_header_verified_from_id_token": True,
                            "contradictory_id_and_access_token_claims_verified": True,
                            "fx_exact_models_discovery_once_per_run_verified": True,
                            "fresh_uuid7_identifiers_distinct_and_one_use_verified": True,
                            "distinct_private_workspace_fingerprints_stable": True,
                            "exact_executable_request_templates_captured": True,
                            "independent_reference_body_fingerprints_verified": True,
                            "xml_metacharacter_private_paths_verified": True,
                        },
                    ),
                ) as preflight,
                mock.patch.object(bench, "StreamingObserver", self._Observer),
                mock.patch.object(
                    bench, "_run_implementation", return_value=failure
                ) as run,
            ):
                document, exit_code, _ = bench.run_benchmark(args)

        self.assertEqual(run.call_count, 1)
        self.assertIs(
            preflight.call_args.kwargs["fresh_identifier_ledger"],
            self._Observer.last_fresh_identifier_ledger,
        )
        self.assertEqual(document["schedule"]["measured_pairs_started"], 1)
        self.assertEqual(document["schedule"]["measured_pairs_completed"], 0)
        self.assertEqual(document["pairs"][0]["order"], ["fx", "nanocodex"])
        self.assertEqual(set(document["pairs"][0]["runs"]), {"fx"})
        self.assertEqual(
            document["comparison_design"]["kind"], "controlled_product_as_shipped"
        )
        self.assertFalse(
            document["comparison_design"]["identical_input_or_isolated_client_claim"]
        )
        self.assertEqual(document["provenance"]["transport"], "https")
        self.assertEqual(
            document["schedule"]["alternation_scope"],
            "continuous_across_warmup_and_measured_pairs",
        )
        self.assertTrue(
            document["exit_conditions"][
                "fresh_uuid7_identifiers_not_distinct_and_one_use"
            ]
        )
        self.assertFalse(
            document["provenance"]["actual_binary_offline_preflight"][
                "fresh_uuid7_identifiers_distinct_and_one_use_verified"
            ]
        )
        self.assertEqual(exit_code, 1)


if __name__ == "__main__":
    unittest.main()
