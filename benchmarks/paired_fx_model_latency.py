#!/usr/bin/env python3
"""Paid, explicitly opted-in fx/Nanocodex product-latency comparison.

The loopback observer validates each product's controlled, product-as-shipped
request before forwarding it to the one official upstream. Credentials and raw
headers are deliberately absent from every serializable result.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import dataclasses
import datetime as dt
import hashlib
import http.client
import http.server
import ipaddress
import json
import math
import os
import platform
import pwd
import re
import selectors
import shutil
import signal
import socket
import ssl
import stat
import subprocess
import socketserver
import sys
import tempfile
import threading
import time
import unicodedata
import urllib.parse
import uuid
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


MODEL = "gpt-5.6-sol"
EFFORT = "low"
ACK_TOKEN = "ACK_NANOCODEX_FX_MODEL_LATENCY_V1"
NANOCODEX_PROMPT_CACHE_KEY = "nanocodex-paired-fx-model-latency-v1"
SYSTEM_INSTRUCTIONS = (
    "This is an explicit paid model-latency benchmark. Return exactly the "
    "user-provided ACK token and nothing else. Do not call any tool."
)
NANOCODEX_PERMISSIONS_INSTRUCTIONS = (
    "<permissions instructions>\n"
    "Filesystem sandboxing defines which files can be read or written. `sandbox_mode` is "
    "`danger-full-access`: No filesystem sandboxing - all commands are permitted. Network "
    "access is enabled.\n"
    "Approval policy is currently never. Do not provide the `sandbox_permissions` for any "
    "reason, commands will be rejected.\n"
    "</permissions instructions>"
)
DEFAULT_UPSTREAM_URL = "https://chatgpt.com/backend-api/codex/responses"
FX_COMMIT = "4a8f765c94f4e205ecae293d6d5c98ec9aef2200"
FX_USER_AGENT = "fx/0.0.5"
NANOCODEX_USER_AGENT = "nanocodex/0.5.0"
FX_MODELS_CLIENT_VERSION = "0.148.0"
FX_TOOL_NAMES = (
    "read_file",
    "glob_files",
    "grep_files",
    "list_files",
    "file_info",
    "semantic_search",
    "edit_file",
    "write_file",
    "delete_file",
    "rename_file",
    "copy_file",
    "create_folder",
    "terminal",
    "skill",
    "install_skill",
    "mcp_search_tools",
    "mcp_select_tool",
    "mcp_features",
    "memory",
    "ask_user_question",
    "open_file",
    "web_fetch",
    "read_tool_result",
)
NANOCODEX_CODE_MODE_TOOL_NAMES = (
    "apply_patch",
    "exec_command",
    "image_gen__imagegen",
    "update_plan",
    "view_image",
    "web__run",
    "write_stdin",
)
# Independently captured from the complete recursively serialized declarations
# emitted by pinned fx ReleaseSafe and the reviewed Nanocodex reference binary.
# Arrays retain order; object keys are sorted before hashing.
FX_TOOL_DECLARATIONS_SHA256 = (
    "db7668aa7bca40ca33a29f20a84862451981c6cbce8e63e184ced3e839ef838e"
)
NANOCODEX_TOOL_DECLARATIONS_SHA256 = (
    "6bd9e3fa5ff2c7133df9896655a39dd5996aa5d1095f51e813e4cb9aceef213e"
)
FX_INSTRUCTIONS_TEMPLATE_SHA256 = (
    "b7b8d2cfd6e009aa15fe38c5aa04f347467f0b6f60790081a5530d5db5bdc5b4"
)
NANOCODEX_ENVIRONMENT_TEMPLATE_SHA256 = (
    "39fbdebee16a345383192ec568c8769e20b6f9d71695f6c169ffde166bc3b263"
)
NANOCODEX_TURN_METADATA_SHA256 = (
    "5c9c0f1c728d07f8b0aa8f3ac48677cd7603c1ab6296161298519b0c909849a1"
)
# Filled only from independently reviewed reference requests. Candidate
# preflight fingerprints remain a second live shape-switching guard.
REFERENCE_BODY_FINGERPRINTS: dict[str, str] = {
    "fx": "1b03392fc1a91cc8351fe24f5af0fb9fa49c627f229f302a4ecfb1f0d0239366",
    "nanocodex": "ffc48fa031415e2dcd56e464cc60246fad491545e9fd61399468fa90cd3551a9",
}
AUTH_MINIMUM_LIFETIME_SECONDS = 10 * 60
MAX_AUTH_BYTES = 1024 * 1024
MAX_REQUEST_BYTES = 512 * 1024
MAX_CLIENT_OUTPUT_BYTES = 1024 * 1024
MAX_CLIENT_STDERR_BYTES = 1024 * 1024
MAX_SSE_LINE_BYTES = 1024 * 1024
MAX_SSE_FRAME_BYTES = 1024 * 1024
CLIENT_TIMEOUT_SECONDS = 5 * 60
UPSTREAM_CONNECT_TIMEOUT_SECONDS = 4
UPSTREAM_RESOLVE_TIMEOUT_SECONDS = 4
OBSERVER_RESULT_TIMEOUT_SECONDS = CLIENT_TIMEOUT_SECONDS + 30
OBSERVER_SHUTDOWN_TIMEOUT_SECONDS = 5
PROCESS_SHUTDOWN_TIMEOUT_SECONDS = 5
MIN_TRIALS = 20
MIN_PRIMARY_ELIGIBLE_PAIRS = 20
MAX_TRIALS = 100
MAX_WARMUP_PAIRS = 10
MAX_PROVIDER_REQUESTS = 2 * (MAX_TRIALS + MAX_WARMUP_PAIRS)
TIE_THRESHOLD_NS = 1_000_000
XML_METACHAR_PREFLIGHT_DIRECTORY = "xml-path-&<>\"'"
COMMIT_RE = re.compile(r"^[0-9a-fA-F]{40}$")
CLIENT_MESSAGE_ID_RE = re.compile(
    r"^msg_[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)

MODELS_DOCUMENT = {
    "models": [
        {
            "slug": MODEL,
            "visibility": "list",
            "supported_in_api": True,
            "supported_reasoning_levels": [{"effort": EFFORT}],
            "additional_speed_tiers": [],
            "input_modalities": ["text"],
            "context_window": 272000,
        },
        {
            "slug": "gpt-5.4-mini",
            "visibility": "list",
            "supported_in_api": True,
            "supported_reasoning_levels": [{"effort": EFFORT}],
            "additional_speed_tiers": [],
            "input_modalities": ["text"],
            "context_window": 128000,
        },
    ]
}
MODELS_BYTES = json.dumps(MODELS_DOCUMENT, separators=(",", ":")).encode("utf-8")

RESPONSE_HEADER_NAMES = {
    "content-type",
    "cache-control",
    "retry-after",
    "x-request-id",
    "x-reasoning-included",
    "x-codex-turn-state",
}
PRODUCT_METADATA_HEADERS = {
    "user-agent",
    "originator",
    "openai-beta",
    "session-id",
    "thread-id",
    "x-client-request-id",
    "x-openai-internal-codex-responses-lite",
    "x-openai-fedramp",
    "x-codex-turn-state",
}
TRANSPORT_REQUEST_HEADERS = {"host"}
COMMON_GENERATION_REQUEST_HEADERS = {
    "authorization",
    "chatgpt-account-id",
    "content-type",
    "accept",
    "content-length",
}
FX_GENERATION_REQUEST_HEADERS = TRANSPORT_REQUEST_HEADERS | (
    COMMON_GENERATION_REQUEST_HEADERS
    | {"user-agent", "originator", "openai-beta", "connection"}
)
NANOCODEX_GENERATION_REQUEST_HEADERS = TRANSPORT_REQUEST_HEADERS | (
    COMMON_GENERATION_REQUEST_HEADERS
    | {
        "user-agent",
        "session-id",
        "thread-id",
        "x-client-request-id",
        "x-openai-internal-codex-responses-lite",
    }
)
FX_MODELS_REQUEST_HEADERS = TRANSPORT_REQUEST_HEADERS | {
    "authorization",
    "chatgpt-account-id",
    "user-agent",
    "originator",
    "accept",
    "connection",
}


class BenchmarkError(RuntimeError):
    """A safe-to-display benchmark configuration or integrity failure."""


class RequestValidationError(BenchmarkError):
    """A request failed its product-specific controlled-workload contract."""

    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


@dataclasses.dataclass(frozen=True, repr=False)
class AuthMaterial:
    raw_codex_auth: bytes = dataclasses.field(repr=False)
    access_token: str = dataclasses.field(repr=False)
    refresh_token: str = dataclasses.field(repr=False)
    account_id: str = dataclasses.field(repr=False)
    expires_at_ms: int
    secret_values: tuple[str, ...] = dataclasses.field(repr=False)
    fedramp: bool = False

    def __repr__(self) -> str:
        return f"AuthMaterial(expires_at_ms={self.expires_at_ms}, credentials=<redacted>)"

    def assert_live_window(self, now_seconds: float | None = None) -> None:
        now = time.time() if now_seconds is None else now_seconds
        if self.expires_at_ms / 1000 <= now + AUTH_MINIMUM_LIFETIME_SECONDS:
            raise BenchmarkError(
                "access JWT expires within the required 10-minute safety window"
            )

    def fx_document(self) -> dict[str, Any]:
        return {
            "version": 1,
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "expires_at_ms": self.expires_at_ms,
            "account_id": self.account_id,
        }

    def safe_metadata(self) -> dict[str, Any]:
        return {
            "mode": "shared_codex_chatgpt_snapshot",
            "minimum_remaining_lifetime_seconds": AUTH_MINIMUM_LIFETIME_SECONDS,
            "fedramp": self.fedramp,
        }


@dataclasses.dataclass(frozen=True, repr=False)
class _FakeAuthMaterial(AuthMaterial):
    """Unmistakably fake credentials accepted only by the private test seam."""


def _fake_auth_material(*, fedramp: bool = False) -> _FakeAuthMaterial:
    def encoded(value: Mapping[str, Any]) -> str:
        return base64.urlsafe_b64encode(
            json.dumps(value, separators=(",", ":")).encode("utf-8")
        ).decode("ascii").rstrip("=")

    account_id = "00000000-0000-4000-8000-000000000001"
    expires_at = 4_102_444_800  # 2100-01-01T00:00:00Z; unmistakably offline-only.
    auth_claims = {
        "chatgpt_account_id": account_id,
        "chatgpt_account_is_fedramp": fedramp,
    }

    def token(claims: Mapping[str, Any], kind: str) -> str:
        return ".".join(
            (
                encoded({"alg": "none", "typ": "JWT"}),
                encoded(
                    {
                        "exp": expires_at,
                        "https://api.openai.com/auth": claims,
                    }
                ),
                f"offline-{kind}-signature",
            )
        )

    # Deliberately contradictory claims prove that the product follows only
    # the ID token for FedRAMP policy instead of accidentally consulting the
    # bearer access token.
    access_claims = {
        "chatgpt_account_id": account_id,
        "chatgpt_account_is_fedramp": not fedramp,
    }
    access_token = token(access_claims, "access")
    id_token = token(auth_claims, "id")
    refresh_token = "fake-benchmark-refresh-token"
    raw_codex_auth = json.dumps(
        {
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
        },
        separators=(",", ":"),
    ).encode("utf-8")
    derived_fedramp = _jwt_fedramp(_decode_jwt_payload(id_token))
    return _FakeAuthMaterial(
        raw_codex_auth=raw_codex_auth,
        access_token=access_token,
        refresh_token=refresh_token,
        account_id=account_id,
        expires_at_ms=expires_at * 1_000,
        secret_values=(
            access_token,
            id_token,
            refresh_token,
            account_id,
        ),
        fedramp=derived_fedramp,
    )


@dataclasses.dataclass(frozen=True)
class ValidatedRequest:
    model: str
    reasoning_effort: str
    service_tier: str
    tool_count: int
    tool_names_sha256: str
    fingerprint_sha256: str
    session_id: str | None
    fresh_identifiers: tuple[str, ...]


@dataclasses.dataclass(frozen=True)
class ExpectedRequest:
    implementation: str
    pair: int
    phase: str
    instructions: str = SYSTEM_INSTRUCTIONS
    prompt: str = ACK_TOKEN
    home: str | None = None
    workspace: str | None = None


@dataclasses.dataclass
class ProcessCapture:
    returncode: int | None
    stdout: bytes
    stderr_bytes: int
    timed_out: bool
    stdout_truncated: bool = False
    stderr_truncated: bool = False
    output_limit_exceeded: bool = False


@dataclasses.dataclass(frozen=True, repr=False)
class CanonicalRequestHeaders:
    authorization: str = dataclasses.field(repr=False)
    account_id: str = dataclasses.field(repr=False)
    content_length: int
    session_id: str | None
    metadata: tuple[tuple[str, str], ...]


def _combined_request_fingerprint(
    validated: ValidatedRequest, headers: CanonicalRequestHeaders
) -> str:
    normalized_metadata = [
        [
            name.lower(),
            "<fresh-session>"
            if name.lower()
            in {"session-id", "thread-id", "x-client-request-id"}
            else value,
        ]
        for name, value in headers.metadata
        if name.lower() != "x-openai-fedramp"
    ]
    document = {
        "body_sha256": validated.fingerprint_sha256,
        "metadata_headers": normalized_metadata,
    }
    encoded = json.dumps(
        document, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def _required_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise BenchmarkError(f"auth snapshot is missing {label}")
    return value


def _decode_jwt_payload(token: str) -> Mapping[str, Any]:
    parts = token.split(".")
    if len(parts) != 3 or not parts[1]:
        raise BenchmarkError("access token is not a three-part JWT")
    payload = parts[1]
    payload += "=" * (-len(payload) % 4)
    try:
        decoded = base64.urlsafe_b64decode(payload.encode("ascii"))
        value = json.loads(decoded)
    except (ValueError, UnicodeError, json.JSONDecodeError) as error:
        raise BenchmarkError("access JWT payload is malformed") from error
    if not isinstance(value, dict):
        raise BenchmarkError("access JWT payload is not an object")
    return value


def _jwt_account_id(payload: Mapping[str, Any]) -> str | None:
    claim = payload.get("https://api.openai.com/auth")
    if isinstance(claim, dict):
        account = claim.get("chatgpt_account_id")
        if isinstance(account, str) and account:
            return account
    account = payload.get("chatgpt_account_id")
    if isinstance(account, str) and account:
        return account
    return None


def _jwt_fedramp(payload: Mapping[str, Any]) -> bool:
    claim = payload.get("https://api.openai.com/auth")
    if not isinstance(claim, dict):
        return False
    value = claim.get("chatgpt_account_is_fedramp")
    return value is True


def _collect_auth_secrets(document: Mapping[str, Any]) -> tuple[str, ...]:
    found: list[str] = []

    def visit(value: Any, key: str = "") -> None:
        if isinstance(value, dict):
            for child_key, child_value in value.items():
                visit(child_value, str(child_key).lower())
        elif isinstance(value, list):
            for child in value:
                visit(child, key)
        elif isinstance(value, str) and value and (
            "token" in key or "api_key" in key or key.endswith("key")
        ):
            found.append(value)

    visit(document)
    return tuple(dict.fromkeys(found))


def load_auth_snapshot(
    path: Path, *, now_seconds: float | None = None
) -> AuthMaterial:
    try:
        info = path.stat()
    except OSError as error:
        raise BenchmarkError("Codex auth snapshot is not readable") from error
    if not stat.S_ISREG(info.st_mode):
        raise BenchmarkError("Codex auth snapshot is not a regular file")
    if stat.S_IMODE(info.st_mode) & 0o077:
        raise BenchmarkError("Codex auth snapshot must not be group/world accessible")
    if info.st_size <= 0 or info.st_size > MAX_AUTH_BYTES:
        raise BenchmarkError("Codex auth snapshot has an invalid size")
    try:
        raw = path.read_bytes()
        document = json.loads(raw)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise BenchmarkError("Codex auth snapshot is malformed") from error
    if not isinstance(document, dict) or not isinstance(document.get("tokens"), dict):
        raise BenchmarkError("Codex auth snapshot does not contain ChatGPT tokens")
    tokens = document["tokens"]
    access_token = _required_string(tokens.get("access_token"), "tokens.access_token")
    refresh_token = _required_string(tokens.get("refresh_token"), "tokens.refresh_token")
    payload = _decode_jwt_payload(access_token)
    id_token = _required_string(tokens.get("id_token"), "tokens.id_token")
    id_payload = _decode_jwt_payload(id_token)
    expiry = payload.get("exp")
    if isinstance(expiry, bool) or not isinstance(expiry, int) or expiry <= 0:
        raise BenchmarkError("access JWT has no valid exp claim")
    token_account = _jwt_account_id(payload)
    stored_account = tokens.get("account_id")
    if stored_account is not None and (
        not isinstance(stored_account, str) or not stored_account
    ):
        raise BenchmarkError("Codex auth snapshot has an invalid account_id")
    account_id = stored_account or token_account
    if not account_id:
        raise BenchmarkError("Codex auth snapshot has no ChatGPT account identity")
    if token_account is not None and token_account != account_id:
        raise BenchmarkError("Codex auth snapshot account identity disagrees with its JWT")
    id_account = _jwt_account_id(id_payload)
    if id_account is not None and id_account != account_id:
        raise BenchmarkError("Codex auth snapshot account identity disagrees with its ID token")
    secret_values = tuple(
        dict.fromkeys((*_collect_auth_secrets(document), account_id))
    )
    material = AuthMaterial(
        raw_codex_auth=raw,
        access_token=access_token,
        refresh_token=refresh_token,
        account_id=account_id,
        expires_at_ms=expiry * 1000,
        secret_values=secret_values,
        fedramp=_jwt_fedramp(id_payload),
    )
    material.assert_live_window(now_seconds)
    return material


def _mkdir_private(path: Path) -> None:
    path.mkdir(mode=0o700, parents=True, exist_ok=False)
    os.chmod(path, 0o700)


def _write_private(path: Path, contents: bytes) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "wb", closefd=True) as handle:
            handle.write(contents)
            handle.flush()
        os.chmod(path, 0o600)
    except BaseException:
        with contextlib.suppress(OSError):
            os.close(descriptor)
        raise


def prepare_fx_home(home: Path, auth: AuthMaterial) -> Path:
    _mkdir_private(home)
    fx_dir = home / ".fx"
    _mkdir_private(fx_dir)
    auth_bytes = (
        json.dumps(auth.fx_document(), separators=(",", ":")) + "\n"
    ).encode("utf-8")
    settings = {
        "provider": "codex",
        "codex_model": MODEL,
        "effort": EFFORT,
        "fast_mode": False,
    }
    _write_private(fx_dir / "chatgpt-auth.json", auth_bytes)
    _write_private(
        fx_dir / "settings.json",
        (json.dumps(settings, separators=(",", ":")) + "\n").encode("utf-8"),
    )
    return fx_dir


def prepare_nanocodex_home(home: Path, auth: AuthMaterial) -> Path:
    _mkdir_private(home)
    codex_dir = home / ".codex"
    _mkdir_private(codex_dir)
    auth_path = codex_dir / "auth.json"
    _write_private(auth_path, auth.raw_codex_auth)
    return auth_path


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def fnv1a64(text: str) -> str:
    value = 0xCBF29CE484222325
    for byte in text.encode("utf-8"):
        value ^= byte
        value = (value * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def _canonical_json_sha256(value: Any) -> str:
    encoded = json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("ascii")
    return hashlib.sha256(encoded).hexdigest()


def _host_shell_name() -> str:
    configured = os.environ.get("SHELL")
    if not configured:
        configured = pwd.getpwuid(os.getuid()).pw_shell
    name = Path(configured).name
    if re.fullmatch(r"[A-Za-z0-9._+-]{1,128}", name) is None:
        raise BenchmarkError("host login shell cannot be represented safely")
    return name


def _host_timezone_name() -> str:
    zoneinfo_marker = "/zoneinfo/"
    resolved = os.path.realpath("/etc/localtime")
    if zoneinfo_marker in resolved:
        name = resolved.split(zoneinfo_marker, 1)[1]
    else:
        local = time.localtime()
        index = 1 if local.tm_isdst > 0 and len(time.tzname) > 1 else 0
        name = time.tzname[index]
    if (
        not name
        or len(name.encode("utf-8")) > 256
        or any(ord(character) < 0x20 or ord(character) > 0x7E for character in name)
    ):
        raise BenchmarkError("host timezone cannot be represented safely")
    return name


def _nanocodex_environment_text(
    workspace: str, *, shell: str, current_date: str, timezone: str
) -> str:
    workspace = _xml_escape_text(workspace)
    shell = _xml_escape_text(shell)
    current_date = _xml_escape_text(current_date)
    timezone = _xml_escape_text(timezone)
    return (
        "<environment_context>\n"
        f"  <cwd>{workspace}</cwd>\n"
        f"  <shell>{shell}</shell>\n"
        f"  <current_date>{current_date}</current_date>\n"
        f"  <timezone>{timezone}</timezone>\n"
        "  <filesystem><workspace_roots><root>"
        f"{workspace}"
        "</root></workspace_roots><permission_profile type=\"disabled\">"
        "<file_system type=\"unrestricted\" /></permission_profile></filesystem>\n"
        "</environment_context>"
    )


def _xml_escape_text(value: str) -> str:
    return "".join(
        {
            "&": "&amp;",
            "<": "&lt;",
            ">": "&gt;",
            '"': "&quot;",
            "'": "&apos;",
        }.get(character, character)
        for character in value
    )


def _expected_nanocodex_environment_text(workspace: str) -> str:
    return _nanocodex_environment_text(
        workspace,
        shell=_host_shell_name(),
        current_date=dt.datetime.now().astimezone().date().isoformat(),
        timezone=_host_timezone_name(),
    )


def _canonical_nanocodex_environment_text() -> str:
    return _nanocodex_environment_text(
        "<private-workspace>",
        shell="<host-shell>",
        current_date="<current-local-date>",
        timezone="<host-timezone>",
    )


def _normalized_fx_instructions(
    value: Any,
    *,
    instructions: str,
    home: str | None,
    workspace: str | None,
) -> str:
    if not isinstance(value, str):
        raise RequestValidationError("fx_runtime_context_invalid")
    start = "<fx-turn-context>\n"
    end = "\n</fx-turn-context>"
    if value.count(start) != 1 or value.count(end) != 1:
        raise RequestValidationError("fx_runtime_context_invalid")
    prefix, remainder = value.split(start, 1)
    context, suffix = remainder.split(end, 1)
    if not prefix.startswith(instructions + "\n\n"):
        raise RequestValidationError("benchmark_instructions_mismatch")
    lines = context.splitlines()
    if len(lines) != 7 or any(": " not in line for line in lines):
        raise RequestValidationError("fx_runtime_context_invalid")
    fields = dict(line.split(": ", 1) for line in lines)
    expected_keys = (
        "workspace_root",
        "current_directory",
        "operating_system",
        "shell_path",
        "date_utc",
        "home_directory",
        "git_worktree",
    )
    if tuple(fields) != expected_keys:
        raise RequestValidationError("fx_runtime_context_invalid")
    expected_workspace = workspace or fields["workspace_root"]
    expected_home = home or fields["home_directory"]
    expected_values = {
        "workspace_root": expected_workspace,
        "current_directory": expected_workspace,
        "operating_system": f"{platform.system()} {platform.release()}",
        "shell_path": "(unknown)",
        "date_utc": dt.datetime.now(dt.timezone.utc).date().isoformat(),
        "home_directory": expected_home,
        "git_worktree": "unknown",
    }
    if fields != expected_values:
        raise RequestValidationError("fx_runtime_context_invalid")
    normalized_context = "\n".join(
        (
            "workspace_root: <private-workspace>",
            "current_directory: <private-workspace>",
            "operating_system: <host-operating-system>",
            "shell_path: (unknown)",
            "date_utc: <current-utc-date>",
            "home_directory: <private-home>",
            "git_worktree: unknown",
        )
    )
    normalized = prefix + start + normalized_context + end + suffix
    if hashlib.sha256(normalized.encode("utf-8")).hexdigest() != (
        FX_INSTRUCTIONS_TEMPLATE_SHA256
    ):
        raise RequestValidationError("fx_instructions_contract_mismatch")
    return normalized


def _tool_declaration_name(tool: Any) -> str:
    if not isinstance(tool, dict):
        raise RequestValidationError("tool_declaration_not_object")
    kind = tool.get("type")
    if not isinstance(kind, str) or not kind:
        raise RequestValidationError("tool_declaration_missing_type")
    if kind == "function":
        name = tool.get("name")
        if not isinstance(name, str) or not name or not isinstance(tool.get("parameters"), dict):
            raise RequestValidationError("invalid_function_tool_declaration")
        return name
    if kind == "custom":
        name = tool.get("name")
        if not isinstance(name, str) or not name or not isinstance(tool.get("format"), dict):
            raise RequestValidationError("invalid_custom_tool_declaration")
        return name
    if kind == "namespace":
        name = tool.get("name")
        children = tool.get("tools")
        if not isinstance(name, str) or not name or not isinstance(children, list) or not children:
            raise RequestValidationError("invalid_namespace_tool_declaration")
        for child in children:
            _tool_declaration_name(child)
        return name
    if kind == "tool_search":
        if not isinstance(tool.get("execution"), str) or not tool["execution"]:
            raise RequestValidationError("invalid_tool_search_declaration")
        if not isinstance(tool.get("parameters"), dict):
            raise RequestValidationError("invalid_tool_search_declaration")
        return "tool_search"
    raise RequestValidationError("unsupported_tool_declaration_type")


def _extract_tools(body: Mapping[str, Any], implementation: str) -> list[Any]:
    if implementation == "fx":
        tools = body.get("tools")
        if not isinstance(tools, list) or not tools:
            raise RequestValidationError("fx_top_level_tools_missing")
        return tools
    if implementation != "nanocodex":
        raise RequestValidationError("unknown_implementation")
    request_input = body.get("input")
    if not isinstance(request_input, list) or len(request_input) != 5:
        raise RequestValidationError("nanocodex_input_missing")
    additional = request_input[0]
    if (
        not isinstance(additional, dict)
        or set(additional) != {"type", "role", "tools"}
        or additional.get("type") != "additional_tools"
        or additional.get("role") != "developer"
        or not isinstance(additional.get("tools"), list)
    ):
        raise RequestValidationError("nanocodex_additional_tools_missing")
    tools = additional["tools"]
    if not tools:
        raise RequestValidationError("nanocodex_additional_tools_empty")
    return tools


def _reject_duplicate_json_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, child in pairs:
        if key in value:
            raise RequestValidationError("duplicate_json_object_key")
        value[key] = child
    return value


def _exact_text_message(
    item: Any,
    *,
    role: str,
    text: str,
    typed: bool,
    client_id: bool = False,
) -> bool:
    expected_keys = {"role", "content"} | ({"type"} if typed else set())
    if client_id:
        expected_keys.add("id")
    if not isinstance(item, dict) or set(item) != expected_keys:
        return False
    if item.get("role") != role or (typed and item.get("type") != "message"):
        return False
    if client_id and not _valid_client_message_id(item.get("id")):
        return False
    return item.get("content") == [{"type": "input_text", "text": text}]


def _valid_client_message_id(value: Any) -> bool:
    return _client_message_uuid(value) is not None


def _client_message_uuid(value: Any) -> str | None:
    if not isinstance(value, str) or CLIENT_MESSAGE_ID_RE.fullmatch(value) is None:
        return None
    candidate = value.removeprefix("msg_")
    return candidate if _valid_uuid7(candidate) else None


def _valid_uuid7(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError):
        return False
    return parsed.version == 7 and str(parsed) == value


def _validate_nanocodex_environment_item(item: Any, workspace: str | None) -> str:
    if (
        not isinstance(item, dict)
        or set(item) != {"type", "id", "role", "content"}
        or item.get("type") != "message"
        or item.get("role") != "user"
        or not _valid_client_message_id(item.get("id"))
    ):
        raise RequestValidationError("nanocodex_environment_context_invalid")
    content = item.get("content")
    if (
        not isinstance(content, list)
        or len(content) != 1
        or not isinstance(content[0], dict)
        or set(content[0]) != {"type", "text"}
        or content[0].get("type") != "input_text"
        or not isinstance(content[0].get("text"), str)
    ):
        raise RequestValidationError("nanocodex_environment_context_invalid")
    raw_text = content[0]["text"]
    try:
        root = ET.fromstring(raw_text)
    except ET.ParseError as error:
        raise RequestValidationError("nanocodex_environment_context_invalid") from error
    if root.tag != "environment_context" or root.attrib:
        raise RequestValidationError("nanocodex_environment_context_invalid")
    children = list(root)
    if [child.tag for child in children] != [
        "cwd",
        "shell",
        "current_date",
        "timezone",
        "filesystem",
    ]:
        raise RequestValidationError("nanocodex_environment_context_invalid")
    cwd, shell, current_date, timezone, filesystem = children
    for child in (cwd, shell, current_date, timezone):
        if child.attrib or list(child) or not isinstance(child.text, str) or not child.text:
            raise RequestValidationError("nanocodex_environment_context_invalid")
    if workspace is not None and cwd.text != workspace:
        raise RequestValidationError("nanocodex_workspace_context_mismatch")
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", current_date.text or "") is None:
        raise RequestValidationError("nanocodex_environment_context_invalid")
    if filesystem.attrib or (filesystem.text or "").strip() or len(filesystem) != 2:
        raise RequestValidationError("nanocodex_environment_context_invalid")
    workspace_roots = filesystem[0]
    if workspace_roots.tag != "workspace_roots" or workspace_roots.attrib or len(workspace_roots) != 1:
        raise RequestValidationError("nanocodex_environment_context_invalid")
    workspace_root = workspace_roots[0]
    if (
        workspace_root.tag != "root"
        or workspace_root.attrib
        or list(workspace_root)
        or workspace_root.text != cwd.text
        or len(filesystem) != 2
    ):
        raise RequestValidationError("nanocodex_environment_context_invalid")
    permission_profile = filesystem[1]
    if (
        permission_profile.tag != "permission_profile"
        or permission_profile.attrib != {"type": "disabled"}
        or (permission_profile.text or "").strip()
        or len(permission_profile) != 1
    ):
        raise RequestValidationError("nanocodex_environment_context_invalid")
    file_system = permission_profile[0]
    if (
        file_system.tag != "file_system"
        or file_system.attrib != {"type": "unrestricted"}
        or list(file_system)
        or (file_system.text or "").strip()
    ):
        raise RequestValidationError("nanocodex_environment_context_invalid")
    if workspace is not None:
        if (
            shell.text != _host_shell_name()
            or current_date.text
            != dt.datetime.now().astimezone().date().isoformat()
            or timezone.text != _host_timezone_name()
            or raw_text != _expected_nanocodex_environment_text(workspace)
        ):
            raise RequestValidationError("nanocodex_environment_context_invalid")
    canonical = _canonical_nanocodex_environment_text()
    if hashlib.sha256(canonical.encode("utf-8")).hexdigest() != (
        NANOCODEX_ENVIRONMENT_TEMPLATE_SHA256
    ):
        raise RequestValidationError("nanocodex_environment_contract_mismatch")
    return canonical


def _validated_nanocodex_metadata(body: Mapping[str, Any]) -> dict[str, Any]:
    cache_key = body.get("prompt_cache_key")
    if cache_key != NANOCODEX_PROMPT_CACHE_KEY:
        raise RequestValidationError("nanocodex_prompt_cache_key_mismatch")
    metadata = body.get("client_metadata")
    if not isinstance(metadata, dict):
        raise RequestValidationError("nanocodex_client_metadata_invalid")
    if set(metadata) != {"session_id", "thread_id", "x-codex-turn-metadata"}:
        raise RequestValidationError("nanocodex_client_metadata_invalid")
    session_id = metadata.get("session_id")
    if (
        not _valid_uuid7(session_id)
        or metadata.get("thread_id") != session_id
    ):
        raise RequestValidationError("nanocodex_session_identity_invalid")
    normalized: dict[str, Any] = {
        "session_id": "<fresh-session>",
        "thread_id": "<fresh-session>",
    }
    turn_metadata = metadata["x-codex-turn-metadata"]
    if not isinstance(turn_metadata, str) or not turn_metadata:
        raise RequestValidationError("nanocodex_turn_metadata_invalid")
    try:
        decoded = json.loads(
            turn_metadata, object_pairs_hook=_reject_duplicate_json_pairs
        )
    except (UnicodeError, json.JSONDecodeError) as error:
        raise RequestValidationError("nanocodex_turn_metadata_invalid") from error
    expected_names = {
        name: {"name": name, "namespace": None}
        for name in NANOCODEX_CODE_MODE_TOOL_NAMES
    }
    if decoded != {"code_mode_tool_names": expected_names}:
        raise RequestValidationError("nanocodex_turn_metadata_invalid")
    if _canonical_json_sha256(decoded) != NANOCODEX_TURN_METADATA_SHA256:
        raise RequestValidationError("nanocodex_turn_metadata_contract_mismatch")
    normalized["x-codex-turn-metadata"] = decoded
    return normalized


def _stable_request_fingerprint(
    body: Mapping[str, Any],
    implementation: str,
    *,
    instructions: str = SYSTEM_INSTRUCTIONS,
    home: str | None,
    workspace: str | None,
) -> str:
    normalized = json.loads(json.dumps(body, separators=(",", ":")))
    if implementation == "nanocodex":
        normalized["client_metadata"] = _validated_nanocodex_metadata(body)
        for item in normalized["input"][2:]:
            item["id"] = "<fresh-client-item>"
        normalized["input"][3]["content"][0]["text"] = (
            _validate_nanocodex_environment_item(body["input"][3], workspace)
        )
    elif implementation == "fx":
        normalized["instructions"] = _normalized_fx_instructions(
            body.get("instructions"),
            instructions=instructions,
            home=home,
            workspace=workspace,
        )
    else:
        raise RequestValidationError("unknown_implementation")
    return _canonical_json_sha256(normalized)


def validate_request_body(
    raw_body: bytes,
    implementation: str,
    *,
    instructions: str = SYSTEM_INSTRUCTIONS,
    prompt: str = ACK_TOKEN,
    home: str | None = None,
    workspace: str | None = None,
) -> ValidatedRequest:
    if not raw_body or len(raw_body) > MAX_REQUEST_BYTES:
        raise RequestValidationError("invalid_request_size")
    try:
        body = json.loads(raw_body, object_pairs_hook=_reject_duplicate_json_pairs)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise RequestValidationError("request_body_not_json") from error
    if not isinstance(body, dict):
        raise RequestValidationError("request_body_not_object")
    if implementation not in {"fx", "nanocodex"}:
        raise RequestValidationError("unknown_implementation")
    if "previous_response_id" in body:
        raise RequestValidationError("unexpected_previous_response_id")
    if "service_tier" in body:
        raise RequestValidationError("service_tier_must_be_standard_omission")
    if "type" in body:
        raise RequestValidationError("https_request_type_must_be_absent")
    if body.get("store") is not False:
        raise RequestValidationError("store_must_be_false")
    if "background" in body:
        raise RequestValidationError("background_must_be_absent")
    if body.get("model") != MODEL:
        raise RequestValidationError("model_mismatch")
    if body.get("stream") is not True:
        raise RequestValidationError("stream_must_be_true")
    request_input = body.get("input")
    if not isinstance(request_input, list):
        raise RequestValidationError("request_input_missing")
    session_id: str | None = None
    fresh_identifiers: tuple[str, ...] = ()

    if implementation == "fx":
        required = {
            "model",
            "store",
            "stream",
            "instructions",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "include",
            "text",
            "reasoning",
        }
        if set(body) != required:
            raise RequestValidationError("fx_top_level_shape_invalid")
        _normalized_fx_instructions(
            body.get("instructions"),
            instructions=instructions,
            home=home,
            workspace=workspace,
        )
        if len(request_input) != 1 or not _exact_text_message(
            request_input[0], role="user", text=prompt, typed=False
        ):
            raise RequestValidationError("fresh_user_input_mismatch")
        if body.get("reasoning") != {"effort": EFFORT, "summary": "auto"}:
            raise RequestValidationError("reasoning_controls_mismatch")
        expected_parallel = True
        reasoning_effort = EFFORT
    else:
        required = {
            "model",
            "store",
            "stream",
            "input",
            "tool_choice",
            "parallel_tool_calls",
            "include",
            "prompt_cache_key",
            "text",
            "reasoning",
            "client_metadata",
        }
        if set(body) != required:
            raise RequestValidationError("nanocodex_top_level_shape_invalid")
        _extract_tools(body, implementation)
        if len(request_input) != 5:
            raise RequestValidationError("nanocodex_input_missing")
        if not _exact_text_message(
            request_input[1], role="developer", text=instructions, typed=True
        ):
            raise RequestValidationError("benchmark_instructions_mismatch")
        if not _exact_text_message(
            request_input[2],
            role="developer",
            text=NANOCODEX_PERMISSIONS_INSTRUCTIONS,
            typed=True,
            client_id=True,
        ):
            raise RequestValidationError("nanocodex_permissions_context_mismatch")
        _validate_nanocodex_environment_item(request_input[3], workspace)
        if not _exact_text_message(
            request_input[4],
            role="user",
            text=prompt,
            typed=True,
            client_id=True,
        ):
            raise RequestValidationError("fresh_user_input_mismatch")
        item_ids = tuple(request_input[index]["id"] for index in range(2, 5))
        item_uuids = tuple(_client_message_uuid(value) for value in item_ids)
        if any(value is None for value in item_uuids):
            raise RequestValidationError("nanocodex_client_item_id_invalid")
        if body.get("reasoning") != {
            "effort": EFFORT,
            "summary": "auto",
            "context": "all_turns",
        }:
            raise RequestValidationError("reasoning_controls_mismatch")
        _validated_nanocodex_metadata(body)
        session_id = body["client_metadata"]["session_id"]
        fresh_identifiers = (session_id, *(value for value in item_uuids if value))
        if len(set(fresh_identifiers)) != len(fresh_identifiers):
            raise RequestValidationError("nanocodex_request_identifiers_reused")
        expected_parallel = False
        reasoning_effort = EFFORT

    if body.get("tool_choice") != "auto":
        raise RequestValidationError("tool_choice_mismatch")
    if body.get("parallel_tool_calls") is not expected_parallel:
        raise RequestValidationError("parallel_tool_calls_mismatch")
    if body.get("include") != ["reasoning.encrypted_content"]:
        raise RequestValidationError("include_mismatch")
    if body.get("text") != {"verbosity": "low"}:
        raise RequestValidationError("text_controls_mismatch")

    tools = _extract_tools(body, implementation)
    names = [_tool_declaration_name(tool) for tool in tools]
    if len(set(names)) != len(names):
        raise RequestValidationError("duplicate_tool_declaration_name")
    tool_contract_hash = _canonical_json_sha256(tools)
    if implementation == "nanocodex":
        if names != ["exec", "wait"]:
            raise RequestValidationError("nanocodex_default_tool_surface_mismatch")
        if tool_contract_hash != NANOCODEX_TOOL_DECLARATIONS_SHA256:
            raise RequestValidationError("nanocodex_tool_contract_mismatch")
    else:
        if names != list(FX_TOOL_NAMES):
            raise RequestValidationError("fx_default_tool_surface_mismatch")
        if tool_contract_hash != FX_TOOL_DECLARATIONS_SHA256:
            raise RequestValidationError("fx_tool_contract_mismatch")
    names_bytes = json.dumps(sorted(names), separators=(",", ":")).encode("utf-8")
    fingerprint = _stable_request_fingerprint(
        body,
        implementation,
        instructions=instructions,
        home=home,
        workspace=workspace,
    )
    reference = REFERENCE_BODY_FINGERPRINTS[implementation]
    if reference and fingerprint != reference:
        raise RequestValidationError(f"{implementation}_request_contract_mismatch")
    return ValidatedRequest(
        model=MODEL,
        reasoning_effort=reasoning_effort,
        service_tier="standard",
        tool_count=len(tools),
        tool_names_sha256=hashlib.sha256(names_bytes).hexdigest(),
        fingerprint_sha256=fingerprint,
        session_id=session_id,
        fresh_identifiers=fresh_identifiers,
    )


def _nonempty_delta(event: Mapping[str, Any]) -> str | None:
    value = event.get("delta")
    if isinstance(value, str) and value:
        return value
    if isinstance(value, dict):
        for key in ("text", "content", "refusal"):
            child = value.get(key)
            if isinstance(child, str) and child:
                return child
    return None


def _cached_input_tokens(event: Mapping[str, Any]) -> int | None:
    response = event.get("response")
    if not isinstance(response, dict):
        return None
    usage = response.get("usage")
    if not isinstance(usage, dict):
        return None
    candidates: list[Any] = [usage.get("cached_input_tokens")]
    for key in ("input_tokens_details", "input_token_details"):
        details = usage.get(key)
        if isinstance(details, dict):
            candidates.append(details.get("cached_tokens"))
    observed: list[int] = []
    for value in candidates:
        if value is None:
            continue
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise ValueError("invalid cached token count")
        observed.append(value)
    if len(set(observed)) > 1:
        raise ValueError("conflicting cached token counts")
    return observed[0] if observed else None


TOOL_OUTPUT_TYPES = {
    "function_call",
    "custom_tool_call",
    "computer_call",
    "mcp_call",
    "shell_call",
    "local_shell_call",
    "web_search_call",
    "code_interpreter_call",
    "image_generation_call",
    "tool_search_call",
}
BENIGN_OUTPUT_ITEM_TYPES = {"message", "reasoning"}


def _event_indicates_tool_call(event_type: str, item_kind: Any) -> bool:
    if isinstance(item_kind, str):
        if item_kind in TOOL_OUTPUT_TYPES or item_kind.endswith("_call"):
            return True
        if event_type in {
            "response.output_item.added",
            "response.output_item.done",
        } and item_kind not in BENIGN_OUTPUT_ITEM_TYPES:
            return True
    return any(kind in event_type for kind in TOOL_OUTPUT_TYPES) or (
        "function_call_arguments" in event_type
        or "tool_call" in event_type
        or re.search(r"(?:^|\.)[^.]*_call(?:\.|$)", event_type) is not None
    )


class SSEObservation:
    """Incrementally extracts provider timestamps without retaining event data."""

    def __init__(self, t0_ns: int):
        self.t0_ns = t0_ns
        self.first_model_output_ns: int | None = None
        self.first_answer_text_ns: int | None = None
        self.terminal_ns: int | None = None
        self.terminal_type: str | None = None
        self.status = "streaming"
        self.cached_input_tokens: int | None = None
        self.tool_call_ids: set[str] = set()
        self.tool_call_without_id = False
        self.error: str | None = None
        self._data_lines: list[str] = []
        self._event_latest_ns: int | None = None

    def feed_line(self, raw_line: bytes, received_ns: int) -> None:
        try:
            line = raw_line.decode("utf-8").rstrip("\r\n")
        except UnicodeError:
            self.error = self.error or "malformed_sse_utf8"
            return
        if not line:
            self._dispatch()
            return
        if line.startswith(":"):
            return
        if line.startswith("data:"):
            value = line[5:]
            if value.startswith(" "):
                value = value[1:]
            self._event_latest_ns = received_ns
            self._data_lines.append(value)

    def finish(self) -> None:
        self._dispatch()
        if self.terminal_ns is None and self.error is None:
            self.error = "missing_terminal_event"
            self.status = "incomplete"

    def _dispatch(self) -> None:
        if not self._data_lines:
            self._event_latest_ns = None
            return
        data = "\n".join(self._data_lines)
        event_ns = (
            self._event_latest_ns
            if self._event_latest_ns is not None
            else self.t0_ns
        )
        self._data_lines.clear()
        self._event_latest_ns = None
        if data == "[DONE]":
            return
        try:
            event = json.loads(data)
        except json.JSONDecodeError:
            self.error = self.error or "malformed_sse_json"
            return
        if not isinstance(event, dict):
            self.error = self.error or "malformed_sse_event"
            return
        self._observe_event(event, event_ns)

    def _observe_event(self, event: Mapping[str, Any], event_ns: int) -> None:
        event_type = event.get("type")
        if not isinstance(event_type, str):
            return
        delta = _nonempty_delta(event)
        if delta is not None and event_type in {
            "response.output_text.delta",
            "response.refusal.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary.delta",
            "response.reasoning_text.delta",
        }:
            if self.first_model_output_ns is None:
                self.first_model_output_ns = event_ns
            if event_type == "response.output_text.delta" and self.first_answer_text_ns is None:
                self.first_answer_text_ns = event_ns

        item = event.get("item")
        item_kind = item.get("type") if isinstance(item, dict) else None
        tool_event = _event_indicates_tool_call(event_type, item_kind)
        if tool_event:
            identity: Any = event.get("call_id")
            if identity is None and isinstance(item, dict):
                identity = item.get("call_id") or item.get("id")
            if isinstance(identity, str) and identity:
                self.tool_call_ids.add(identity)
            else:
                self.tool_call_without_id = True

        if event_type in {
            "response.completed",
            "response.failed",
            "response.incomplete",
            "response.cancelled",
        }:
            if self.terminal_ns is not None:
                self.error = "duplicate_terminal_event"
                self.status = "failed"
                return
            self.terminal_ns = event_ns
            self.terminal_type = event_type
            response = event.get("response")
            response_status = response.get("status") if isinstance(response, dict) else None
            if isinstance(response_status, str) and response_status:
                self.status = response_status
            else:
                self.status = event_type.removeprefix("response.")
            try:
                self.cached_input_tokens = _cached_input_tokens(event)
            except ValueError:
                self.error = self.error or "invalid_cached_input_tokens"
            if isinstance(response, dict) and isinstance(response.get("output"), list):
                for output_item in response["output"]:
                    if isinstance(output_item, dict) and _event_indicates_tool_call(
                        "response.output_item.done", output_item.get("type")
                    ):
                        identity = output_item.get("call_id") or output_item.get("id")
                        if isinstance(identity, str) and identity:
                            self.tool_call_ids.add(identity)
                        else:
                            self.tool_call_without_id = True
            if event_type != "response.completed":
                self.error = self.error or "provider_terminal_failure"

    @staticmethod
    def _elapsed(timestamp: int | None, origin: int) -> int | None:
        return None if timestamp is None else max(0, timestamp - origin)

    def has_tool_call(self) -> bool:
        return bool(self.tool_call_ids) or self.tool_call_without_id

    def record(self) -> dict[str, Any]:
        tool_count = len(self.tool_call_ids) + int(self.tool_call_without_id)
        return {
            "t0_monotonic_ns": self.t0_ns,
            "first_model_output_monotonic_ns": self.first_model_output_ns,
            "first_answer_text_monotonic_ns": self.first_answer_text_ns,
            "terminal_monotonic_ns": self.terminal_ns,
            "timing_ns": {
                "time_to_first_model_output": self._elapsed(
                    self.first_model_output_ns, self.t0_ns
                ),
                "time_to_first_answer_text": self._elapsed(
                    self.first_answer_text_ns, self.t0_ns
                ),
                "time_to_terminal": self._elapsed(self.terminal_ns, self.t0_ns),
            },
            "terminal_event": self.terminal_type,
            "cached_input_tokens": self.cached_input_tokens,
            "tool_call_detected": tool_count > 0,
            "tool_call_count": tool_count,
            "status": self.status,
            "error": self.error,
        }


def observe_sse_lines(
    lines: Iterable[tuple[bytes, int]], *, t0_ns: int
) -> dict[str, Any]:
    observation = SSEObservation(t0_ns)
    for line, received_ns in lines:
        observation.feed_line(line, received_ns)
    observation.finish()
    return observation.record()


class AuthIdentityMatcher:
    """Retains only expected credential fingerprints in process memory."""

    def __init__(self, auth: AuthMaterial):
        self._bearer = hashlib.sha256(auth.access_token.encode("utf-8")).digest()
        self._account = hashlib.sha256(auth.account_id.encode("utf-8")).digest()
        self.fedramp = auth.fedramp
        self._seen: set[str] = set()
        self._mismatch = False
        self._lock = threading.Lock()

    def observe(self, implementation: str, authorization: str | None, account: str | None) -> None:
        if not isinstance(authorization, str) or not authorization.startswith("Bearer "):
            self._reject()
        token = authorization[7:] if authorization is not None else ""
        if (
            not token
            or token != token.strip()
            or not isinstance(account, str)
            or not account
            or account != account.strip()
        ):
            self._reject()
        bearer = hashlib.sha256(token.encode("utf-8")).digest()
        account_fingerprint = hashlib.sha256(account.encode("utf-8")).digest()
        with self._lock:
            if bearer != self._bearer or account_fingerprint != self._account:
                self._mismatch = True
                raise RequestValidationError("shared_auth_identity_mismatch")
            self._seen.add(implementation)

    def _reject(self) -> None:
        with self._lock:
            self._mismatch = True
        raise RequestValidationError("shared_auth_identity_missing")

    def verified(self) -> bool:
        return self.verified_for({"fx", "nanocodex"})

    def verified_for(self, implementations: set[str]) -> bool:
        with self._lock:
            return not self._mismatch and self._seen == implementations


@dataclasses.dataclass
class _Armed:
    expected: ExpectedRequest
    claimed: bool = False
    models_requests: int = 0
    result: dict[str, Any] | None = None


class FreshUuid7Ledger:
    """Run-scoped evidence that canonical UUIDv7 identities are used once."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._identities: set[str] = set()
        self._accepted_identity_uses = 0
        self._reuse_detected = False
        self._noncanonical_identity_detected = False

    def observe(self, identifiers: Sequence[str]) -> None:
        values = tuple(identifiers)
        if not values:
            return
        with self._lock:
            if not all(_valid_uuid7(value) for value in values):
                self._noncanonical_identity_detected = True
                raise RequestValidationError("fresh_request_identifier_invalid")
            if len(set(values)) != len(values) or any(
                value in self._identities for value in values
            ):
                self._reuse_detected = True
                raise RequestValidationError("fresh_request_identifier_reused")
            self._identities.update(values)
            self._accepted_identity_uses += len(values)

    def evidence(self) -> dict[str, int | bool]:
        with self._lock:
            distinct_identities = len(self._identities)
            verified = (
                distinct_identities > 0
                and self._accepted_identity_uses == distinct_identities
                and not self._reuse_detected
                and not self._noncanonical_identity_detected
            )
            return {
                "accepted_identity_uses": self._accepted_identity_uses,
                "distinct_canonical_uuid7_identities": distinct_identities,
                "reuse_detected": self._reuse_detected,
                "noncanonical_identity_detected": self._noncanonical_identity_detected,
                "distinct_and_one_use_verified": verified,
            }

    def distinct_and_one_use_verified(self) -> bool:
        return bool(self.evidence()["distinct_and_one_use_verified"])


