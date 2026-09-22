#!/usr/bin/env bash
#
# Builds everything optimised.
#
# Two traps this script exists to avoid:
#
# 1. `uv sync` alone will often not rebuild the extension at all. uv keys its
#    cached wheel on the project version, and the Rust version has not changed,
#    so a source-only edit is invisible to it and you keep running yesterday's
#    code. --reinstall-package search-core forces the rebuild.
#
# 2. The optimised profile is configured in crates/search-core/pyproject.toml
#    ([tool.maturin] profile = "release"), not passed as a flag. The flag route
#    does not work: MATURIN_PEP517_ARGS="--release" is rejected by maturin's
#    pep517 subcommand, and the failure mode is a debug extension that is correct
#    but several times slower.
#
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> building the search_core extension (release)"
uv sync --reinstall-package search-core

echo "==> building the crawler and indexer binaries (release)"
cargo build --release --bin crawler --bin indexer

echo
echo "built:"
ls -1 target/release/crawler target/release/indexer 2>/dev/null || true
uv run python -c "import search_core; print('search_core', search_core.version())"
