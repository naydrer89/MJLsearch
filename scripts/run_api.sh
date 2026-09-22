#!/usr/bin/env bash
#
# Starts the API.
#
# Workers are bounded on purpose. Each worker opens the same read-only index and
# mmaps it; the OS page cache shares those pages across processes, so extra
# workers add CPU contention without buying memory headroom. Two to four is the
# useful range.
#
# SEARCH_MAX_PER_DOMAIN is left at its default of two, which is the diversity setting
# rather than an oversight: it is what stops one host from filling a page, and on a
# corpus of forty sites the top result for `github` should be github.com and the next
# one something else. Raise it for a corpus that shares a single host -- the local
# smoke-test site in the run doc -- where the cap would otherwise leave the page with
# two results however many matched:
#
#   SEARCH_MAX_PER_DOMAIN=10 ./scripts/run_api.sh
#
# It is set here rather than in run_searchui.sh because this is the process that
# enforces it: the cap is applied while ranking, inside the API, and the page passes no
# such parameter. Exporting it from the page looked like it worked and did nothing.
#
set -euo pipefail

cd "$(dirname "$0")/.."

export SEARCH_HOST="${SEARCH_HOST:-127.0.0.1}"
export SEARCH_PORT="${SEARCH_PORT:-8000}"
workers="${SEARCH_WORKERS:-2}"

exec uv run uvicorn api.main:app \
    --host "$SEARCH_HOST" \
    --port "$SEARCH_PORT" \
    --workers "$workers"