class ObserverState:
    def __init__(
        self,
        auth_matcher: AuthIdentityMatcher,
        provider_request_budget: int,
        expected_fingerprints: Mapping[str, str] | None = None,
        *,
        fresh_identifier_ledger: FreshUuid7Ledger | None = None,
        require_fx_models_discovery: bool = False,
    ):
        if provider_request_budget <= 0 or provider_request_budget > MAX_PROVIDER_REQUESTS:
            raise BenchmarkError("provider request budget is outside the bounded range")
        self.auth_matcher = auth_matcher
        self.provider_request_budget = provider_request_budget
        self.require_fx_models_discovery = require_fx_models_discovery
        self._condition = threading.Condition()
        self._next_ticket = 1
        self._armed: dict[int, _Armed] = {}
        self._request_fingerprints: dict[str, str] = {}
        self._expected_fingerprints = dict(expected_fingerprints or {})
        if self._expected_fingerprints and set(self._expected_fingerprints) != {
            "fx",
            "nanocodex",
        }:
            raise BenchmarkError("offline request templates must cover both implementations")
        if any(
            re.fullmatch(r"[0-9a-f]{64}", fingerprint) is None
            for fingerprint in self._expected_fingerprints.values()
        ):
            raise BenchmarkError("offline request template fingerprint is invalid")
        self._fingerprint_mismatch = False
        self.fresh_identifier_ledger = (
            fresh_identifier_ledger
            if fresh_identifier_ledger is not None
            else FreshUuid7Ledger()
        )
        self.unexpected_requests = 0
        self.models_requests = 0
        self.refresh_attempts_blocked = 0
        self.provider_requests_forwarded = 0

    def arm(self, expected: ExpectedRequest) -> int:
        with self._condition:
            if any(entry.result is None for entry in self._armed.values()):
                raise BenchmarkError("observer already has an armed request")
            ticket = self._next_ticket
            self._next_ticket += 1
            self._armed[ticket] = _Armed(expected=expected)
            return ticket

    def claim(self) -> tuple[int, ExpectedRequest] | None:
        with self._condition:
            for ticket, armed in self._armed.items():
                if armed.result is None and not armed.claimed:
                    armed.claimed = True
                    return ticket, armed.expected
            self.unexpected_requests += 1
            return None

    def accept_fx_models_discovery(self) -> bool:
        with self._condition:
            for armed in self._armed.values():
                if (
                    armed.result is None
                    and not armed.claimed
                    and armed.expected.implementation == "fx"
                ):
                    if armed.models_requests != 0:
                        return False
                    armed.models_requests = 1
                    self.models_requests += 1
                    return True
            return False

    def fx_models_discovery_complete(self, ticket: int) -> bool:
        if not self.require_fx_models_discovery:
            return True
        with self._condition:
            armed = self._armed.get(ticket)
            return armed is not None and armed.models_requests == 1

    def complete(self, ticket: int, result: dict[str, Any]) -> None:
        with self._condition:
            armed = self._armed.get(ticket)
            if armed is not None and armed.result is None:
                armed.result = result
            self._condition.notify_all()

    def wait(self, ticket: int, timeout_seconds: float) -> dict[str, Any] | None:
        deadline = time.monotonic() + timeout_seconds
        with self._condition:
            while True:
                armed = self._armed.get(ticket)
                if armed is None:
                    return None
                if armed.result is not None:
                    result = armed.result
                    del self._armed[ticket]
                    return result
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    del self._armed[ticket]
                    return None
                self._condition.wait(remaining)

    def cancel_unclaimed(self, ticket: int) -> bool:
        with self._condition:
            armed = self._armed.get(ticket)
            if armed is not None and not armed.claimed:
                del self._armed[ticket]
                self._condition.notify_all()
                return True
            return False

    def count_refresh_block(self) -> None:
        with self._condition:
            self.refresh_attempts_blocked += 1

    def count_unexpected_request(self) -> None:
        with self._condition:
            self.unexpected_requests += 1

    def observe_request_fingerprint(
        self, implementation: str, fingerprint: str
    ) -> None:
        with self._condition:
            expected = self._expected_fingerprints.get(implementation)
            if expected is not None and fingerprint != expected:
                self._fingerprint_mismatch = True
                raise RequestValidationError("request_fingerprint_preflight_mismatch")
            previous = self._request_fingerprints.get(implementation)
            if previous is None:
                self._request_fingerprints[implementation] = fingerprint
            elif previous != fingerprint:
                self._fingerprint_mismatch = True
                raise RequestValidationError("unstable_request_fingerprint")

    def observe_fresh_identifiers(self, identifiers: Sequence[str]) -> None:
        self.fresh_identifier_ledger.observe(identifiers)

    def reserve_provider_request(self) -> None:
        with self._condition:
            if self.provider_requests_forwarded >= self.provider_request_budget:
                raise RequestValidationError("provider_request_budget_exhausted")
            self.provider_requests_forwarded += 1

    def request_fingerprints(self) -> dict[str, str]:
        with self._condition:
            return dict(sorted(self._request_fingerprints.items()))

    def fingerprints_stable(self) -> bool:
        with self._condition:
            fingerprints_stable = (
                not self._fingerprint_mismatch
                and set(self._request_fingerprints) == {"fx", "nanocodex"}
            )
        return (
            fingerprints_stable
            and self.fresh_identifier_ledger.distinct_and_one_use_verified()
        )

    def stats(self) -> dict[str, int]:
        with self._condition:
            return {
                "unexpected_requests": self.unexpected_requests,
                "models_requests": self.models_requests,
                "refresh_attempts_blocked": self.refresh_attempts_blocked,
                "provider_request_budget": self.provider_request_budget,
                "provider_requests_forwarded": self.provider_requests_forwarded,
            }


