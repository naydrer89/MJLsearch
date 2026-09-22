#!/usr/bin/env bash
#
# Starts a crawl. Long-running and standalone: it is never driven through the API.
#
#   scripts/run_crawler.sh https://example.com https://example.org
#
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <seed-url> [seed-url ...]" >&2
    exit 2
fi

exec cargo run --release --bin crawler -- "$@"
