"""The Phase 1 gate: the compiled Rust extension imports and answers.

These tests deliberately do not mock anything. Their whole purpose is to prove
that maturin, abi3, the cdylib link and the wheel install are wired up correctly
while the extension is still small enough for a failure to be obvious.
"""

from __future__ import annotations

import types
from pathlib import Path

import search_core


def test_version_comes_from_the_compiled_extension_not_from_python() -> None:
    # `version` is a built-in function, and its module is the inner compiled
    # module rather than a pure-Python shim that happens to answer.
    assert isinstance(search_core.version, types.BuiltinFunctionType)
    assert search_core.version.__module__ == "search_core.search_core"


def test_the_extension_is_installed_as_an_abi3_shared_object() -> None:
    # maturin installs a package wrapper whose __init__ re-exports the shared
    # object, so the public module is a package. What matters is that the code
    # answering us is a compiled .so.
    package_dir = Path(search_core.__file__).parent  # type: ignore[arg-type]
    shared_objects = sorted(package_dir.glob("*.so")) + sorted(package_dir.glob("*.pyd"))

    assert shared_objects, f"no compiled extension found in {package_dir}"

    # abi3 is what lets one wheel serve CPython 3.12 and every later version, and
    # is the reason the same artifact works on the dev interpreter and inside
    # python:3.12-slim.
    if any(path.suffix == ".so" for path in shared_objects):
        assert any("abi3" in path.name for path in shared_objects), (
            f"expected an abi3-tagged shared object, found {[p.name for p in shared_objects]}"
        )


def test_version_is_reported_consistently_three_ways() -> None:
    from api import __version__

    assert search_core.version() == search_core.__version__
    assert search_core.version() == __version__