class _ResolvedSocketMixin:
    sock: socket.socket | None

    def _initialize_resolved_socket(
        self,
        endpoints: tuple[tuple[int, tuple[Any, ...]], ...],
        timeout: float,
    ) -> None:
        self._resolved_endpoints = endpoints
        self._connect_timeout = timeout
        self._connect_cancelled = threading.Event()

    def _connect_deadline(self) -> float:
        return time.monotonic() + self._connect_timeout

    def _remaining_connect_timeout(self, deadline: float) -> float:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("upstream connection deadline expired")
        return remaining

    def _raise_if_connect_cancelled(self) -> None:
        if self._connect_cancelled.is_set():
            raise OSError("upstream connection was cancelled")

    @staticmethod
    def _shutdown_and_close_sockets(*resources: socket.socket | None) -> None:
        seen: set[int] = set()
        for resource in resources:
            if resource is None or id(resource) in seen:
                continue
            seen.add(id(resource))
            with contextlib.suppress(OSError):
                resource.shutdown(socket.SHUT_RDWR)
            with contextlib.suppress(OSError):
                resource.close()

    def _connect_resolved_tcp(self, deadline: float) -> socket.socket:
        last_error: OSError | None = None
        for family, address in self._resolved_endpoints:
            self._raise_if_connect_cancelled()
            self._remaining_connect_timeout(deadline)
            candidate = socket.socket(family, socket.SOCK_STREAM, socket.IPPROTO_TCP)
            self.sock = candidate
            try:
                self._raise_if_connect_cancelled()
                candidate.settimeout(self._remaining_connect_timeout(deadline))
                candidate.connect(address)
                self._raise_if_connect_cancelled()
                return candidate
            except OSError as error:
                last_error = error
                self._shutdown_and_close_sockets(candidate)
                if self.sock is candidate:
                    self.sock = None
                if self._connect_cancelled.is_set():
                    raise OSError("upstream connection was cancelled") from error
        if last_error is None:
            raise OSError("upstream endpoint list was empty")
        raise last_error

    def abort_connect(self) -> None:
        self._connect_cancelled.set()
        self._shutdown_and_close_sockets(self.sock)


