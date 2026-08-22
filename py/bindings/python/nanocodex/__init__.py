"""Embedded Python bindings for the Nanocodex agents SDK."""

from importlib import import_module
from importlib.metadata import version as distribution_version
from typing import TypedDict


_NATIVE_EXPORTS = frozenset(
    {
        "AgentEvent",
        "AgentEvents",
        "Nanocodex",
        "SessionSnapshot",
        "Turn",
        "TurnResult",
    }
)

# Reading installed package metadata keeps `import nanocodex` cheap. The native
# extension is loaded on the first agent API access and every export is cached
# in this module, so normal `from nanocodex import Nanocodex` use is unchanged.
__version__ = distribution_version("nanocodex")


def __getattr__(name: str) -> object:
    if name not in _NATIVE_EXPORTS:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    native = import_module("._native", __name__)
    for export in _NATIVE_EXPORTS:
        globals()[export] = getattr(native, export)
    return globals()[name]


def __dir__() -> list[str]:
    return sorted((*globals(), *_NATIVE_EXPORTS))


class EstimatedCost(TypedDict):
    usd: str
    input_usd: str
    cached_input_usd: str
    cache_write_input_usd: str
    output_usd: str
    service_tier: str


class Usage(TypedDict):
    input_tokens: int
    cached_input_tokens: int
    cache_write_input_tokens: int
    output_tokens: int
    reasoning_output_tokens: int
    total_tokens: int
    estimated_cost: EstimatedCost | None
    cost_status: str


__all__ = [
    "AgentEvent",
    "AgentEvents",
    "EstimatedCost",
    "Nanocodex",
    "SessionSnapshot",
    "Turn",
    "TurnResult",
    "Usage",
    "__version__",
]
