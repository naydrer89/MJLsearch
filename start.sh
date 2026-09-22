#!/usr/bin/env bash
#
# One command from a fresh checkout to a running search engine.
#
#   ./start.sh                  # install, build, seed if the index is empty, start
#   ./start.sh --crawl          # start a background crawl even if there is data
#   ./start.sh --no-crawl       # start nothing that crawls
#   ./start.sh --foreground     # keep the API in this terminal instead of detaching
#   ./start.sh --no-install     # never install a toolchain, only use what is there
#   ./start.sh --help
#
# Idempotent on purpose: a second run reuses whatever is already answering its port
# and starts only the parts that are missing, so it is safe to re-run after editing a
# page. Nothing is killed that this script did not start.
#
# It installs what it can without root -- the Rust toolchain through rustup and `uv`
# through its own installer, both into the current user's home -- because "run this
# one command" is the promise, and a script that stops at the first missing tool keeps
# that promise only on the machine it was written on. `--no-install` turns it off.
#
# It still does *not* install system packages: a C/C++ compiler cannot be installed
# into a home directory, and a start script that reaches for sudo is one that asks for
# a password on a machine it does not own. If the compiler is missing it names the
# package, and on an Ubuntu server `sudo ./deploy/setup-server.sh` does the whole job
# including apt, the service user and the systemd units.

set -euo pipefail

cd "$(dirname "$0")"

LOGS="logs"
CRAWL=auto
FOREGROUND=0
INSTALL=auto

for arg in "$@"; do
  case "$arg" in
    --crawl) CRAWL=yes ;;
    --no-crawl) CRAWL=no ;;
    --foreground) FOREGROUND=1 ;;
    --install) INSTALL=yes ;;
    --no-install) INSTALL=no ;;
    -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

step() { printf '\n==> %s\n' "$1"; }
note() { printf '    %s\n' "$1"; }

have() { command -v "$1" >/dev/null 2>&1; }

# Whether something already holds a port. Both probes are attempted because neither is
# portable on its own: `ss` is Linux, `lsof` is macOS.
port_busy() {
  if have ss; then ss -ltn 2>/dev/null | grep -q ":$1 "; else lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1; fi
}

# Waits until a URL answers, rather than sleeping a guessed number of seconds. The
# pages are useless before the API is up, so the API is waited on first.
wait_for() {
  local url="$1" label="$2" limit="${3:-30}" waited=0
  while [ "$waited" -lt "$limit" ]; do
    if curl -fsS -m 2 -o /dev/null "$url" 2>/dev/null; then
      note "$label is up at $url"
      return 0
    fi
    waited=$((waited + 1))
    sleep 1
  done
  note "$label did not answer at $url within ${limit}s — see $LOGS/"
  return 1
}

# Starts a command in its own session so it outlives this shell, logging to `logs/`.
detach() {
  local name="$1"
  shift
  mkdir -p "$LOGS"
  setsid "$@" >"$LOGS/$name.log" 2>&1 </dev/null &
  echo $!
}

step "checking prerequisites"

# The prerequisites this script cannot provide: building the crawler compiles the
# RocksDB C++ sources the crate bundles and runs rust-bindgen over its headers, so there
# has to be a C++ compiler, `make`, and libclang. Checked first because they are the
# failures that no amount of installing into a home directory can fix, and the message
# names the package rather than the symptom.
if have cc || have gcc || have clang; then
  note "C compiler $(command -v cc || command -v gcc || command -v clang)"
else
  note "MISSING: a C/C++ compiler -- the crawler links a RocksDB built from source"
  note "on Ubuntu/Debian: sudo ./deploy/setup-server.sh, or sudo apt-get install -y build-essential"
  exit 1
fi

