#!/usr/bin/env bash
#
# Crawls the official sites from a generated seed list, in slices, draining the
# spool between them.
#
# Why slices rather than one long crawl: the growth page reports what became
# searchable per minute, and that timeline only has more than one point if the
# indexer commits more than once. Draining between slices is what produces a real
# curve instead of a single bar -- and it is also the honest shape of how this
# system is run, since a crawl and a drain are separate processes on purpose.
#
# Why a seed list rather than hand-written groups: the list is built from each
# site's own sitemap (scripts/make_seeds.py), spread evenly across every site, and
# it grows to whatever `--limit` says. Hand-written groups cannot do that, and the
# two drifted apart the moment the list existed.
#
# Politeness is one second per host, the crawler names itself, and `--stay-on-site`
# keeps the crawl on the sites it was pointed at. Where a site refuses this bot,
# the refusal is counted rather than talked around.
#
# Usage: scripts/crawl_official.sh [pages-per-slice] [slices] [--fresh] [--loop [seconds]]
#
#   scripts/crawl_official.sh            # 750 pages per slice, 6 slices, then stop
#   SLICES=10 scripts/crawl_official.sh  # a longer, finer-grained run
#   scripts/crawl_official.sh --fresh    # rebuild the corpus from nothing first
#   scripts/crawl_official.sh --loop 20  # rounds back to back, 20s apart, forever
#   OVERLAP_DRAIN=1 scripts/crawl_official.sh --loop 20
#
# `--loop` is what keeps the growth page honest: the default is one round, which ends,
# and an index that stops growing is what /stats looks like when nobody is crawling.
# Rounds accumulate rather than repeat -- the frontier and the seen-set persist -- and
# each round is bounded by its slice budget, so looping is a schedule rather than a way
# to crawl without limit. The systemd crawl unit gets the same effect from
# Restart=always; this is the same thing for a checkout that runs by hand.
#
# `--fresh` exists because a partial wipe is worse than no wipe: the frontier lives in
# RocksDB but the seen-set lives in the bloom filter, so removing one and not the other
# leaves a crawler that believes every URL has already been fetched. It then exits zero
# having done nothing, and the crawl looks like a success. This removes all of it --
# frontier, bloom, index, spool and the derived JSON -- and keeps the WARC archive,
# which is the record of what was fetched rather than crawl state.

set -euo pipefail

cd "$(dirname "$0")/.."

FRESH=0
LOOP=0
LOOP_SECONDS="${LOOP_SECONDS:-20}"
positional=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --fresh) FRESH=1; shift ;;
    --loop)
      LOOP=1; shift
      # An optional argument: `--loop` alone means the default, `--loop 60` means 60.
      case "${1:-}" in
        ''|*[!0-9]*) : ;;
        *) LOOP_SECONDS="$1"; shift ;;
      esac
      ;;
    *) positional+=("$1"); shift ;;
  esac
done

PAGES_PER_SLICE="${positional[0]:-${PAGES_PER_SLICE:-750}}"
SLICES="${positional[1]:-${SLICES:-6}}"
OVERLAP_DRAIN="${OVERLAP_DRAIN:-0}"

CRAWLER="./target/release/crawler"
INDEXER="./target/release/indexer"
UA="MJLsearchBot/0.1 (+https://example.invalid/bot)"
SEEDS="seeds/official.txt"

for binary in "$CRAWLER" "$INDEXER"; do
  if [ ! -x "$binary" ]; then
    echo "missing $binary -- run ./scripts/build.sh first" >&2
    exit 1
  fi
done

if [ "$FRESH" -eq 1 ]; then
  for state in data/rocksdb data/urls.bloom data/urls.bloom.tmp data/index data/spool; do
    rm -rf "$state"
  done
  rm -f data/index-history.json data/host-authority.json data/crawl-stats.json
  echo "==> fresh corpus: frontier, bloom, index, spool and derived JSON removed"
  echo "    (kept data/warc -- that is the archive, not crawl state)"
fi

if [ ! -f "$SEEDS" ] || [ "$(grep -cv '^#' "$SEEDS" || true)" -eq 0 ]; then
  echo "==> no seed list at $SEEDS; generating one"
  uv run python scripts/make_seeds.py --limit 3000
fi