class _ResolvedHTTPConnection(_ResolvedSocketMixin, http.client.HTTPConnection):
    def __init__(
        self,
        host: str,
        port: int | None,
        endpoints: tuple[tuple[int, tuple[Any, ...]], ...],
        *,
        timeout: float,
    ) -> None:
        super().__init__(host, port, timeout=timeout)
        self._initialize_resolved_socket(endpoints, timeout)

    def connect(self) -> None:
        if self._tunnel_host is not None:
            raise OSError("upstream HTTP tunneling is disabled")
        self._connect_resolved_tcp(self._connect_deadline())


class _ResolvedHTTPSConnection(_ResolvedSocketMixin, http.client.HTTPSConnection):
    def __init__(
        self,
        host: str,
        port: int | None,
        endpoints: tuple[tuple[int, tuple[Any, ...]], ...],
        *,
        timeout: float,
        context: ssl.SSLContext,
    ) -> None:
        super().__init__(host, port, timeout=timeout, context=context)
        self._initialize_resolved_socket(endpoints, timeout)

    def connect(self) -> None:
        if self._tunnel_host is not None:
            raise OSError("upstream HTTPS tunneling is disabled")
        deadline = self._connect_deadline()
        raw_socket = self._connect_resolved_tcp(deadline)
        try:
            tls_socket = self._context.wrap_socket(
                raw_socket,
                server_hostname=self.host,
                do_handshake_on_connect=False,
            )
            self.sock = tls_socket
            self._raise_if_connect_cancelled()
            tls_socket.settimeout(self._remaining_connect_timeout(deadline))
            tls_socket.do_handshake()
        except BaseException:
            current = self.sock
            self._shutdown_and_close_sockets(current, raw_socket)
            if self.sock is current:
                self.sock = None
            raise


@dataclasses.dataclass(frozen=True)
class UpstreamTarget:
    scheme: str
    host: str
    port: int | None
    path: str
    endpoints: tuple[tuple[int, tuple[Any, ...]], ...] = ()

    @classmethod
    def official(cls) -> "UpstreamTarget":
        target = cls._parse(DEFAULT_UPSTREAM_URL, allow_loopback_http=False)
        if target != cls("https", "chatgpt.com", None, "/backend-api/codex/responses"):
            raise BenchmarkError("official upstream endpoint invariant was violated")
        return dataclasses.replace(
            target,
            endpoints=_resolve_upstream_endpoints(target.host, target.port or 443),
        )

    @classmethod
    def _for_test(cls, value: str) -> "UpstreamTarget":
        target = cls._parse(value, allow_loopback_http=True)
        if target.host not in {"127.0.0.1", "localhost", "::1"}:
            raise BenchmarkError("test upstream must resolve to an explicit loopback host")
        return dataclasses.replace(
            target,
            endpoints=_literal_loopback_endpoints(
                target.host,
                target.port or (443 if target.scheme == "https" else 80),
            ),
        )

    @classmethod
    def _parse(
        cls, value: str, *, allow_loopback_http: bool
    ) -> "UpstreamTarget":
        parsed = urllib.parse.urlsplit(value)
        if parsed.scheme not in {"http", "https"} or not parsed.hostname:
            raise BenchmarkError("upstream URL must be an absolute HTTP(S) URL")
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise BenchmarkError(
                "upstream URL must not contain credentials, a query, or a fragment"
            )
        if parsed.scheme == "http" and (
            not allow_loopback_http
            or parsed.hostname not in {"127.0.0.1", "localhost", "::1"}
        ):
            raise BenchmarkError("plain-HTTP upstreams are test-only loopback targets")
        try:
            port = parsed.port
        except ValueError as error:
            raise BenchmarkError("upstream URL has an invalid port") from error
        path = parsed.path or "/"
        return cls(parsed.scheme, parsed.hostname, port, path)

    def connection(self) -> http.client.HTTPConnection:
        if not self.endpoints:
            raise BenchmarkError("upstream endpoint was not resolved before use")
        if self.scheme == "https":
            return _ResolvedHTTPSConnection(
                self.host,
                self.port,
                self.endpoints,
                timeout=UPSTREAM_CONNECT_TIMEOUT_SECONDS,
                context=ssl.create_default_context(),
            )
        return _ResolvedHTTPConnection(
            self.host,
            self.port,
            self.endpoints,
            timeout=UPSTREAM_CONNECT_TIMEOUT_SECONDS,
        )


def _empty_observer_record(
    expected: ExpectedRequest, *, request_bytes: bytes | None, error: str
) -> dict[str, Any]:
    request = {
        "byte_count": None,
        "sha256": None,
        "model": None,
        "reasoning_effort": None,
        "service_tier": None,
        "tool_count": None,
        "tool_names_sha256": None,
        "stable_fingerprint_sha256": None,
    }
    if request_bytes is not None:
        request["byte_count"] = len(request_bytes)
        request["sha256"] = hashlib.sha256(request_bytes).hexdigest()
    return {
        "implementation": expected.implementation,
        "pair": expected.pair,
        "phase": expected.phase,
        "request": request,
        "upstream_http_status": None,
        "t0_monotonic_ns": None,
        "first_model_output_monotonic_ns": None,
        "first_answer_text_monotonic_ns": None,
        "terminal_monotonic_ns": None,
        "timing_ns": {
            "time_to_first_model_output": None,
            "time_to_first_answer_text": None,
            "time_to_terminal": None,
        },
        "terminal_event": None,
        "cached_input_tokens": None,
        "tool_call_detected": False,
        "tool_call_count": 0,
        "status": "failed",
        "error": error,
    }


class _TrackedThreadingHTTPServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = False
    allow_reuse_address = False

    def __init__(
        self,
        address: tuple[str, int],
        handler: type[http.server.BaseHTTPRequestHandler],
    ) -> None:
        self._lifecycle = threading.Condition()
        self._closing = False
        self._active_requests: set[socket.socket] = set()
        super().__init__(address, handler)

    def process_request(
        self, request: socket.socket, client_address: tuple[str, int]
    ) -> None:
        with self._lifecycle:
            if self._closing:
                with contextlib.suppress(OSError):
                    request.shutdown(socket.SHUT_RDWR)
                request.close()
                return
            self._active_requests.add(request)
        try:
            super().process_request(request, client_address)
        except BaseException:
            with self._lifecycle:
                self._active_requests.discard(request)
                self._lifecycle.notify_all()
            with contextlib.suppress(OSError):
                request.shutdown(socket.SHUT_RDWR)
            request.close()
            raise

    def process_request_thread(
        self, request: socket.socket, client_address: tuple[str, int]
    ) -> None:
        try:
            super().process_request_thread(request, client_address)
        finally:
            with self._lifecycle:
                self._active_requests.discard(request)
                self._lifecycle.notify_all()

    def begin_shutdown(self) -> None:
        with self._lifecycle:
            self._closing = True

    def interrupt_active_work(self) -> None:
        with self._lifecycle:
            requests = tuple(self._active_requests)
        for request in requests:
            with contextlib.suppress(OSError):
                request.shutdown(socket.SHUT_RDWR)

    def _active_work_locked(self) -> int:
        return len(self._active_requests)

    def wait_for_quiescence(self, deadline: float) -> int:
        with self._lifecycle:
            while self._active_work_locked() > 0:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                self._lifecycle.wait(remaining)
            return self._active_work_locked()

    def active_work_count(self) -> int:
        with self._lifecycle:
            return self._active_work_locked()

    def server_bind(self) -> None:
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = host
        self.server_port = port


class _ObserverServer(_TrackedThreadingHTTPServer):

    def __init__(
        self,
        address: tuple[str, int],
        state: ObserverState,
        upstream: UpstreamTarget,
    ):
        self.state = state
        self.upstream = upstream
        self._active_upstreams: dict[
            http.client.HTTPConnection, list[socket.socket]
        ] = {}
        super().__init__(address, _ObserverHandler)

    def register_upstream(self, connection: http.client.HTTPConnection) -> None:
        upstream_socket = connection.sock
        with self._lifecycle:
            if self._closing:
                should_abort = True
            else:
                self._active_upstreams[connection] = (
                    [] if upstream_socket is None else [upstream_socket]
                )
                should_abort = False
        if should_abort:
            self._interrupt_upstream_resources(
                ((connection, (upstream_socket,), upstream_socket),)
            )
            raise OSError("observer is shutting down")

    def connected_upstream(self, connection: http.client.HTTPConnection) -> None:
        upstream_socket = connection.sock
        if upstream_socket is None:
            raise OSError("upstream connection did not expose a socket")
        with self._lifecycle:
            if connection not in self._active_upstreams:
                raise OSError("upstream connection lease is missing")
            registered = self._active_upstreams[connection]
            if self._closing:
                retained_sockets = tuple(registered)
                should_abort = True
            else:
                if all(resource is not upstream_socket for resource in registered):
                    registered.append(upstream_socket)
                retained_sockets = ()
                should_abort = False
        if should_abort:
            self._interrupt_upstream_resources(
                ((connection, (*retained_sockets, upstream_socket), upstream_socket),)
            )
            raise OSError("observer is shutting down")

    def unregister_upstream(self, connection: http.client.HTTPConnection) -> None:
        with self._lifecycle:
            self._active_upstreams.pop(connection, None)
            self._lifecycle.notify_all()

    @staticmethod
    def _interrupt_upstream_resources(
        upstreams: Sequence[
            tuple[
                http.client.HTTPConnection,
                Sequence[socket.socket],
                socket.socket | None,
            ]
        ],
    ) -> None:
        resources: list[socket.socket] = []
        for connection, registered_sockets, current_socket in upstreams:
            resources.extend(registered_sockets)
            if current_socket is not None:
                resources.append(current_socket)
            abort = getattr(connection, "abort_connect", None)
            if callable(abort):
                with contextlib.suppress(OSError):
                    abort()

        seen: set[int] = set()
        for upstream in resources:
            if id(upstream) in seen:
                continue
            seen.add(id(upstream))
            with contextlib.suppress(OSError):
                upstream.shutdown(socket.SHUT_RDWR)
            with contextlib.suppress(OSError):
                upstream.close()

    def interrupt_active_work(self) -> None:
        with self._lifecycle:
            upstreams = tuple(
                (connection, tuple(registered_sockets), connection.sock)
                for connection, registered_sockets in self._active_upstreams.items()
            )
        super().interrupt_active_work()
        self._interrupt_upstream_resources(upstreams)

    def _active_work_locked(self) -> int:
        return super()._active_work_locked() + len(self._active_upstreams)