# libclang is what rust-bindgen loads to generate the RocksDB bindings. A machine can
# easily have a compiler and not this, and the failure it produces names bindgen rather
# than the missing package. A warning rather than an exit: clang-sys can find a libclang
# in places this check does not look, and refusing to start on a machine that can build
# would be worse than the confusing error it avoids.
if [ "$(uname -s)" != "Darwin" ] \
  && ! have llvm-config \
  && ! ls /usr/lib/llvm-*/lib/libclang.so* >/dev/null 2>&1 \
  && ! ls /usr/lib/*/libclang.so* >/dev/null 2>&1 \
  && ! { have ldconfig && ldconfig -p 2>/dev/null | grep -q libclang; }; then
  note "WARNING: no libclang found -- rust-bindgen needs it to build the crawler"
  note "on Ubuntu/Debian: sudo apt-get install -y libclang-dev clang"
fi

# Both installers are the projects' own official ones, and both install into $HOME, so
# neither needs root. A toolchain that was already there is never touched: the check is
# "is the command on PATH", not "is it the pinned version".
install_cargo() {
  if ! have curl; then
    note "curl is missing as well, so rustup cannot be fetched (apt-get install curl)"
    return 1
  fi
  note "installing the Rust toolchain with rustup -- minimal profile, into ~/.cargo"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
  export PATH="$HOME/.cargo/bin:$PATH"
}

install_uv() {
  if ! have curl; then
    note "curl is missing as well, so uv cannot be fetched (apt-get install curl)"
    return 1
  fi
  note "installing uv with its own installer, into ~/.local/bin"
  curl -LsSf https://astral.sh/uv/install.sh | sh
  export PATH="$HOME/.local/bin:$PATH"
}

for tool in cargo uv; do
  if have "$tool"; then
    note "$tool $(command -v "$tool")"
    continue
  fi
  if [ "$INSTALL" = "no" ]; then
    note "MISSING: $tool -- and --no-install was given"
    note "install Rust from https://rustup.rs and uv from https://docs.astral.sh/uv/"
    exit 1
  fi
  if [ "$tool" = "cargo" ]; then install_cargo || exit 1; else install_uv || exit 1; fi
  if have "$tool"; then
    note "$tool $(command -v "$tool")"
  else
    note "$tool still not on PATH after installing -- open a new shell and re-run"
    exit 1
  fi
done

# A stale toolchain is the same failure as a missing one, one step later: edition 2024
# needs 1.85, and the error it produces otherwise points at the manifest rather than at
# the toolchain. rustup can fix it in place; a system package cannot, so that case is
# reported instead of guessed at.
if [ "$(cargo --version | awk '{ split($2, v, "."); print (v[1] < 1 || (v[1] == 1 && v[2] < 85)) ? 1 : 0 }')" = "1" ]; then
  note "$(cargo --version) is older than this workspace needs (1.85, edition 2024)"
  if have rustup && [ "$INSTALL" != "no" ]; then
    note "updating the stable toolchain with rustup"
    rustup update stable
    rustup default stable
  else
    note "update it with: rustup update stable"
    exit 1
  fi
fi

step "installing and building (release)"
note "uv sync rebuilds the search_core extension; cargo builds the crawler and indexer"
./scripts/build.sh

step "seed list"
if [ ! -s seeds/official.txt ]; then
  note "none yet — building one from the official sites' own sitemaps"
  uv run python scripts/make_seeds.py --limit "${SEED_LIMIT:-3000}"
else
  note "$(grep -cv '^#' seeds/official.txt || true) URLs in seeds/official.txt"
fi

documents=0
if [ -f data/index-history.json ]; then
  documents="$(uv run python -c "
import json, pathlib
try:
    print(int(json.loads(pathlib.Path('data/index-history.json').read_text())['total_documents']))
except Exception:
    print(0)
")"
fi
note "documents in the index: $documents"

# A crawl is started when there is nothing to search, because every page is otherwise
# an empty state; with an index already in place it is left to the flag, since crawling
# is the one thing here that leaves the machine rather than reading a local directory.
#
# It loops, rather than doing a single round: one round ends, and an index that stops
# growing is a growth page with a flat line and a stale number -- which is exactly what
# "the crawler is active" is supposed to rule out. CRAWL_LOOP_SECONDS is the gap between
# rounds; set it to 0 to run them back to back.
if [ "$CRAWL" = "yes" ] || { [ "$CRAWL" = "auto" ] && [ "$documents" -eq 0 ]; }; then
  step "crawl in the background"
  note "rounds back to back, draining between slices, so /stats keeps a live curve"
  crawl_pid="$(detach crawl env "PAGES_PER_SLICE=${PAGES_PER_SLICE:-500}" "SLICES=${SLICES:-6}" \
    bash scripts/crawl_official.sh --loop "${CRAWL_LOOP_SECONDS:-20}")"
  note "pid $crawl_pid — progress in $LOGS/crawl.log and data/crawl-stats.json"
  note "stop it with: kill $crawl_pid"
else
  step "crawl"
  note "not started (--crawl to force one); the index will not grow until it runs"
fi

step "query API"
api_port="${SEARCH_PORT:-8000}"
if port_busy "$api_port"; then
  note "port $api_port is already in use — leaving it alone"
else
  api_pid="$(detach api ./scripts/run_api.sh)"
  note "pid $api_pid"
fi
wait_for "http://127.0.0.1:$api_port/health" "API" 40 || exit 1

step "search page"
page_port="${SEARCH_UI_PORT:-5001}"
if port_busy "$page_port"; then
  note "port $page_port is already in use — leaving it alone"
else
  # SEARCH_UI_PORT, not SEARCH_PORT: the page reads its port from the settings object
  # (`SEARCH_SEARCH_PORT`), so the API's variable name left the page on 5001 whatever
  # this script asked for. run_searchui.sh does the translation now.
  page_pid="$(detach searchui env SEARCH_UI_PORT="$page_port" ./scripts/run_searchui.sh)"
  note "pid $page_pid"
fi
wait_for "http://127.0.0.1:$page_port/healthz" "search page" 40 || exit 1

# The dashboard is optional in the sense that the search engine works without it, but
# it is the only view of the crawler and the spool, so it is started unless its port is
# taken. It reads the API over HTTP and holds no index handle of its own.
step "operator dashboard"
dash_port="${SEARCH_DASHBOARD_PORT:-5000}"
if port_busy "$dash_port"; then
  note "port $dash_port is already in use — leaving it alone"
else
  dash_pid="$(detach dashboard env SEARCH_DASHBOARD_PORT="$dash_port" ./scripts/run_dashboard.sh)"
  note "pid $dash_pid"
  wait_for "http://127.0.0.1:$dash_port/" "dashboard" 20 || note "continuing without it"
fi

step "ready"
cat <<EOF
    search    http://127.0.0.1:${page_port}/
    growth    http://127.0.0.1:${page_port}/stats
    API       http://127.0.0.1:${api_port}/docs
    monitor   http://127.0.0.1:${dash_port}/
    logs      $LOGS/*.log

    stop everything this script started:  pkill -f 'scripts/run_(api|searchui|dashboard)'

    on an Ubuntu server, run this as a service instead (autostart, restart on crash):
      sudo ./deploy/setup-server.sh
EOF

if [ "$FOREGROUND" = "1" ]; then
  step "foreground"
  note "Ctrl-C stops the API; the pages keep running"
  exec ./scripts/run_api.sh
fi
