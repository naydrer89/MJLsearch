#!/usr/bin/env bash
#
# Starts the dashboard on port 5000.
#
# A separate process from the API, and that is the point: it reads the API over
# HTTP, so it comes up (and explains the outage) when the API does not. It also
# needs no index handle, no mmap and no Tantivy.
#
# SEARCH_BACKEND_URL points it at the API. Set it when the API is not on the
# default 127.0.0.1:8000, e.g. when both run on different hosts.
#
set -euo pipefail

cd "$(dirname "$0")/.."

export SEARCH_BACKEND_URL="${SEARCH_BACKEND_URL:-http://127.0.0.1:${SEARCH_PORT:-8000}}"
port="${SEARCH_DASHBOARD_PORT:-5000}"
host="${SEARCH_HOST:-127.0.0.1}"

if [ "${SEARCH_STARTUP_SOUND:-1}" != "0" ]; then
    # Played here rather than only in the API, so starting the dashboard alone is
    # still audible. api/sound.py stamps the announcement and suppresses a second
    # chime within a few seconds, so starting both scripts together beeps once.
    uv run python -m api.sound || true
fi

echo "dashboard reading ${SEARCH_BACKEND_URL} on http://${host}:${port}"

# Flask's development server, with threading so one slow upstream call cannot
# block the poll that already started. For anything longer-lived, run the same
# app under a real WSGI server: `uv run gunicorn 'dashboard.app:create_app()'`.
exec uv run flask --app dashboard.app run \
    --host "$host" \
    --port "$port" \
    --with-threads