class _ObserverHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "nanocodex-latency-observer/1"

    @property
    def observer_server(self) -> _ObserverServer:
        return self.server  # type: ignore[return-value]

    def log_message(self, _format: str, *_args: Any) -> None:
        return

    def handle_one_request(self) -> None:
        # BaseHTTPRequestHandler rejects an oversized request line before it
        # calls parse_request. Reset a per-request guard here so every nonempty
        # parser or dispatch rejection poisons eligibility exactly once.
        self._unexpected_request_counted = False
        super().handle_one_request()

    def _count_unexpected_request_once(self) -> None:
        if self._unexpected_request_counted:
            return
        self._unexpected_request_counted = True
        self.observer_server.state.count_unexpected_request()

    def _raw_request_target(self) -> str:
        words = self.requestline.split()
        if len(words) != 3:
            raise RequestValidationError("request_target_unavailable")
        return words[1]

    def parse_request(self) -> bool:
        if not super().parse_request():
            # BaseHTTPRequestHandler has already emitted its parser error, but
            # every nonempty inbound attempt must still poison eligibility.
            self._count_unexpected_request_once()
            self.close_connection = True
            return False
        defects = tuple(getattr(self.headers, "defects", ()))
        if self.request_version != "HTTP/1.1" or defects:
            self._count_unexpected_request_once()
            self.close_connection = True
            with contextlib.suppress(OSError):
                self._fixed_response(400, b"malformed request\n", "text/plain")
            return False
        try:
            local_address = ipaddress.ip_address(self.connection.getsockname()[0])
        except (ValueError, OSError, IndexError):
            local_address = None
        if local_address != ipaddress.ip_address("127.0.0.1"):
            self._count_unexpected_request_once()
            self._fixed_response(404, b"not found\n", "text/plain")
            return False
        return True

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        try:
            if (
                self._raw_request_target()
                != f"/models?client_version={FX_MODELS_CLIENT_VERSION}"
            ):
                raise RequestValidationError("fx_models_request_target_invalid")
            self._require_exact_header_set(FX_MODELS_REQUEST_HEADERS)
            if self._single_header("Host") != self._observer_host_header():
                raise RequestValidationError("observer_host_header_invalid")
            if self._single_header("Connection") != "keep-alive":
                raise RequestValidationError("fx_models_connection_header_invalid")
            authorization = self._single_header("Authorization")
            account_id = self._single_header("ChatGPT-Account-ID")
            if self._single_header("User-Agent") != FX_USER_AGENT:
                raise RequestValidationError("fx_models_user_agent_invalid")
            if self._single_header("Originator") != "fx":
                raise RequestValidationError("fx_models_originator_invalid")
            if self._single_header("Accept") != "application/json":
                raise RequestValidationError("fx_models_accept_invalid")
            self.observer_server.state.auth_matcher.observe(
                "fx", authorization, account_id
            )
            if not self.observer_server.state.accept_fx_models_discovery():
                raise RequestValidationError("fx_models_request_unexpected")
        except RequestValidationError:
            self._count_unexpected_request_once()
            self._fixed_response(404, b"not found\n", "text/plain")
            return
        self._fixed_response(200, MODELS_BYTES, "application/json")

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        try:
            request_target = self._raw_request_target()
        except RequestValidationError:
            self._discard_request_body()
            self._count_unexpected_request_once()
            self._fixed_response(404, b"not found\n", "text/plain")
            return
        if request_target == "/forbid-refresh":
            self._discard_request_body()
            self.observer_server.state.count_refresh_block()
            self._fixed_response(409, b"credential refresh disabled\n", "text/plain")
            return
        if request_target != "/responses":
            self._discard_request_body()
            self._count_unexpected_request_once()
            self._fixed_response(404, b"not found\n", "text/plain")
            return
        claim = self.observer_server.state.claim()
        if claim is None:
            self._discard_request_body()
            self._fixed_response(409, b"unexpected request\n", "text/plain")
            return
        ticket, expected = claim
        body: bytes | None = None
        try:
            if (
                expected.implementation == "fx"
                and not self.observer_server.state.fx_models_discovery_complete(ticket)
            ):
                raise RequestValidationError("fx_models_discovery_missing")
            canonical_headers = self._canonical_request_headers(
                expected.implementation
            )
            body = self._read_request_body(canonical_headers.content_length)
            validated = validate_request_body(
                body,
                expected.implementation,
                instructions=expected.instructions,
                prompt=expected.prompt,
                home=expected.home,
                workspace=expected.workspace,
            )
            self.observer_server.state.auth_matcher.observe(
                expected.implementation,
                canonical_headers.authorization,
                canonical_headers.account_id,
            )
            if (
                expected.implementation == "nanocodex"
                and validated.session_id != canonical_headers.session_id
            ):
                raise RequestValidationError("nanocodex_body_header_session_mismatch")
            self.observer_server.state.observe_fresh_identifiers(
                validated.fresh_identifiers
            )
            stable_fingerprint = _combined_request_fingerprint(
                validated, canonical_headers
            )
            self.observer_server.state.observe_request_fingerprint(
                expected.implementation, stable_fingerprint
            )
            self.observer_server.state.reserve_provider_request()
        except RequestValidationError as error:
            record = _empty_observer_record(
                expected, request_bytes=body, error=error.code
            )
            self.observer_server.state.complete(ticket, record)
            self._fixed_response(400, b"benchmark request rejected\n", "text/plain")
            return
        except BenchmarkError as error:
            record = _empty_observer_record(
                expected, request_bytes=body, error=str(error)
            )
            self.observer_server.state.complete(ticket, record)
            self._fixed_response(400, b"benchmark request rejected\n", "text/plain")
            return

        request_record = {
            "byte_count": len(body),
            "sha256": hashlib.sha256(body).hexdigest(),
            "model": validated.model,
            "reasoning_effort": validated.reasoning_effort,
            "service_tier": validated.service_tier,
            "tool_count": validated.tool_count,
            "tool_names_sha256": validated.tool_names_sha256,
            "stable_fingerprint_sha256": stable_fingerprint,
        }
        record = self._forward(
            expected, body, request_record, canonical_headers
        )
        self.observer_server.state.complete(ticket, record)

    def do_CONNECT(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        self._reject_unsupported_method()

    do_PUT = do_CONNECT
    do_PATCH = do_CONNECT
    do_DELETE = do_CONNECT
    do_HEAD = do_CONNECT
    do_OPTIONS = do_CONNECT
    do_TRACE = do_CONNECT

    def send_error(
        self,
        code: int,
        message: str | None = None,
        explain: str | None = None,
    ) -> None:
        self._count_unexpected_request_once()
        self.close_connection = True
        if code == 501:
            self._reject_unsupported_method()
            return
        super().send_error(code, message, explain)

    def _reject_unsupported_method(self) -> None:
        # Account before touching attacker-controlled body bytes. A truncated
        # body or TCP reset must never hide the extra request from eligibility.
        self._count_unexpected_request_once()
        self.close_connection = True
        self._discard_request_body()
        with contextlib.suppress(OSError):
            self._fixed_response(405, b"method not allowed\n", "text/plain")

    def _single_header(self, name: str) -> str:
        values = self.headers.get_all(name, [])
        if len(values) != 1:
            raise RequestValidationError(
                f"{name.lower().replace('-', '_')}_header_count_invalid"
            )
        value = values[0]
        if not isinstance(value, str) or not value:
            raise RequestValidationError(
                f"{name.lower().replace('-', '_')}_header_invalid"
            )
        return value

    def _require_exact_header_set(self, expected: set[str]) -> None:
        supplied = {name.lower() for name in self.headers.keys()}
        if supplied != expected:
            raise RequestValidationError("request_header_set_invalid")
        for name in expected:
            if len(self.headers.get_all(name, [])) != 1:
                raise RequestValidationError(
                    f"{name.lower().replace('-', '_')}_header_count_invalid"
                )

    def _observer_host_header(self) -> str:
        return f"127.0.0.1:{self.observer_server.server_port}"

    def _optional_single_header(self, name: str) -> str | None:
        values = self.headers.get_all(name, [])
        if len(values) > 1:
            raise RequestValidationError(
                f"{name.lower().replace('-', '_')}_header_count_invalid"
            )
        return values[0] if values else None

    @staticmethod
    def _metadata_value(name: str, value: str | None) -> str:
        if (
            not isinstance(value, str)
            or not value
            or value != value.strip()
            or len(value.encode("utf-8")) > 1024
            or any(
                ord(character) < 0x20 or ord(character) > 0x7E
                for character in value
            )
        ):
            raise RequestValidationError(
                f"{name.lower().replace('-', '_')}_header_invalid"
            )
        return value

    def _canonical_request_headers(
        self, implementation: str
    ) -> CanonicalRequestHeaders:
        allowed_headers = (
            FX_GENERATION_REQUEST_HEADERS
            if implementation == "fx"
            else NANOCODEX_GENERATION_REQUEST_HEADERS
        )
        if implementation == "nanocodex" and self.observer_server.state.auth_matcher.fedramp:
            allowed_headers = allowed_headers | {"x-openai-fedramp"}
        self._require_exact_header_set(allowed_headers)
        if self._single_header("Host") != self._observer_host_header():
            raise RequestValidationError("observer_host_header_invalid")
        if implementation == "fx" and self._single_header("Connection") != "close":
            raise RequestValidationError("fx_connection_header_invalid")
        authorization = self._single_header("Authorization")
        account_id = self._single_header("ChatGPT-Account-ID")
        raw_length = self._single_header("Content-Length")
        content_type = self._single_header("Content-Type")
        accept = self._single_header("Accept")
        if authorization != authorization.strip() or account_id != account_id.strip():
            raise RequestValidationError("credential_header_whitespace_invalid")
        if content_type != "application/json":
            raise RequestValidationError("content_type_must_be_application_json")
        if accept != "text/event-stream":
            raise RequestValidationError("accept_must_be_text_event_stream")
        if not raw_length.isascii() or not raw_length.isdecimal():
            raise RequestValidationError("invalid_content_length")
        length = int(raw_length)
        if str(length) != raw_length or length <= 0 or length > MAX_REQUEST_BYTES:
            raise RequestValidationError("invalid_request_size")

        supplied = {
            name: self._optional_single_header(name)
            for name in PRODUCT_METADATA_HEADERS
        }
        if supplied["x-codex-turn-state"] is not None:
            raise RequestValidationError("fresh_turn_state_header_forbidden")
        user_agent = self._metadata_value("user-agent", supplied["user-agent"])

        if implementation == "fx":
            if user_agent != FX_USER_AGENT:
                raise RequestValidationError("fx_user_agent_invalid")
            if supplied["originator"] != "fx":
                raise RequestValidationError("fx_originator_invalid")
            if supplied["openai-beta"] != "responses=experimental":
                raise RequestValidationError("fx_openai_beta_invalid")
            for forbidden in (
                "session-id",
                "thread-id",
                "x-client-request-id",
                "x-openai-internal-codex-responses-lite",
                "x-openai-fedramp",
            ):
                if supplied[forbidden] is not None:
                    raise RequestValidationError("fx_metadata_header_set_invalid")
            session_id = None
            metadata = (
                ("User-Agent", user_agent),
                ("Originator", "fx"),
                ("OpenAI-Beta", "responses=experimental"),
            )
        elif implementation == "nanocodex":
            if user_agent != NANOCODEX_USER_AGENT:
                raise RequestValidationError("nanocodex_user_agent_invalid")
            if supplied["originator"] is not None or supplied["openai-beta"] is not None:
                raise RequestValidationError("nanocodex_metadata_header_set_invalid")
            if supplied["x-openai-internal-codex-responses-lite"] != "true":
                raise RequestValidationError("nanocodex_responses_lite_header_invalid")
            session_id = self._metadata_value("session-id", supplied["session-id"])
            request_id = self._metadata_value(
                "x-client-request-id", supplied["x-client-request-id"]
            )
            if request_id != session_id:
                raise RequestValidationError("request_session_identity_mismatch")
            thread_id = self._metadata_value("thread-id", supplied["thread-id"])
            if thread_id != session_id:
                raise RequestValidationError("request_session_identity_mismatch")
            fedramp = supplied["x-openai-fedramp"]
            expected_fedramp = self.observer_server.state.auth_matcher.fedramp
            if expected_fedramp and fedramp != "true":
                raise RequestValidationError("nanocodex_fedramp_header_missing")
            if not expected_fedramp and fedramp is not None:
                raise RequestValidationError("nanocodex_fedramp_header_unexpected")
            metadata_items = [
                ("User-Agent", user_agent),
                ("X-OpenAI-Internal-Codex-Responses-Lite", "true"),
                ("Session-ID", session_id),
                ("Thread-ID", thread_id),
                ("X-Client-Request-ID", request_id),
            ]
            if expected_fedramp:
                metadata_items.append(("X-OpenAI-Fedramp", "true"))
            metadata = tuple(metadata_items)
        else:
            raise RequestValidationError("unknown_implementation")
        return CanonicalRequestHeaders(
            authorization, account_id, length, session_id, metadata
        )

    def _read_request_body(self, length: int) -> bytes:
        body = self.rfile.read(length)
        if len(body) != length:
            raise RequestValidationError("truncated_request_body")
        return body

    def _discard_request_body(self) -> None:
        headers = getattr(self, "headers", None)
        if headers is None:
            return
        try:
            remaining = int(headers.get("Content-Length", "0"))
        except (TypeError, ValueError):
            remaining = 0
        remaining = min(max(remaining, 0), MAX_REQUEST_BYTES)
        while remaining:
            try:
                chunk = self.rfile.read(min(remaining, 64 * 1024))
            except OSError:
                return
            if not chunk:
                break
            remaining -= len(chunk)

    def _forward(
        self,
        expected: ExpectedRequest,
        body: bytes,
        request_record: dict[str, Any],
        headers: CanonicalRequestHeaders,
    ) -> dict[str, Any]:
        connection: http.client.HTTPConnection | None = None
        upstream: http.client.HTTPResponse | None = None
        upstream_registered = False
        response_started = False
        upstream_status: int | None = None
        observation: SSEObservation | None = None
        transport_error: str | None = None
        try:
            connection = self.observer_server.upstream.connection()
            self.observer_server.register_upstream(connection)
            upstream_registered = True
            connection.connect()
            self.observer_server.connected_upstream(connection)
            if connection.sock is None:
                raise OSError("upstream connection did not expose a socket")
            connection.sock.settimeout(CLIENT_TIMEOUT_SECONDS)
            connection.putrequest(
                "POST",
                self.observer_server.upstream.path,
                skip_accept_encoding=True,
            )
            connection.putheader("Authorization", headers.authorization)
            connection.putheader("ChatGPT-Account-ID", headers.account_id)
            connection.putheader("Content-Type", "application/json")
            connection.putheader("Accept", "text/event-stream")
            for name, value in headers.metadata:
                connection.putheader(name, value)
            connection.putheader("Content-Length", str(len(body)))
            connection.putheader("Connection", "close")
            connection.endheaders(body)
            t0_ns = time.monotonic_ns()
            observation = SSEObservation(t0_ns)
            upstream = connection.getresponse()
            upstream_status = upstream.status
            hide_authentication_failure = upstream.status == 401

            if hide_authentication_failure:
                self._fixed_response(
                    502, b"upstream authentication failed\n", "text/plain"
                )
                response_started = True
            else:
                self.send_response_only(upstream.status)
                for name, value in upstream.getheaders():
                    if name.lower() in RESPONSE_HEADER_NAMES:
                        self.send_header(name, value)
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                response_started = True

            frame: list[bytes] = []
            frame_bytes = 0
            while True:
                line = upstream.readline(MAX_SSE_LINE_BYTES + 1)
                if not line:
                    break
                if len(line) > MAX_SSE_LINE_BYTES:
                    transport_error = "upstream_sse_line_too_large"
                    break
                frame.append(line)
                frame_bytes += len(line)
                if frame_bytes > MAX_SSE_FRAME_BYTES:
                    transport_error = "upstream_sse_frame_too_large"
                    break
                received_ns = time.monotonic_ns()
                observation.feed_line(line, received_ns)
                if line.rstrip(b"\r\n"):
                    continue
                if observation.error == "duplicate_terminal_event":
                    transport_error = "duplicate_terminal_event"
                    break
                if observation.has_tool_call():
                    transport_error = "tool_call_blocked_before_delivery"
                    break
                if not hide_authentication_failure:
                    try:
                        self.wfile.write(b"".join(frame))
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError, OSError):
                        transport_error = "client_disconnected"
                        break
                frame.clear()
                frame_bytes = 0
            observation.finish()
            if transport_error is None and frame:
                if observation.has_tool_call():
                    transport_error = "tool_call_blocked_before_delivery"
                elif not hide_authentication_failure:
                    try:
                        self.wfile.write(b"".join(frame))
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError, OSError):
                        transport_error = "client_disconnected"
            if not 200 <= upstream.status < 300:
                observation.status = "http_error"
                observation.error = f"upstream_http_{upstream.status}"
            if transport_error is not None:
                observation.status = "failed"
                observation.error = transport_error
        except (TimeoutError, ssl.SSLError, OSError, http.client.HTTPException) as error:
            if isinstance(error, TimeoutError):
                transport_error = "upstream_timeout"
            elif isinstance(error, ssl.SSLError):
                transport_error = "upstream_tls_error"
            else:
                transport_error = "upstream_transport_error"
            if not response_started:
                with contextlib.suppress(OSError):
                    self._fixed_response(502, b"upstream unavailable\n", "text/plain")
        finally:
            if upstream is not None:
                with contextlib.suppress(OSError, http.client.HTTPException):
                    upstream.close()
            if connection is not None:
                try:
                    connection.close()
                finally:
                    if upstream_registered:
                        self.observer_server.unregister_upstream(connection)

        if observation is None:
            record = _empty_observer_record(
                expected, request_bytes=body, error=transport_error or "upstream_failure"
            )
        else:
            record = {
                "implementation": expected.implementation,
                "pair": expected.pair,
                "phase": expected.phase,
                "request": request_record,
                "upstream_http_status": upstream_status,
                **observation.record(),
            }
        return record

    def _fixed_response(self, status_code: int, body: bytes, content_type: str) -> None:
        self.send_response_only(status_code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        self.wfile.write(body)
        self.wfile.flush()


def _close_tracked_server(
    server: _TrackedThreadingHTTPServer,
    thread: threading.Thread,
    label: str,
) -> None:
    deadline = time.monotonic() + OBSERVER_SHUTDOWN_TIMEOUT_SECONDS
    failures: list[BaseException] = []
    for operation in (
        server.begin_shutdown,
        server.interrupt_active_work,
        server.shutdown,
        server.server_close,
    ):
        try:
            operation()
        except BaseException as error:
            failures.append(error)
    try:
        remaining_work = server.wait_for_quiescence(deadline)
    except BaseException as error:
        failures.append(error)
        remaining_work = server.active_work_count()
    try:
        thread.join(max(0.0, deadline - time.monotonic()))
    except BaseException as error:
        failures.append(error)
    thread_alive = thread.is_alive()
    listener = getattr(server, "socket", None)
    listener_open = listener is not None and listener.fileno() >= 0
    if failures or remaining_work != 0 or thread_alive or listener_open:
        raise BenchmarkError(
            f"{label} shutdown did not quiesce "
            f"(active_work={remaining_work}, serve_thread_alive={thread_alive}, "
            f"listener_open={listener_open})"
        ) from (failures[0] if failures else None)


def _start_tracked_server(
    server: _TrackedThreadingHTTPServer,
    thread: threading.Thread,
    label: str,
) -> None:
    try:
        thread.start()
        return
    except BaseException as start_error:
        if thread.is_alive():
            try:
                _close_tracked_server(server, thread, label)
            except BaseException as cleanup_error:
                raise BenchmarkError(
                    f"{label} failed to start and cleanup could not be verified"
                ) from cleanup_error
        else:
            failures: list[BaseException] = []
            for operation in (
                server.begin_shutdown,
                server.interrupt_active_work,
                server.server_close,
            ):
                try:
                    operation()
                except BaseException as error:
                    failures.append(error)
            if failures or server.socket.fileno() >= 0 or server.active_work_count() != 0:
                raise BenchmarkError(
                    f"{label} failed to start and listener cleanup failed"
                ) from (failures[0] if failures else start_error)
        raise BenchmarkError(f"{label} serve thread failed to start") from start_error


class StreamingObserver:
    def __init__(
        self,
        auth: AuthMaterial,
        provider_request_budget: int,
        *,
        expected_fingerprints: Mapping[str, str],
        fresh_identifier_ledger: FreshUuid7Ledger,
    ):
        self._initialize(
            auth,
            UpstreamTarget.official(),
            provider_request_budget,
            expected_fingerprints,
            fresh_identifier_ledger,
            require_fx_models_discovery=True,
        )

    @classmethod
    def _for_test(
        cls,
        auth: _FakeAuthMaterial,
        upstream_url: str,
        *,
        provider_request_budget: int,
        expected_fingerprints: Mapping[str, str] | None = None,
        fresh_identifier_ledger: FreshUuid7Ledger | None = None,
        require_fx_models_discovery: bool = False,
    ) -> "StreamingObserver":
        expected_fake = _fake_auth_material(fedramp=auth.fedramp)
        if (
            not isinstance(auth, _FakeAuthMaterial)
            or auth.raw_codex_auth != expected_fake.raw_codex_auth
            or auth.access_token != expected_fake.access_token
            or auth.refresh_token != expected_fake.refresh_token
            or auth.account_id != expected_fake.account_id
        ):
            raise BenchmarkError("custom upstreams require fake test credentials")
        observer = cls.__new__(cls)
        observer._initialize(
            auth,
            UpstreamTarget._for_test(upstream_url),
            provider_request_budget,
            expected_fingerprints,
            fresh_identifier_ledger,
            require_fx_models_discovery=require_fx_models_discovery,
        )
        return observer

    def _initialize(
        self,
        auth: AuthMaterial,
        upstream: UpstreamTarget,
        provider_request_budget: int,
        expected_fingerprints: Mapping[str, str] | None,
        fresh_identifier_ledger: FreshUuid7Ledger | None,
        *,
        require_fx_models_discovery: bool,
    ) -> None:
        self.auth_matcher = AuthIdentityMatcher(auth)
        self.state = ObserverState(
            self.auth_matcher,
            provider_request_budget,
            expected_fingerprints,
            fresh_identifier_ledger=fresh_identifier_ledger,
            require_fx_models_discovery=require_fx_models_discovery,
        )
        self.server = _ObserverServer(
            ("0.0.0.0", 0), self.state, upstream
        )
        try:
            self._thread = threading.Thread(
                target=self.server.serve_forever,
                name="paired-latency-observer",
                daemon=True,
            )
        except BaseException as error:
            self.server.server_close()
            raise BenchmarkError("streaming observer thread construction failed") from error

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}"

    def __enter__(self) -> "StreamingObserver":
        _start_tracked_server(self.server, self._thread, "streaming observer")
        return self

    def __exit__(self, *_exc: Any) -> None:
        _close_tracked_server(self.server, self._thread, "streaming observer")


def _offline_sse_response() -> bytes:
    message = {
        "id": "msg_offline_preflight",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": ACK_TOKEN}],
    }
    events = (
        {
            "type": "response.created",
            "response": {"id": "resp_offline_preflight", "status": "in_progress"},
        },
        {
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": message["id"],
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": [],
            },
        },
        {
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": ACK_TOKEN,
        },
        {
            "type": "response.output_item.done",
            "output_index": 0,
            "item": message,
        },
        {
            "type": "response.completed",
            "response": {
                "id": "resp_offline_preflight",
                "status": "completed",
                "output": [message],
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": {"cached_tokens": 1},
                    "output_tokens": 2,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": 12,
                },
            },
        },
    )
    return b"".join(
        b"data: "
        + json.dumps(event, separators=(",", ":")).encode("utf-8")
        + b"\n\n"
        for event in events
    )


OFFLINE_SSE_BYTES = _offline_sse_response()


class _OfflineProviderHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    @property
    def offline_server(self) -> "_OfflineProviderServer":
        return self.server  # type: ignore[return-value]

    def log_message(self, _format: str, *_args: Any) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler contract
        parsed = urllib.parse.urlsplit(self.path)
        if parsed.path != "/responses" or parsed.query or parsed.fragment:
            self.send_error(404)
            return
        try:
            length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            self.send_error(400)
            return
        if length <= 0 or length > MAX_REQUEST_BYTES or len(self.rfile.read(length)) != length:
            self.send_error(400)
            return
        with self.offline_server.lock:
            self.offline_server.requests += 1
        if self.offline_server.stall_after_headers:
            self.send_response_only(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.flush()
            self.offline_server.request_started.set()
            try:
                self.rfile.read(1)
            finally:
                self.offline_server.peer_disconnected.set()
            return
        self.send_response_only(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(OFFLINE_SSE_BYTES)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        self.wfile.write(OFFLINE_SSE_BYTES)
        self.wfile.flush()


class _OfflineProviderServer(_TrackedThreadingHTTPServer):
    def __init__(self, *, stall_after_headers: bool = False) -> None:
        self.lock = threading.Lock()
        self.requests = 0
        self.stall_after_headers = stall_after_headers
        self.request_started = threading.Event()
        self.peer_disconnected = threading.Event()
        super().__init__(("127.0.0.1", 0), _OfflineProviderHandler)


class _OfflineProvider:
    def __init__(self, *, stall_after_headers: bool = False) -> None:
        self.server = _OfflineProviderServer(
            stall_after_headers=stall_after_headers
        )
        try:
            self._thread = threading.Thread(
                target=self.server.serve_forever,
                name="paired-latency-offline-provider",
                daemon=True,
            )
        except BaseException as error:
            self.server.server_close()
            raise BenchmarkError("offline provider thread construction failed") from error

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}/responses"

    def __enter__(self) -> "_OfflineProvider":
        _start_tracked_server(self.server, self._thread, "offline provider")
        return self

    def __exit__(self, *_exc: Any) -> None:
        _close_tracked_server(self.server, self._thread, "offline provider")


SANDBOX_EXEC = Path("/usr/bin/sandbox-exec")


def _require_child_egress_sandbox() -> None:
    if sys.platform != "darwin":
        raise BenchmarkError(
            "live paired benchmarking requires the Darwin child egress sandbox"
        )
    try:
        info = SANDBOX_EXEC.stat()
    except OSError as error:
        raise BenchmarkError("Darwin child egress sandbox is unavailable") from error
    if not stat.S_ISREG(info.st_mode) or not os.access(SANDBOX_EXEC, os.X_OK):
        raise BenchmarkError("Darwin child egress sandbox is unavailable")


def _observer_port(base_url: str) -> int:
    parsed = urllib.parse.urlsplit(base_url)
    try:
        host = ipaddress.ip_address(parsed.hostname or "")
        port = parsed.port
    except ValueError as error:
        raise BenchmarkError("observer URL is not an exact numeric loopback endpoint") from error
    if (
        parsed.scheme != "http"
        or host != ipaddress.ip_address("127.0.0.1")
        or port is None
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
    ):
        raise BenchmarkError("observer URL is not an exact numeric loopback endpoint")
    return port


def _sandboxed_child_command(command: Sequence[str], observer_base_url: str) -> list[str]:
    _require_child_egress_sandbox()
    if not command or any(not isinstance(part, str) or not part for part in command):
        raise BenchmarkError("child command is empty or malformed")
    port = _observer_port(observer_base_url)
    profile = "\n".join(
        (
            "(version 1)",
            "(allow default)",
            "(deny network*)",
            "(deny process-fork)",
            # Seatbelt accepts only its reserved `localhost` token here. The
            # AF_INET + TCP conjunction prevents its token from also admitting
            # ::1 or UDP; the product endpoint remains numeric 127.0.0.1.
            "(allow network-outbound "
            f'(require-all (socket-domain AF_INET) (remote tcp "localhost:{port}")))',
        )
    )
    return [str(SANDBOX_EXEC), "-p", profile, *command]


def _child_environment(home: Path) -> dict[str, str]:
    path_value = os.environ.get("PATH") or os.defpath
    if "\x00" in path_value:
        raise BenchmarkError("PATH contains an invalid NUL byte")
    directories = {
        "XDG_CONFIG_HOME": home / ".config",
        "XDG_CACHE_HOME": home / ".cache",
        "XDG_DATA_HOME": home / ".local" / "share",
        "TMPDIR": home / ".tmp",
    }
    for directory in directories.values():
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(directory, 0o700)
    return {
        "PATH": path_value,
        "HOME": str(home),
        **{key: str(value) for key, value in directories.items()},
        "NO_COLOR": "1",
    }


def _reap_process_once(
    process: subprocess.Popen[bytes], *, deadline: float, retry_seconds: float
) -> tuple[bool, BaseException | None, bool]:
    """Return (reaped, last error, verified impossible) for one wait cycle."""
    if process.returncode is not None:
        return True, None, False

    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return False, None, False

    wait_error: BaseException | None = None
    try:
        returncode = process.wait(timeout=min(retry_seconds, remaining))
    except (subprocess.TimeoutExpired, OSError) as error:
        wait_error = error
    else:
        if process.returncode is None:
            process.returncode = returncode
        return True, None, False

    if process.returncode is not None:
        return True, None, False
    if os.name != "posix":
        return False, wait_error, False

    try:
        child, status = os.waitpid(process.pid, os.WNOHANG)
    except ChildProcessError as error:
        if process.returncode is not None:
            return True, None, False
        return False, error, True
    except OSError as error:
        return False, error, False
    if child == process.pid:
        process.returncode = os.waitstatus_to_exitcode(status)
        return True, None, False
    return False, wait_error, False


def _kill_process_group(
    process: subprocess.Popen[bytes], *, deadline: float
) -> None:
    retry_seconds = 0.01
    group_absent = os.name != "posix"
    leader_reaped = process.returncode is not None
    reap_impossible: BaseException | None = None
    last_group_error: BaseException | None = None
    last_reap_error: BaseException | None = None

    while time.monotonic() < deadline:
        group_signal_failed = False
        if os.name == "posix" and not group_absent:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                group_absent = True
            except OSError as error:
                group_signal_failed = True
                last_group_error = error

        leader_reaped = process.returncode is not None
        if not leader_reaped and (group_signal_failed or group_absent):
            try:
                process.kill()
            except ProcessLookupError:
                pass
            except OSError as error:
                last_group_error = error
            leader_reaped = process.returncode is not None

        if not leader_reaped and reap_impossible is None:
            leader_reaped, reap_error, impossible = _reap_process_once(
                process, deadline=deadline, retry_seconds=retry_seconds
            )
            if reap_error is not None:
                last_reap_error = reap_error
            if impossible:
                reap_impossible = reap_error

        if os.name == "posix" and not group_absent:
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                group_absent = True
            except OSError as error:
                last_group_error = error

        if leader_reaped and group_absent:
            return
        if group_absent and reap_impossible is not None:
            raise BenchmarkError("failed to reap the child process") from reap_impossible

        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(retry_seconds, remaining))

    if not group_absent:
        raise BenchmarkError(
            "child process group remained alive after termination"
        ) from last_group_error
    if not leader_reaped:
        raise BenchmarkError("failed to reap the child process") from last_reap_error


