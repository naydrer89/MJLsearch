#!/usr/bin/env bash
#
# Runs the search page on http://127.0.0.1:5001
#
# Needs the API to be up first: this process renders results by asking it, and
# deliberately holds no index handle of its own.
#
#   ./scripts/run_api.sh      # in another terminal, or backgrounded
#   ./scripts/run_searchui.sh
#
# Nothing is configured here beyond the port. In particular the per-site result cap is
# *not* set from this process: it is enforced while ranking, inside the API, so it
# belongs to run_api.sh. Setting it here looks like it works and does nothing -- this
# process passes no such parameter and reads none -- which is how the two scripts ended
# up disagreeing about a number that neither of them displays.
#
# The port needs translating, and this is the second instance of the same mistake, so
# it is worth being explicit: the page reads its port from the settings object, whose
# prefix turns the field `search_port` into `SEARCH_SEARCH_PORT`. A plain `SEARCH_PORT`
# means the *API* port -- it is what run_api.sh binds -- so the page ignored it, and a
# page told to listen on 5010 printed that it was on 5010 while binding 5001. The name
# is translated here, once, so both the echoed line and the socket come from the same
# variable.

set -euo pipefail

cd "$(dirname "$0")/.."

# SEARCH_PORT is accepted as a fallback so that a single variable still moves a
# single-host install: start.sh passes SEARCH_UI_PORT, and a bare
# `SEARCH_PORT=5010 ./scripts/run_searchui.sh` does what it looks like it does.
port="${SEARCH_UI_PORT:-${SEARCH_PORT:-5001}}"
host="${SEARCH_HOST:-127.0.0.1}"
export SEARCH_SEARCH_PORT="$port"

echo "==> search page on http://${host}:${port}"
exec uv run python -m searchui.app
