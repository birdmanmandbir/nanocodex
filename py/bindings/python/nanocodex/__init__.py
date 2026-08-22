"""Embedded Python bindings for the Nanocodex agents SDK."""

from importlib import import_module


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
_TYPE_EXPORTS = frozenset({"EstimatedCost", "Usage"})

# This constant is release-checked against the workspace version. Keeping it in
# the lightweight Python module avoids loading the native extension or scanning
# distribution metadata merely to answer `nanocodex.__version__`.
__version__ = "0.5.0"


def __getattr__(name: str) -> object:
    if name in _NATIVE_EXPORTS:
        module_name = "._native"
        exports = _NATIVE_EXPORTS
    elif name in _TYPE_EXPORTS:
        module_name = "._types"
        exports = _TYPE_EXPORTS
    else:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    module = import_module(module_name, __name__)
    for export in exports:
        globals()[export] = getattr(module, export)
    return globals()[name]


def __dir__() -> list[str]:
    return sorted((*globals(), *_NATIVE_EXPORTS, *_TYPE_EXPORTS))


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