def _run_process(
    command: Sequence[str],
    cwd: Path,
    environment: Mapping[str, str],
    *,
    timeout_seconds: float = CLIENT_TIMEOUT_SECONDS,
    stdout_limit: int = MAX_CLIENT_OUTPUT_BYTES,
    stderr_limit: int = MAX_CLIENT_STDERR_BYTES,
) -> ProcessCapture:
    if timeout_seconds <= 0 or stdout_limit <= 0 or stderr_limit <= 0:
        raise BenchmarkError("process supervision limits must be positive")
    prepared_command = list(command)
    prepared_environment = dict(environment)
    stdout_chunks: list[bytes] = []
    totals = {"stdout": 0, "stderr": 0}
    selector = selectors.DefaultSelector()
    active_streams: dict[int, tuple[str, int, bool]] = {}
    retained_stdout = 0

    def close_stream(file_descriptor: int) -> None:
        with contextlib.suppress(Exception):
            selector.unregister(file_descriptor)
        active_streams.pop(file_descriptor, None)

    def read_ready(file_descriptor: int) -> None:
        nonlocal output_limit_exceeded, retained_stdout
        name, limit, retain = active_streams[file_descriptor]
        try:
            chunk = os.read(file_descriptor, 64 * 1024)
        except BlockingIOError:
            return
        except OSError as error:
            raise BenchmarkError("failed to drain bounded child output") from error
        if not chunk:
            close_stream(file_descriptor)
            return
        totals[name] += len(chunk)
        if retain and retained_stdout < limit:
            kept = chunk[: limit - retained_stdout]
            stdout_chunks.append(kept)
            retained_stdout += len(kept)
        if totals[name] > limit:
            output_limit_exceeded = True

    try:
        process = subprocess.Popen(
            prepared_command,
            cwd=cwd,
            env=prepared_environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except BaseException:
        selector.close()
        raise

    run_error: BaseException | None = None
    timed_out = False
    output_limit_exceeded = False
    try:
        deadline = time.monotonic() + timeout_seconds
        if process.stdout is None or process.stderr is None:
            raise BenchmarkError("failed to create bounded child output pipes")
        for stream, name, limit, retain in (
            (process.stdout, "stdout", stdout_limit, True),
            (process.stderr, "stderr", stderr_limit, False),
        ):
            file_descriptor = stream.fileno()
            os.set_blocking(file_descriptor, False)
            active_streams[file_descriptor] = (name, limit, retain)
            try:
                selector.register(file_descriptor, selectors.EVENT_READ)
            except BaseException:
                active_streams.pop(file_descriptor, None)
                raise

        while process.poll() is None:
            if output_limit_exceeded:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                break
            for key, _mask in selector.select(min(remaining, 0.05)):
                read_ready(key.fd)
    except BaseException as error:
        run_error = error
    finally:
        # The private session is disposable. Terminate the whole group on every
        # outcome so a completed leader cannot leave pipe-holding descendants.
        cleanup_deadline = time.monotonic() + PROCESS_SHUTDOWN_TIMEOUT_SECONDS
        cleanup_error: BaseException | None = None
        try:
            _kill_process_group(process, deadline=cleanup_deadline)
        except BaseException as error:
            cleanup_error = error

        if cleanup_error is None:
            while active_streams and time.monotonic() < cleanup_deadline:
                events = selector.select(
                    min(0.05, max(0.0, cleanup_deadline - time.monotonic()))
                )
                if not events:
                    continue
                try:
                    for key, _mask in events:
                        read_ready(key.fd)
                except BaseException as error:
                    cleanup_error = error
                    break
            if active_streams:
                cleanup_error = cleanup_error or BenchmarkError(
                    "child output pipes did not reach EOF"
                )

        selector.close()
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                with contextlib.suppress(OSError):
                    stream.close()
        if cleanup_error is not None:
            raise BenchmarkError("child process cleanup could not be verified") from (
                run_error or cleanup_error
            )

    if run_error is not None:
        raise run_error

    return ProcessCapture(
        returncode=None if timed_out else process.returncode,
        stdout=b"".join(stdout_chunks),
        stderr_bytes=totals["stderr"],
        timed_out=timed_out,
        stdout_truncated=totals["stdout"] > stdout_limit,
        stderr_truncated=totals["stderr"] > stderr_limit,
        output_limit_exceeded=output_limit_exceeded,
    )


def _literal_loopback_endpoints(
    host: str, port: int
) -> tuple[tuple[int, tuple[Any, ...]], ...]:
    if host == "127.0.0.1":
        return ((socket.AF_INET, (host, port)),)
    if host == "::1":
        return ((socket.AF_INET6, (host, port, 0, 0)),)
    if host == "localhost":
        return (
            (socket.AF_INET, ("127.0.0.1", port)),
            (socket.AF_INET6, ("::1", port, 0, 0)),
        )
    raise BenchmarkError("test upstream was not an exact loopback host")


def _resolve_upstream_endpoints(
    host: str, port: int
) -> tuple[tuple[int, tuple[Any, ...]], ...]:
    resolver = """
import json
import socket
import sys

host = sys.argv[1]
port = int(sys.argv[2])
results = []
for family, kind, protocol, _canonical, address in socket.getaddrinfo(
    host, port, type=socket.SOCK_STREAM, proto=socket.IPPROTO_TCP
):
    if family in (socket.AF_INET, socket.AF_INET6):
        results.append([family, list(address)])
print(json.dumps(results, separators=(",", ":")))
"""
    capture = _run_process(
        [sys.executable, "-I", "-c", resolver, host, str(port)],
        Path.cwd(),
        {"PATH": os.environ.get("PATH") or os.defpath, "LC_ALL": "C"},
        timeout_seconds=UPSTREAM_RESOLVE_TIMEOUT_SECONDS,
        stdout_limit=64 * 1024,
        stderr_limit=64 * 1024,
    )
    if (
        capture.returncode != 0
        or capture.timed_out
        or capture.output_limit_exceeded
        or capture.stderr_truncated
    ):
        raise BenchmarkError("official upstream DNS resolution failed")
    try:
        raw_endpoints = json.loads(capture.stdout)
    except (UnicodeError, json.JSONDecodeError) as error:
        raise BenchmarkError("official upstream DNS result was malformed") from error
    if not isinstance(raw_endpoints, list) or not 1 <= len(raw_endpoints) <= 32:
        raise BenchmarkError("official upstream DNS result was empty or oversized")
    endpoints: list[tuple[int, tuple[Any, ...]]] = []
    for entry in raw_endpoints:
        if (
            not isinstance(entry, list)
            or len(entry) != 2
            or entry[0] not in {socket.AF_INET, socket.AF_INET6}
            or not isinstance(entry[1], list)
        ):
            raise BenchmarkError("official upstream DNS endpoint was malformed")
        family = entry[0]
        address = entry[1]
        expected_length = 2 if family == socket.AF_INET else 4
        if len(address) != expected_length or address[1] != port:
            raise BenchmarkError("official upstream DNS endpoint was malformed")
        try:
            parsed_ip = ipaddress.ip_address(address[0])
        except ValueError as error:
            raise BenchmarkError("official upstream DNS address was malformed") from error
        if (family == socket.AF_INET) != isinstance(parsed_ip, ipaddress.IPv4Address):
            raise BenchmarkError("official upstream DNS address family disagreed")
        if family == socket.AF_INET6 and any(
            isinstance(value, bool) or not isinstance(value, int) or value < 0
            for value in address[2:]
        ):
            raise BenchmarkError("official upstream IPv6 endpoint was malformed")
        endpoint = (family, tuple(address))
        if endpoint not in endpoints:
            endpoints.append(endpoint)
    if not endpoints:
        raise BenchmarkError("official upstream DNS result contained no usable address")
    return tuple(endpoints)


def _parse_stdout_object(capture: ProcessCapture) -> tuple[dict[str, Any] | None, str | None]:
    if capture.stdout_truncated:
        return None, "client_stdout_too_large"
    try:
        value = json.loads(capture.stdout)
    except (UnicodeError, json.JSONDecodeError):
        return None, "client_stdout_not_single_json_object"
    if not isinstance(value, dict):
        return None, "client_stdout_not_single_json_object"
    return value, None


def parse_fx_client_output(capture: ProcessCapture) -> dict[str, Any]:
    errors: list[str] = []
    if capture.timed_out:
        errors.append("client_timeout")
    if capture.returncode != 0:
        errors.append("process_exit_nonzero")
    if capture.stderr_truncated:
        errors.append("client_stderr_too_large")
    value, parse_error = _parse_stdout_object(capture)
    if parse_error is not None:
        errors.append(parse_error)
        value = {}
    json_exit = value.get("exit_code")
    if isinstance(json_exit, bool) or not isinstance(json_exit, int) or json_exit != 0:
        errors.append("fx_json_exit_nonzero_or_missing")
    final_output_valid = value.get("output") == ACK_TOKEN
    if not final_output_valid:
        errors.append("fx_final_output_mismatch")
    model_valid = value.get("model") == MODEL
    if not model_valid:
        errors.append("fx_reported_model_mismatch")
    tool_calls = value.get("tool_calls")
    if not isinstance(tool_calls, list):
        errors.append("fx_tool_calls_not_array")
        reported_tool_calls: int | None = None
    else:
        reported_tool_calls = len(tool_calls)
    return {
        "valid": not errors,
        "errors": list(dict.fromkeys(errors)),
        "process_exit_code": capture.returncode,
        "timed_out": capture.timed_out,
        "stdout_bytes": len(capture.stdout),
        "stderr_bytes": capture.stderr_bytes,
        "final_output_valid": final_output_valid,
        "reported_model_valid": model_valid,
        "reported_tool_calls": reported_tool_calls,
        "client_configuration_echo_valid": model_valid,
    }


def _is_nonnegative_integer(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _nanocodex_diagnostics_valid(value: Mapping[str, Any]) -> bool:
    timing = value.get("timing_ns")
    if not isinstance(timing, dict):
        return False
    required_timings = (
        "prompt_submit_to_acceptance",
        "prompt_submit_to_first_assistant_delta_emitted",
        "prompt_acceptance_to_first_assistant_delta_emitted",
        "prompt_submit_to_first_assistant_delta_received",
        "prompt_acceptance_to_first_assistant_delta_received",
        "assistant_delta_emit_to_receive",
        "prompt_submit_to_result_completion",
        "prompt_acceptance_to_result_completion",
    )
    if not all(_is_nonnegative_integer(timing.get(key)) for key in required_timings):
        return False
    provider_to_emit = timing.get("provider_source_to_assistant_delta_emit")
    if provider_to_emit is not None and not _is_nonnegative_integer(provider_to_emit):
        return False

    usage = value.get("usage")
    turn_usage = value.get("turn_usage")
    token_fields = (
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    )
    if not isinstance(usage, dict) or not isinstance(usage.get("reported"), bool):
        return False
    if not all(_is_nonnegative_integer(usage.get(key)) for key in token_fields):
        return False
    if not isinstance(turn_usage, dict) or not all(
        _is_nonnegative_integer(turn_usage.get(key)) for key in token_fields
    ):
        return False

    events = value.get("events")
    return isinstance(events, dict) and all(
        (
            _is_nonnegative_integer(events.get("count")),
            _is_nonnegative_integer(events.get("first_sequence")),
            _is_nonnegative_integer(events.get("last_sequence")),
            _is_nonnegative_integer(events.get("assistant_delta_count")),
            events.get("sequences_contiguous") is True,
        )
    )


def parse_nanocodex_client_output(
    capture: ProcessCapture, *, source_commit: str, cwd: Path
) -> dict[str, Any]:
    errors: list[str] = []
    if capture.timed_out:
        errors.append("client_timeout")
    if capture.returncode != 0:
        errors.append("process_exit_nonzero")
    if capture.stderr_truncated:
        errors.append("client_stderr_too_large")
    value, parse_error = _parse_stdout_object(capture)
    if parse_error is not None:
        errors.append(parse_error)
        value = {}

    provenance = value.get("provenance")
    configuration_echo_valid = isinstance(provenance, dict) and all(
        (
            value.get("schema_version") == 1,
            value.get("benchmark") == "paired_fx_model_latency",
            provenance.get("implementation") == "nanocodex" if isinstance(provenance, dict) else False,
            provenance.get("source_commit") == source_commit if isinstance(provenance, dict) else False,
            provenance.get("model") == MODEL if isinstance(provenance, dict) else False,
            provenance.get("thinking") == EFFORT if isinstance(provenance, dict) else False,
            provenance.get("fast_mode") is False if isinstance(provenance, dict) else False,
            provenance.get("workspace") == str(cwd) if isinstance(provenance, dict) else False,
            provenance.get("instructions_fnv1a64") == fnv1a64(SYSTEM_INSTRUCTIONS)
            if isinstance(provenance, dict)
            else False,
            provenance.get("prompt_fnv1a64") == fnv1a64(ACK_TOKEN)
            if isinstance(provenance, dict)
            else False,
            provenance.get("expected_fnv1a64") == fnv1a64(ACK_TOKEN)
            if isinstance(provenance, dict)
            else False,
            provenance.get("prompt_cache_key_fnv1a64")
            == fnv1a64(NANOCODEX_PROMPT_CACHE_KEY)
            if isinstance(provenance, dict)
            else False,
        )
    )
    if not configuration_echo_valid:
        errors.append("nanocodex_configuration_echo_invalid")
    if value.get("transport") != "https":
        errors.append("nanocodex_transport_invalid")

    verified = value.get("verified")
    final_output_valid = isinstance(verified, dict) and verified.get("final_output") is True
    functional_checks = isinstance(verified, dict) and all(
        verified.get(key) is True
        for key in (
            "final_output",
            "assistant_deltas",
            "one_model_call",
            "zero_tool_calls",
            "run_completed",
            "clean_shutdown",
            "auth_refresh_disabled",
        )
    )
    if not functional_checks:
        errors.append("nanocodex_functional_verification_failed")

    model_call = value.get("model_call")
    model_call_valid = isinstance(model_call, dict) and all(
        (
            model_call.get("call_index") == 1,
            isinstance(model_call.get("attempt"), int)
            and not isinstance(model_call.get("attempt"), bool)
            and model_call["attempt"] >= 1,
            isinstance(model_call.get("connection_generation"), int)
            and not isinstance(model_call.get("connection_generation"), bool)
            and model_call["connection_generation"] == 0,
            model_call.get("status") == "completed",
            _is_nonnegative_integer(model_call.get("duration_ns")),
            _is_nonnegative_integer(model_call.get("time_to_first_event_ns")),
            model_call.get("time_to_first_output_ns") is None
            or _is_nonnegative_integer(model_call.get("time_to_first_output_ns")),
            _is_nonnegative_integer(model_call.get("tool_calls")),
        )
    )
    if not model_call_valid:
        errors.append("nanocodex_model_call_invalid")
    reported_tool_calls = (
        model_call.get("tool_calls") if isinstance(model_call, dict) else None
    )

    usage = value.get("usage")
    reported_cached = usage.get("cached_input_tokens") if isinstance(usage, dict) else None
    if reported_cached is not None and not _is_nonnegative_integer(reported_cached):
        errors.append("nanocodex_cached_tokens_invalid")
        reported_cached = None
    if not _nanocodex_diagnostics_valid(value):
        errors.append("nanocodex_diagnostics_invalid")

    return {
        "valid": not errors,
        "errors": list(dict.fromkeys(errors)),
        "process_exit_code": capture.returncode,
        "timed_out": capture.timed_out,
        "stdout_bytes": len(capture.stdout),
        "stderr_bytes": capture.stderr_bytes,
        "final_output_valid": final_output_valid,
        "reported_tool_calls": reported_tool_calls,
        "reported_cached_input_tokens": reported_cached,
        "client_configuration_echo_valid": configuration_echo_valid,
    }


def _missing_observation(expected: ExpectedRequest) -> dict[str, Any]:
    return _empty_observer_record(
        expected, request_bytes=None, error="client_sent_no_observed_request"
    )


def _required_timing_present(observation: Mapping[str, Any]) -> bool:
    timing = observation.get("timing_ns")
    return isinstance(timing, dict) and _is_nonnegative_integer(
        timing.get("time_to_first_model_output")
    )


def _combined_run(
    implementation: str,
    client: dict[str, Any],
    observer: dict[str, Any],
    unexpected_requests: int,
) -> dict[str, Any]:
    observer_functional = (
        observer.get("error") is None
        and observer.get("status") == "completed"
        and observer.get("upstream_http_status") is not None
        and 200 <= observer["upstream_http_status"] < 300
    )
    functional_valid = bool(client.get("valid")) and observer_functional
    client_tool_count = client.get("reported_tool_calls")
    no_tool_calls = (
        client_tool_count == 0
        and observer.get("tool_call_detected") is False
        and unexpected_requests == 0
    )
    timing_present = _required_timing_present(observer)
    measured_failure = not (
        functional_valid and no_tool_calls and timing_present and unexpected_requests == 0
    )
    return {
        "implementation": implementation,
        "functional_valid": functional_valid,
        "no_tool_calls": no_tool_calls,
        "timing_present": timing_present,
        "cached_input_tokens": observer.get("cached_input_tokens"),
        "unexpected_request_count": unexpected_requests,
        "measured_failure": measured_failure,
        "client_configuration_echo_valid": bool(
            client.get("client_configuration_echo_valid")
        ),
        "client": client,
        "observer": observer,
    }


def _run_implementation(
    implementation: str,
    *,
    pair: int,
    phase: str,
    sequence: int,
    args: argparse.Namespace,
    auth: AuthMaterial,
    observer: StreamingObserver,
    runtime_root: Path,
    executables: Mapping[str, tuple[Path, str]],
) -> dict[str, Any]:
    auth.assert_live_window()
    executable, expected_digest = executables[implementation]
    _assert_executable_copy(executable, expected_digest)
    with tempfile.TemporaryDirectory(
        prefix=f"{sequence:04d}-{implementation}-", dir=runtime_root
    ) as raw_home:
        generated = Path(raw_home)
        generated.rmdir()
        if implementation == "fx":
            prepare_fx_home(generated, auth)
        else:
            prepare_nanocodex_home(generated, auth)
        generated = generated.resolve(strict=True)
        workspace = generated / "workspace"
        _mkdir_private(workspace)
        workspace = workspace.resolve(strict=True)
        expected = ExpectedRequest(
            implementation=implementation,
            pair=pair,
            phase=phase,
            home=str(generated),
            workspace=str(workspace),
        )
        before_unexpected = observer.state.stats()["unexpected_requests"]
        ticket = observer.state.arm(expected)
        try:
            environment = _child_environment(generated)
            if implementation == "fx":
                environment.update(
                    {
                        "FX_DISABLE_KEYCHAIN": "1",
                        "FX_AUTO_UPGRADE": "0",
                        "FX_E2E_OPENAI_CODEX_RESPONSES_URL": observer.base_url
                        + "/responses",
                        "FX_E2E_OPENAI_CODEX_MODELS_URL": observer.base_url + "/models",
                        "FX_E2E_CHATGPT_TOKEN_URL": observer.base_url + "/forbid-refresh",
                    }
                )
                command = [
                    str(executable),
                    "ask",
                    "--json",
                    "--no-save",
                    "--auto",
                    "--system",
                    SYSTEM_INSTRUCTIONS,
                    "--",
                    ACK_TOKEN,
                ]
                capture = _run_process(
                    _sandboxed_child_command(command, observer.base_url),
                    workspace,
                    environment,
                )
                client = parse_fx_client_output(capture)
            else:
                auth_path = generated / ".codex" / "auth.json"
                environment["CODEX_HOME"] = str(auth_path.parent)
                command = [
                    str(executable),
                    "--cwd",
                    str(workspace),
                    "--auth-file",
                    str(auth_path),
                    "--api-base-url",
                    observer.base_url,
                    "--instructions",
                    SYSTEM_INSTRUCTIONS,
                    "--prompt",
                    ACK_TOKEN,
                    "--expected",
                    ACK_TOKEN,
                    "--source-commit",
                    args.nanocodex_commit,
                    "--transport",
                    "https",
                ]
                capture = _run_process(
                    _sandboxed_child_command(command, observer.base_url),
                    workspace,
                    environment,
                )
                client = parse_nanocodex_client_output(
                    capture,
                    source_commit=args.nanocodex_commit,
                    cwd=workspace,
                )
        except BaseException:
            observer.state.cancel_unclaimed(ticket)
            raise

        if observer.state.cancel_unclaimed(ticket):
            observed = _missing_observation(expected)
        else:
            observed = observer.state.wait(ticket, OBSERVER_RESULT_TIMEOUT_SECONDS)
            if observed is None:
                observed = _missing_observation(expected)
        after_unexpected = observer.state.stats()["unexpected_requests"]
    return _combined_run(
        implementation,
        client,
        observed,
        after_unexpected - before_unexpected,
    )


def _offline_actual_binary_preflight(
    *,
    args: argparse.Namespace,
    runtime_root: Path,
    executables: Mapping[str, tuple[Path, str]],
    fresh_identifier_ledger: FreshUuid7Ledger,
) -> tuple[dict[str, str], dict[str, Any]]:
    fake_auth = _fake_auth_material()
    run_count = 4
    preflight_runtime_root = runtime_root / XML_METACHAR_PREFLIGHT_DIRECTORY
    _mkdir_private(preflight_runtime_root)
    with _OfflineProvider() as provider:
        with StreamingObserver._for_test(
            fake_auth,
            provider.url,
            provider_request_budget=run_count,
            fresh_identifier_ledger=fresh_identifier_ledger,
            require_fx_models_discovery=True,
        ) as observer:
            for sequence, implementation in enumerate(
                ("fx", "nanocodex", "nanocodex", "fx"), 1
            ):
                run = _run_implementation(
                    implementation,
                    pair=(sequence + 1) // 2,
                    phase="offline_preflight",
                    sequence=sequence,
                    args=args,
                    auth=fake_auth,
                    observer=observer,
                    runtime_root=(
                        preflight_runtime_root
                        if implementation == "nanocodex"
                        else runtime_root
                    ),
                    executables=executables,
                )
                if run["measured_failure"]:
                    client_errors = run.get("client", {}).get("errors", [])
                    observer_error = run.get("observer", {}).get("error")
                    safe_detail = observer_error or ",".join(client_errors) or "unknown"
                    raise BenchmarkError(
                        f"{implementation} actual-binary offline preflight failed: {safe_detail}"
                    )
            stats = observer.state.stats()
            fingerprints = observer.state.request_fingerprints()
            stable = observer.state.fingerprints_stable()
            shared_auth_verified = observer.auth_matcher.verified()
        with provider.server.lock:
            upstream_requests = provider.server.requests

    if (
        stats["provider_requests_forwarded"] != run_count
        or upstream_requests != run_count
        or stats["models_requests"] != run_count // 2
        or stats["unexpected_requests"] != 0
        or stats["refresh_attempts_blocked"] != 0
        or not stable
        or not shared_auth_verified
    ):
        raise BenchmarkError("actual-binary offline preflight containment failed")

    fedramp_auth = _fake_auth_material(fedramp=True)
    fedramp_run_count = 2
    with _OfflineProvider() as fedramp_provider:
        with StreamingObserver._for_test(
            fedramp_auth,
            fedramp_provider.url,
            provider_request_budget=fedramp_run_count,
            fresh_identifier_ledger=fresh_identifier_ledger,
            require_fx_models_discovery=True,
        ) as fedramp_observer:
            for sequence in range(run_count + 1, run_count + fedramp_run_count + 1):
                run = _run_implementation(
                    "nanocodex",
                    pair=sequence,
                    phase="offline_fedramp_preflight",
                    sequence=sequence,
                    args=args,
                    auth=fedramp_auth,
                    observer=fedramp_observer,
                    runtime_root=preflight_runtime_root,
                    executables=executables,
                )
                if run["measured_failure"]:
                    client_errors = run.get("client", {}).get("errors", [])
                    observer_error = run.get("observer", {}).get("error")
                    safe_detail = observer_error or ",".join(client_errors) or "unknown"
                    raise BenchmarkError(
                        "nanocodex actual-binary FedRAMP preflight failed: "
                        f"{safe_detail}"
                    )
            fedramp_stats = fedramp_observer.state.stats()
            fedramp_fingerprints = fedramp_observer.state.request_fingerprints()
            fedramp_auth_verified = fedramp_observer.auth_matcher.verified_for(
                {"nanocodex"}
            )
        with fedramp_provider.server.lock:
            fedramp_upstream_requests = fedramp_provider.server.requests
    if (
        fedramp_stats["provider_requests_forwarded"] != fedramp_run_count
        or fedramp_upstream_requests != fedramp_run_count
        or fedramp_stats["models_requests"] != 0
        or fedramp_stats["unexpected_requests"] != 0
        or fedramp_stats["refresh_attempts_blocked"] != 0
        or fedramp_fingerprints != {"nanocodex": fingerprints["nanocodex"]}
        or not fedramp_auth_verified
    ):
        raise BenchmarkError("actual-binary FedRAMP preflight containment failed")
    freshness_evidence = fresh_identifier_ledger.evidence()
    if not freshness_evidence["distinct_and_one_use_verified"]:
        raise BenchmarkError("actual-binary UUIDv7 freshness preflight failed")
    return fingerprints, {
        "passed": True,
        "provider_network_used": False,
        "scripted_loopback_requests": upstream_requests + fedramp_upstream_requests,
        "runs_per_implementation": 2,
        "nanocodex_fedramp_runs": fedramp_run_count,
        "nanocodex_fedramp_header_verified_from_id_token": True,
        "contradictory_id_and_access_token_claims_verified": True,
        "fx_exact_models_discovery_once_per_run_verified": True,
        "fresh_uuid7_identifiers_distinct_and_one_use_verified": freshness_evidence[
            "distinct_and_one_use_verified"
        ],
        "xml_metacharacter_private_paths_verified": True,
        "distinct_private_workspace_fingerprints_stable": stable,
        "exact_executable_request_templates_captured": True,
        "independent_reference_body_fingerprints_verified": True,
    }


def percentile(values: Sequence[int], quantile: float) -> int | None:
    if not values:
        return None
    if not 0 < quantile <= 1:
        raise BenchmarkError("nearest-rank quantiles must be in (0, 1]")
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * quantile) - 1]