# The list is split without its comments, so a slice is nothing but URLs.
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
grep -v '^#' "$SEEDS" | grep -v '^$' > "$WORKDIR/all.txt"
total="$(wc -l < "$WORKDIR/all.txt")"
echo "==> $total seeds in $SEEDS, ${SLICES} slices of up to ${PAGES_PER_SLICE} pages"
split -n "l/$SLICES" -d -a 2 "$WORKDIR/all.txt" "$WORKDIR/slice-"

# `--allow-host` names the subdomains that are separate sites with their own
# robots.txt; the seed hosts themselves are added to the scope automatically.
# It sits above drain_slice on purpose: the comment belongs to crawl_slice and
# stayed here when the drain helper was introduced.
# Drains the spool left by a slice. Kept as its own function so the overlap
# mode can call it asynchronously while the next slice is being fetched.
drain_slice() {
  local label="$1"
  echo "=== $(date -u +%H:%M:%S) drain slice ${label} ==="
  RUST_LOG=info "$INDEXER" --once || echo "indexer exited non-zero after slice ${label}"
}

crawl_slice() {
  local label="$1" file="$2"
  echo "=== $(date -u +%H:%M:%S) crawl slice ${label}: $(wc -l < "$file") seeds, max ${PAGES_PER_SLICE} pages ==="
  # The crawler's own log is shown, not just the indexer's. Its closing summary and
  # its warnings are the only report of what a slice did, and a slice that queues
  # nothing is otherwise a run that succeeds while doing no work.
  RUST_LOG="${CRAWLER_LOG:-info}" "$CRAWLER" \
    --seed-file "$file" \
    --stay-on-site \
    --max-depth 3 \
    --workers 24 \
    --per-host-concurrency 2 \
    --politeness-delay-ms 1000 \
    --max-pages "$PAGES_PER_SLICE" \
    --user-agent "$UA" \
    || echo "crawler exited non-zero for slice ${label}"

  # Non-overlap keeps the original, sequential shape: crawl, drain, crawl.
  # This is the safe default and the mode a cold index should start in.
  if [ "$OVERLAP_DRAIN" -eq 0 ]; then
    drain_slice "$label"
  fi
}

# Waits for a previously backgrounded drain and reports its status. The file
# descriptor exists so the parent does not accidentally inherit the drain's
# stdout and hold the terminal open.
wait_for_drain() {
  local pid_file="$1" label="$2"
  if [ -f "$pid_file" ]; then
    local pid
    pid="$(cat "$pid_file")"
    if kill -0 "$pid" 2>/dev/null; then
      echo "=== $(date -u +%H:%M:%S) waiting for overlap drain from slice ${label} ==="
      wait "$pid" || echo "overlap indexer exited non-zero after slice ${label}"
    fi
    rm -f "$pid_file"
  fi
}

round=0
last_pid_file=""
while :; do
  round=$((round + 1))
  if [ "$LOOP" -eq 1 ]; then
    echo "=== $(date -u +%H:%M:%S) round ${round} ==="
  fi

  for file in "$WORKDIR"/slice-*; do
    label="$(basename "$file" | sed 's/^slice-//')"

    # Finish any drain left running from the previous slice before this slice
    # starts. The drain is a bounded local job, so this normally returns
    # immediately; when it does not, the crawl waits rather than queueing two
    # index writers, which Tantivy forbids.
    if [ -n "$last_pid_file" ]; then
      wait_for_drain "$last_pid_file" "$label"
      last_pid_file=""
    fi

    crawl_slice "$label" "$file"

    if [ "$OVERLAP_DRAIN" -eq 1 ]; then
      # Drains the slice that just finished while the next slice is fetched.
      # The crawler and indexer touch different directories while running, and
      # the next drain still waits for this one, so only one index writer is
      # ever active.
      drain_slice "$label" &
      drain_pid=$!
      last_pid_file="$WORKDIR/drain-${label}.pid"
      printf "%s\n" "$drain_pid" >"$last_pid_file"
    fi
  done

  # Never leave a background drain behind when the round ends.
  if [ -n "$last_pid_file" ]; then
    wait_for_drain "$last_pid_file" "final"
    last_pid_file=""
  fi

  echo "=== $(date -u +%H:%M:%S) round ${round} done ==="
  echo "documents in the index:"
  RUST_LOG=info "$INDEXER" --once 2>&1 | tail -1

  [ "$LOOP" -eq 1 ] || break
  echo "=== $(date -u +%H:%M:%S) next round in ${LOOP_SECONDS}s (Ctrl-C to stop) ==="
  sleep "$LOOP_SECONDS"
done

ls -l data/index-history.json data/host-authority.json data/crawl-stats.json 2>&1