def distribution(values: Sequence[int]) -> dict[str, Any]:
    return {
        "n": len(values),
        "percentile_method": "nearest_rank",
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "min": min(values) if values else None,
        "max": max(values) if values else None,
    }


def _pair_reasons(
    pair: Mapping[str, Any], *, cache_policy: str
) -> list[str]:
    reasons: list[str] = []
    runs = pair.get("runs")
    if not isinstance(runs, dict):
        return ["malformed_pair"]
    for implementation in ("fx", "nanocodex"):
        run = runs.get(implementation)
        if not isinstance(run, dict):
            reasons.append(f"{implementation}_missing")
            continue
        if run.get("functional_valid") is not True:
            reasons.append(f"{implementation}_functionally_invalid")
        if run.get("no_tool_calls") is not True:
            reasons.append(f"{implementation}_tool_call_or_extra_request")
        if run.get("timing_present") is not True:
            reasons.append(f"{implementation}_timing_missing")
    if cache_policy not in {"ignore", "warm", "equal"}:
        raise BenchmarkError("unknown cache eligibility policy")
    if cache_policy != "ignore" and all(
        isinstance(runs.get(name), dict) for name in ("fx", "nanocodex")
    ):
        fx_cached = runs["fx"].get("cached_input_tokens")
        nano_cached = runs["nanocodex"].get("cached_input_tokens")
        if not _is_nonnegative_integer(fx_cached) or not _is_nonnegative_integer(
            nano_cached
        ):
            reasons.append("cached_input_tokens_unknown_or_invalid")
        elif cache_policy == "warm" and fx_cached == 0 and nano_cached == 0:
            reasons.append("cached_input_both_cold")
        elif cache_policy == "warm" and (fx_cached == 0 or nano_cached == 0):
            reasons.append("cached_input_warm_cold_mismatch")
        elif cache_policy == "equal" and fx_cached != nano_cached:
            reasons.append("cached_input_tokens_mismatch")
    return reasons


def _metric_value(run: Mapping[str, Any], metric: str) -> int | None:
    return run["observer"]["timing_ns"][metric]


def paired_metric(pairs: Sequence[Mapping[str, Any]], metric: str) -> dict[str, Any]:
    eligible = [
        pair
        for pair in pairs
        if _is_nonnegative_integer(_metric_value(pair["runs"]["fx"], metric))
        and _is_nonnegative_integer(
            _metric_value(pair["runs"]["nanocodex"], metric)
        )
    ]
    fx_values = [_metric_value(pair["runs"]["fx"], metric) for pair in eligible]
    nano_values = [
        _metric_value(pair["runs"]["nanocodex"], metric) for pair in eligible
    ]
    deltas = [nano - fx for fx, nano in zip(fx_values, nano_values)]
    return {
        "unit": "ns",
        "eligible_pairs": len(eligible),
        "delta_definition": "nanocodex_minus_fx",
        "fx": distribution(fx_values),
        "nanocodex": distribution(nano_values),
        "paired_delta": distribution(deltas),
        "win_counts_are": "descriptive_only_not_inferential",
        "tie_threshold_ns": TIE_THRESHOLD_NS,
        "wins": {
            "nanocodex": sum(delta < -TIE_THRESHOLD_NS for delta in deltas),
            "fx": sum(delta > TIE_THRESHOLD_NS for delta in deltas),
            "ties": sum(abs(delta) <= TIE_THRESHOLD_NS for delta in deltas),
        },
    }


def summarize_pairs(pairs: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    primary: list[Mapping[str, Any]] = []
    strict_equal: list[Mapping[str, Any]] = []
    diagnostic: list[Mapping[str, Any]] = []
    exclusion_counts: dict[str, int] = {}
    cache_matches = 0
    cache_mismatches = 0
    cache_unknown = 0
    for pair in pairs:
        diagnostic_reasons = _pair_reasons(pair, cache_policy="ignore")
        primary_reasons = _pair_reasons(pair, cache_policy="warm")
        strict_reasons = _pair_reasons(pair, cache_policy="equal")
        if not diagnostic_reasons:
            diagnostic.append(pair)
            runs = pair["runs"]
            fx_cached = runs["fx"].get("cached_input_tokens")
            nano_cached = runs["nanocodex"].get("cached_input_tokens")
            if not _is_nonnegative_integer(fx_cached) or not _is_nonnegative_integer(
                nano_cached
            ):
                cache_unknown += 1
            elif fx_cached == nano_cached:
                cache_matches += 1
            else:
                cache_mismatches += 1
        if not primary_reasons:
            primary.append(pair)
        if not strict_reasons:
            strict_equal.append(pair)
        for reason in primary_reasons:
            exclusion_counts[reason] = exclusion_counts.get(reason, 0) + 1

    metrics = (
        "time_to_first_model_output",
        "time_to_first_answer_text",
        "time_to_terminal",
    )
    return {
        "primary": {
            "eligibility": (
                "both functionally valid, no tool/extra calls, scored TTFT present, "
                "and both product-specific cached-input counts are known "
                "positive integers; exact counts may differ and remain part of each "
                "product's shipped latency"
            ),
            "cache_exclusions": (
                "null/invalid counts, both-cold pairs, and warm/cold mismatches are "
                "not scored"
            ),
            "minimum_eligible_pairs": MIN_PRIMARY_ELIGIBLE_PAIRS,
            "eligible_pairs": len(primary),
            "metrics": {metric: paired_metric(primary, metric) for metric in metrics},
        },
        "strict_equal_cache_diagnostic": {
            "eligibility": (
                "all functional/scored-TTFT gates plus known nonnegative and exactly "
                "equal cached-input counts"
            ),
            "eligible_pairs": len(strict_equal),
            "metrics": {
                metric: paired_metric(strict_equal, metric) for metric in metrics
            },
        },
        "all_valid_diagnostic": {
            "eligibility": (
                "both functionally valid, no tool/extra calls, and scored TTFT "
                "present; optional diagnostic timings are summarized where present; "
                "cache equality ignored"
            ),
            "eligible_pairs": len(diagnostic),
            "cache_equal_pairs": cache_matches,
            "cache_mismatch_pairs": cache_mismatches,
            "cache_unknown_pairs": cache_unknown,
            "metrics": {
                metric: paired_metric(diagnostic, metric) for metric in metrics
            },
        },
        "primary_exclusions": exclusion_counts,
    }


def _local_command_environment() -> dict[str, str]:
    return {
        "PATH": os.environ.get("PATH") or os.defpath,
        "LC_ALL": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_OPTIONAL_LOCKS": "0",
    }


def _git_output(root: Path, *arguments: str) -> str:
    capture = _run_process(
        ["git", "-C", str(root), *arguments],
        root,
        _local_command_environment(),
        timeout_seconds=15,
        stdout_limit=128 * 1024,
        stderr_limit=128 * 1024,
    )
    if (
        capture.returncode != 0
        or capture.timed_out
        or capture.output_limit_exceeded
        or capture.stderr_truncated
    ):
        raise BenchmarkError("source checkout git inspection failed")
    try:
        return capture.stdout.decode("utf-8").rstrip("\n")
    except UnicodeError as error:
        raise BenchmarkError("source checkout git output was not UTF-8") from error


def _source_provenance(
    source_root: Path,
    executable: Path,
    expected_commit: str,
    implementation: str,
) -> dict[str, Any]:
    if not COMMIT_RE.fullmatch(expected_commit):
        raise BenchmarkError("source commits must be full 40-character hexadecimal IDs")
    expected_commit = expected_commit.lower()
    try:
        root = source_root.resolve(strict=True)
    except OSError as error:
        raise BenchmarkError(f"{implementation} source root is not readable") from error
    if not root.is_dir():
        raise BenchmarkError(f"{implementation} source root must be a directory")
    try:
        git_root = Path(_git_output(root, "rev-parse", "--show-toplevel")).resolve(
            strict=True
        )
    except OSError as error:
        raise BenchmarkError(f"{implementation} git root is not readable") from error
    if git_root != root:
        raise BenchmarkError(
            f"{implementation} source root must be the checkout top level"
        )
    head = _git_output(root, "rev-parse", "--verify", "HEAD").lower()
    if head != expected_commit:
        raise BenchmarkError(f"{implementation} source checkout HEAD mismatch")
    if _git_output(root, "status", "--porcelain=v1", "--untracked-files=all"):
        raise BenchmarkError(f"{implementation} source checkout is not clean")
    try:
        resolved = executable.resolve(strict=True)
        resolved.relative_to(root)
        info = resolved.stat()
    except (OSError, ValueError) as error:
        raise BenchmarkError(
            f"{implementation} executable must resolve beneath its source checkout"
        ) from error
    if not stat.S_ISREG(info.st_mode) or not os.access(resolved, os.X_OK):
        raise BenchmarkError(
            f"{implementation} executable must be an executable regular file"
        )
    return {
        "implementation": implementation,
        "source_root": str(root),
        "expected_commit": expected_commit,
        "git_head": head,
        "tree_clean": True,
        "binary": {
            "path": str(resolved),
            "sha256": sha256_file(resolved),
            "byte_count": info.st_size,
            "source_build_attestation": "not_claimed",
        },
    }


def _copy_executable(
    provenance: Mapping[str, Any], runtime_root: Path
) -> Path:
    implementation = provenance["implementation"]
    source = Path(provenance["binary"]["path"])
    destination = runtime_root / f"{implementation}-immutable"
    descriptor = os.open(
        destination,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o500,
    )
    try:
        with source.open("rb") as reader, os.fdopen(
            descriptor, "wb", closefd=True
        ) as writer:
            shutil.copyfileobj(reader, writer, length=1024 * 1024)
            writer.flush()
            os.fsync(writer.fileno())
            os.fchmod(writer.fileno(), 0o500)
    except BaseException:
        with contextlib.suppress(OSError):
            os.close(descriptor)
        with contextlib.suppress(OSError):
            destination.unlink()
        raise
    _assert_executable_copy(destination, provenance["binary"]["sha256"])
    return destination


def _assert_executable_copy(path: Path, expected_sha256: str) -> None:
    try:
        info = path.lstat()
    except OSError as error:
        raise BenchmarkError("private executable copy is not readable") from error
    if (
        not stat.S_ISREG(info.st_mode)
        or stat.S_IMODE(info.st_mode) != 0o500
        or info.st_nlink != 1
        or not os.access(path, os.X_OK)
        or sha256_file(path) != expected_sha256
    ):
        raise BenchmarkError("private executable copy failed its immutable digest check")


def _source_unchanged(provenance: Mapping[str, Any]) -> bool:
    try:
        current = _source_provenance(
            Path(provenance["source_root"]),
            Path(provenance["binary"]["path"]),
            provenance["expected_commit"],
            provenance["implementation"],
        )
    except BenchmarkError:
        return False
    return current["binary"]["sha256"] == provenance["binary"]["sha256"]


def _preflight_report_output(path: Path) -> None:
    if os.path.lexists(path):
        raise BenchmarkError("--output already exists; refusing to overwrite it")
    descriptor = -1
    temporary = ""
    try:
        descriptor, temporary = tempfile.mkstemp(
            prefix=f".{path.name}.preflight-", dir=path.parent
        )
        os.fchmod(descriptor, 0o600)
        os.write(descriptor, b"{}\n")
        os.fsync(descriptor)
    except OSError as error:
        raise BenchmarkError("report output failed its preflight write") from error
    finally:
        if descriptor >= 0:
            with contextlib.suppress(OSError):
                os.close(descriptor)
        if temporary:
            with contextlib.suppress(OSError):
                os.unlink(temporary)


def _validate_and_resolve_args(args: argparse.Namespace) -> tuple[dict[str, Any], dict[str, Any]]:
    if not args.confirm_paid_live_run:
        raise BenchmarkError("--confirm-paid-live-run is required")
    if (
        isinstance(args.trials, bool)
        or not isinstance(args.trials, int)
        or args.trials < MIN_TRIALS
        or args.trials > MAX_TRIALS
        or args.trials % 2 != 0
    ):
        raise BenchmarkError(
            f"--trials must be an even integer from {MIN_TRIALS} through {MAX_TRIALS}"
        )
    if (
        isinstance(args.warmup_pairs, bool)
        or not isinstance(args.warmup_pairs, int)
        or args.warmup_pairs < 0
        or args.warmup_pairs > MAX_WARMUP_PAIRS
    ):
        raise BenchmarkError(
            f"--warmup-pairs must be from 0 through {MAX_WARMUP_PAIRS}"
        )
    request_budget = 2 * (args.warmup_pairs + args.trials)
    if request_budget <= 0 or request_budget > MAX_PROVIDER_REQUESTS:
        raise BenchmarkError("provider request budget is outside the bounded range")
    args.provider_request_budget = request_budget
    _require_child_egress_sandbox()
    args.nanocodex_commit = args.nanocodex_commit.lower()
    args.auth_file = args.auth_file.resolve(strict=True)
    args.output = args.output.resolve()
    if not args.output.parent.is_dir():
        raise BenchmarkError("--output parent directory does not exist")
    _preflight_report_output(args.output)
    fx = _source_provenance(
        args.fx_source_root, args.fx_bin, FX_COMMIT, "fx"
    )
    nano = _source_provenance(
        args.nanocodex_source_root,
        args.nanocodex_bin,
        args.nanocodex_commit,
        "nanocodex",
    )
    args.fx_source_root = Path(fx["source_root"])
    args.nanocodex_source_root = Path(nano["source_root"])
    args.fx_bin = Path(fx["binary"]["path"])
    args.nanocodex_bin = Path(nano["binary"]["path"])
    return fx, nano


def _order(pair: int) -> list[str]:
    return ["fx", "nanocodex"] if pair % 2 == 1 else ["nanocodex", "fx"]


def _warmup_failure_summary(pair: int, run: Mapping[str, Any]) -> dict[str, Any]:
    return {
        "pair": pair,
        "implementation": run.get("implementation"),
        "client_errors": run.get("client", {}).get("errors", []),
        "observer_error": run.get("observer", {}).get("error"),
    }


def run_benchmark(args: argparse.Namespace) -> tuple[dict[str, Any], int, AuthMaterial]:
    fx_provenance, nano_provenance = _validate_and_resolve_args(args)
    fresh_identifier_ledger = FreshUuid7Ledger()
    start_utc = dt.datetime.now(dt.timezone.utc).isoformat()
    measured_pairs: list[dict[str, Any]] = []
    warmup_failures: list[dict[str, Any]] = []
    warmup_pairs_completed = 0
    sequence = 0
    aborted_before_measurement = False
    aborted_on_measured_failure = False
    copy_integrity = {"fx": False, "nanocodex": False}

    with tempfile.TemporaryDirectory(prefix="nanocodex-fx-latency-") as raw_root:
        runtime_root = Path(raw_root)
        os.chmod(runtime_root, 0o700)
        fx_copy = _copy_executable(fx_provenance, runtime_root)
        nano_copy = _copy_executable(nano_provenance, runtime_root)
        executables = {
            "fx": (fx_copy, fx_provenance["binary"]["sha256"]),
            "nanocodex": (
                nano_copy,
                nano_provenance["binary"]["sha256"],
            ),
        }
        request_templates, offline_preflight = _offline_actual_binary_preflight(
            args=args,
            runtime_root=runtime_root,
            executables=executables,
            fresh_identifier_ledger=fresh_identifier_ledger,
        )
        auth = load_auth_snapshot(args.auth_file)
        with StreamingObserver(
            auth,
            args.provider_request_budget,
            expected_fingerprints=request_templates,
            fresh_identifier_ledger=fresh_identifier_ledger,
        ) as observer:
            for pair in range(1, args.warmup_pairs + 1):
                pair_failed = False
                for implementation in _order(pair):
                    sequence += 1
                    run = _run_implementation(
                        implementation,
                        pair=pair,
                        phase="warmup",
                        sequence=sequence,
                        args=args,
                        auth=auth,
                        observer=observer,
                        runtime_root=runtime_root,
                        executables=executables,
                    )
                    if run["measured_failure"]:
                        pair_failed = True
                        warmup_failures.append(_warmup_failure_summary(pair, run))
                        break
                if not pair_failed:
                    warmup_pairs_completed += 1
                if pair_failed:
                    aborted_before_measurement = True
                    break

            if not aborted_before_measurement:
                for pair in range(1, args.trials + 1):
                    schedule_pair = args.warmup_pairs + pair
                    pair_record: dict[str, Any] = {
                        "pair": pair,
                        "schedule_pair": schedule_pair,
                        "order": _order(schedule_pair),
                        "runs": {},
                    }
                    for implementation in pair_record["order"]:
                        sequence += 1
                        run = _run_implementation(
                            implementation,
                            pair=pair,
                            phase="measured",
                            sequence=sequence,
                            args=args,
                            auth=auth,
                            observer=observer,
                            runtime_root=runtime_root,
                            executables=executables,
                        )
                        pair_record["runs"][implementation] = run
                        if run["measured_failure"]:
                            aborted_on_measured_failure = True
                            break
                    measured_pairs.append(pair_record)
                    if aborted_on_measured_failure:
                        break

        # Snapshot only after the context's checked handler/upstream drain. A
        # teardown failure raises before report construction or publication.
        observer_stats = observer.state.stats()
        shared_auth_verified = observer.auth_matcher.verified()
        request_fingerprints = observer.state.request_fingerprints()
        fingerprints_stable = observer.state.fingerprints_stable()
        fresh_uuid7_identifiers_verified = (
            fresh_identifier_ledger.distinct_and_one_use_verified()
        )
        offline_preflight[
            "fresh_uuid7_identifiers_distinct_and_one_use_verified"
        ] = fresh_uuid7_identifiers_verified

        for implementation, (path, digest) in executables.items():
            try:
                _assert_executable_copy(path, digest)
            except BenchmarkError:
                copy_integrity[implementation] = False
            else:
                copy_integrity[implementation] = True

    fx_unchanged = _source_unchanged(fx_provenance)
    nano_unchanged = _source_unchanged(nano_provenance)
    source_integrity_failure = not all(
        (fx_unchanged, nano_unchanged, *copy_integrity.values())
    )
    measured_failures = sum(
        bool(run.get("measured_failure"))
        for pair in measured_pairs
        for run in pair["runs"].values()
    )
    summary = summarize_pairs(measured_pairs)
    insufficient_primary = (
        summary["primary"]["eligible_pairs"] < MIN_PRIMARY_ELIGIBLE_PAIRS
    )
    measured_pairs_completed = sum(
        set(pair.get("runs", {})) == {"fx", "nanocodex"}
        for pair in measured_pairs
    )
    incomplete_schedule = measured_pairs_completed != args.trials
    exit_conditions = {
        "warmup_failure": bool(warmup_failures),
        "measured_failure": measured_failures > 0,
        "insufficient_primary_eligible_pairs": insufficient_primary,
        "source_or_execution_copy_integrity_failure": source_integrity_failure,
        "incomplete_measured_schedule": incomplete_schedule,
        "shared_auth_identity_not_verified": not shared_auth_verified,
        "request_fingerprints_not_stable": not fingerprints_stable,
        "fresh_uuid7_identifiers_not_distinct_and_one_use": (
            not fresh_uuid7_identifiers_verified
        ),
        "credential_refresh_attempted": observer_stats["refresh_attempts_blocked"] > 0,
        "unexpected_request_seen": observer_stats["unexpected_requests"] > 0,
    }
    exit_code = int(any(exit_conditions.values()))
    document = {
        "schema_version": 1,
        "benchmark": "paired_fx_model_latency",
        "status": "passed" if exit_code == 0 else "failed",
        "started_at": start_utc,
        "completed_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "comparison_design": {
            "kind": "controlled_product_as_shipped",
            "identical_input_or_isolated_client_claim": False,
            "tool_surfaces": "implementation_specific_product_defaults",
            "source_build_attestation_claimed": False,
            "request_equivalence": (
                "each live request must match the independently reviewed fixed full-body "
                "contract and the stable actual-binary provider-free preflight fingerprint"
            ),
        },
        "provenance": {
            "fx": {
                **fx_provenance,
                "source_unchanged_during_run": fx_unchanged,
                "execution_copy": {
                    "sha256": fx_provenance["binary"]["sha256"],
                    "mode": "0500",
                    "digest_rechecked_before_each_client": True,
                    "unchanged_after_run": copy_integrity["fx"],
                },
            },
            "nanocodex": {
                **nano_provenance,
                "source_unchanged_during_run": nano_unchanged,
                "execution_copy": {
                    "sha256": nano_provenance["binary"]["sha256"],
                    "mode": "0500",
                    "digest_rechecked_before_each_client": True,
                    "unchanged_after_run": copy_integrity["nanocodex"],
                },
            },
            "child_workspace_policy": "fresh_private_empty_workspace_per_run",
            "upstream_url": DEFAULT_UPSTREAM_URL,
            "transport": "https",
            "model": MODEL,
            "reasoning_effort": EFFORT,
            "service_tier": "standard",
            "fast_mode": False,
            "tool_names_hash": "sha256(sorted compact JSON names)",
            "request_hash": "sha256(raw forwarded JSON bytes)",
            "stable_request_fingerprints": request_fingerprints,
            "offline_request_template_fingerprints": request_templates,
            "independent_reference_body_fingerprints": dict(
                REFERENCE_BODY_FINGERPRINTS
            ),
            "stable_request_fingerprint_method": (
                "sha256(validated canonical body plus allowlisted product metadata "
                "headers, with only validated fresh session/item identities and exact "
                "private home/workspace paths normalized and the separately validated "
                "credential-derived FedRAMP header excluded)"
            ),
            "shared_auth_identity_verified": shared_auth_verified,
            "actual_binary_offline_preflight": offline_preflight,
            "child_egress_policy": (
                "Darwin Seatbelt permits the product process only AF_INET TCP to a "
                "unique host-local port; the observer reserves that port on every "
                "IPv4 interface and accepts requests only when their local destination "
                "is 127.0.0.1; IPv6, UDP, and child-process creation are denied; only "
                "the parent observer can reach the fixed official HTTPS endpoint"
            ),
            "observer_shutdown_policy": (
                "selector-owned child pipes, process groups, downstream handlers, and "
                "upstream sockets must be terminated, reaped, and drained to bounded "
                "deadlines before a successful report can be returned or published"
            ),
        },
        "auth": auth.safe_metadata(),
        "workload": {
            "instructions": SYSTEM_INSTRUCTIONS,
            "prompt": ACK_TOKEN,
            "expected": ACK_TOKEN,
            "instructions_fnv1a64": fnv1a64(SYSTEM_INSTRUCTIONS),
            "prompt_fnv1a64": fnv1a64(ACK_TOKEN),
            "nanocodex_prompt_cache_key": NANOCODEX_PROMPT_CACHE_KEY,
            "nanocodex_prompt_cache_key_fnv1a64": fnv1a64(
                NANOCODEX_PROMPT_CACHE_KEY
            ),
        },
        "schedule": {
            "kind": "alternating_sequential_fresh_clients",
            "alternation_scope": "continuous_across_warmup_and_measured_pairs",
            "odd_schedule_pairs": ["fx", "nanocodex"],
            "even_schedule_pairs": ["nanocodex", "fx"],
            "warmup_pairs_requested": args.warmup_pairs,
            "warmup_pairs_discarded": warmup_pairs_completed,
            "measured_pairs_requested": args.trials,
            "measured_pairs_started": len(measured_pairs),
            "measured_pairs_completed": measured_pairs_completed,
            "provider_request_budget": args.provider_request_budget,
            "stop_policy": "first_functional_failure",
        },
        "warmup": {
            "timings_discarded": True,
            "failures": warmup_failures,
        },
        "observer": observer_stats,
        "score": {
            "name": "provider_time_to_first_model_output",
            "metric": "time_to_first_model_output",
            "unit": "nanoseconds",
            "clock_start": "full_upstream_request_flushed",
            "clock_end": "first_nonempty_semantic_sse_delta_received",
            "lower_is_better": True,
            "eligibility": "warm_primary_pairs_only",
            "result": summary["primary"]["metrics"][
                "time_to_first_model_output"
            ],
        },
        "diagnostics": {
            "time_to_first_answer_text": summary["primary"]["metrics"][
                "time_to_first_answer_text"
            ],
            "time_to_terminal": summary["primary"]["metrics"][
                "time_to_terminal"
            ],
        },
        "summary": {
            "measured_failures": measured_failures,
            "percentile_method": "nearest_rank",
            "wins_are": "descriptive_only_not_inferential",
            **summary,
        },
        "exit_conditions": exit_conditions,
        "pairs": measured_pairs,
    }
    return document, exit_code, auth


def _report_string_values(value: Any) -> Iterable[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, Mapping):
        for key, child in value.items():
            if isinstance(key, str):
                yield key
            yield from _report_string_values(child)
    elif isinstance(value, Sequence) and not isinstance(
        value, (str, bytes, bytearray)
    ):
        for child in value:
            yield from _report_string_values(child)


def _json_unescape_string(value: str) -> str | None:
    escaped = (
        value.replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\r", "\\r")
        .replace("\t", "\\t")
        .replace("\b", "\\b")
        .replace("\f", "\\f")
    )
    try:
        decoded = json.loads(f'"{escaped}"')
    except (UnicodeError, json.JSONDecodeError):
        return None
    return decoded if isinstance(decoded, str) else None


def _normalized_string_forms(value: str) -> set[str]:
    forms = {
        value,
        unicodedata.normalize("NFC", value),
        unicodedata.normalize("NFKC", value),
    }
    decoded = _json_unescape_string(value)
    if decoded is not None:
        forms.update(
            {
                decoded,
                unicodedata.normalize("NFC", decoded),
                unicodedata.normalize("NFKC", decoded),
            }
        )
    return forms


def _assert_report_has_no_secrets(
    document: Mapping[str, Any], auth: AuthMaterial
) -> None:
    secret_forms: set[str] = set()
    for secret in auth.secret_values:
        if not secret:
            continue
        secret_forms.update(_normalized_string_forms(secret))
        escaped = json.dumps(secret, ensure_ascii=True)[1:-1]
        secret_forms.update(_normalized_string_forms(escaped))
    for value in _report_string_values(document):
        for candidate in _normalized_string_forms(value):
            if any(secret in candidate for secret in secret_forms):
                raise BenchmarkError("refusing to serialize a credential-bearing report")


def _encoded_report(document: Mapping[str, Any], auth: AuthMaterial) -> bytes:
    _assert_report_has_no_secrets(document, auth)
    encoded = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8")
    lowered = encoded.lower()
    if b'"authorization"' in lowered or b'"chatgpt-account-id"' in lowered:
        raise BenchmarkError("refusing to serialize raw auth headers")
    return encoded


def write_report(path: Path, encoded: bytes) -> None:
    descriptor, temporary = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb", closefd=True) as handle:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
        # Link is an atomic no-clobber publication on the same filesystem. If
        # another process creates the requested path after preflight, fail
        # rather than replacing its file.
        os.link(temporary, path, follow_symlinks=False)
        # The no-clobber link is the publication commit. Failure to remove the
        # now-redundant private inode name must not turn a published successful
        # benchmark into a failed exit status.
        with contextlib.suppress(OSError):
            os.unlink(temporary)
    except BaseException:
        with contextlib.suppress(OSError):
            os.close(descriptor)
        with contextlib.suppress(OSError):
            os.unlink(temporary)
        raise


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a live paid controlled product-as-shipped fx-vs-Nanocodex "
            "latency comparison against the fixed official HTTPS endpoint. "
            "No request is made unless --confirm-paid-live-run is supplied."
        )
    )
    parser.add_argument("--fx-source-root", type=Path, required=True)
    parser.add_argument("--fx-bin", type=Path, required=True)
    parser.add_argument("--nanocodex-source-root", type=Path, required=True)
    parser.add_argument("--nanocodex-bin", type=Path, required=True)
    parser.add_argument("--nanocodex-commit", required=True)
    parser.add_argument("--auth-file", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=20)
    parser.add_argument("--warmup-pairs", type=int, default=2)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--confirm-paid-live-run",
        action="store_true",
        required=True,
        help="explicitly authorize the paid live provider requests",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    try:
        args = parse_args(argv)
        document, exit_code, auth = run_benchmark(args)
        notification = json.dumps(
            {
                "status": document["status"],
                "measured_pairs": document["schedule"][
                    "measured_pairs_completed"
                ],
                "primary_eligible_pairs": document["summary"]["primary"][
                    "eligible_pairs"
                ],
            },
            separators=(",", ":"),
        )
        if exit_code != 0:
            with contextlib.suppress(OSError, ValueError):
                print(notification)
            return exit_code
        write_report(args.output, _encoded_report(document, auth))
        with contextlib.suppress(OSError, ValueError):
            print(notification)
        return exit_code
    except BenchmarkError as error:
        print(f"paired model-latency benchmark refused: {error}", file=sys.stderr)
        return 2
    except OSError:
        print("paired model-latency benchmark failed with a local I/O error", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
