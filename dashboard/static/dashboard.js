/* The dashboard's client side.
 *
 * No framework and no build step: one page of DOM updates against two JSON
 * routes. A tool whose job is to watch the server should not need a toolchain of
 * its own.
 *
 * Three things here are deliberate rather than incidental:
 *
 * 1. One poll, six panels. The page asks for everything in a single request so
 *    that watching the server does not become a load on the server.
 * 2. Nothing is assigned with innerHTML except search snippets, which arrive from
 *    the index as text with <b> around the matched terms. Those go through an
 *    escape-then-restore path; everything else uses textContent.
 * 3. Every animation is a flourish on top of correct output. The numbers are
 *    formatted and applied first, and the animation only moves them into place,
 *    so a browser that refuses to animate (or a user who asked it not to) still
 *    gets the finished page rather than a frozen one.
 */

'use strict';

// The formatters follow the language the page declares, not the visitor's locale.
// Passing `undefined` means "whatever the browser is set to", which mixed German
// relative times into an otherwise English interface -- ``Stopped vor 2 Minuten``
// -- because every label here is written in English. Taking the locale from
// ``<html lang>`` keeps the page internally consistent today and correct if the
// template is ever translated.
const LOCALE = document.documentElement.lang || 'en';

const NUMBER = new Intl.NumberFormat(LOCALE);
const RELATIVE_TIME = new Intl.RelativeTimeFormat(LOCALE, { numeric: 'auto' });
const ABSOLUTE_TIME = new Intl.DateTimeFormat(LOCALE, { hour: '2-digit', minute: '2-digit' });
const ABSOLUTE_DATE = new Intl.DateTimeFormat(LOCALE, { dateStyle: 'medium', timeStyle: 'short' });
const NON_BREAKING = '\u00a0';

const body = document.body;
const POLL_MS = Number(body.dataset.pollMs) || 2000;

const prefersReducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

const state = {
  paused: false,
  sound: false,
  audio: null,
  failures: 0,
  lastQueryAt: null,
  lastBackendOk: null,
  resetArmed: false,
  resetTimer: null,
};

const $ = (id) => document.getElementById(id);

/* ---------- formatting ---------- */

const count = (value) => (typeof value === 'number' ? NUMBER.format(value) : '—');

function bytes(value) {
  if (typeof value !== 'number') return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let size = value;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  const digits = size < 10 && unit > 0 ? 1 : 0;
  return `${size.toFixed(digits)}${NON_BREAKING}${units[unit]}`;
}

function duration(seconds) {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds)) return '—';
  if (seconds < 60) return `${seconds.toFixed(0)}${NON_BREAKING}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}${NON_BREAKING}min ${seconds % 60 < 10 ? '0' : ''}${(seconds % 60).toFixed(0)}${NON_BREAKING}s`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}${NON_BREAKING}h ${Math.floor((seconds % 3600) / 60)}${NON_BREAKING}min`;
  return `${Math.floor(seconds / 86400)}${NON_BREAKING}d`;
}

function ago(seconds) {
  if (typeof seconds !== 'number' || !Number.isFinite(seconds)) return 'unknown';
  if (seconds < 60) return RELATIVE_TIME.format(-Math.round(seconds), 'second');
  if (seconds < 3600) return RELATIVE_TIME.format(-Math.round(seconds / 60), 'minute');
  if (seconds < 86400) return RELATIVE_TIME.format(-Math.round(seconds / 3600), 'hour');
  return RELATIVE_TIME.format(-Math.round(seconds / 86400), 'day');
}

const ms = (value) =>
  typeof value === 'number' ? `${value >= 100 ? value.toFixed(0) : value.toFixed(1)}` : '—';

function setText(id, value) {
  const node = $(id);
  if (node) node.textContent = value;
}

/* ---------- motion ---------- */

/* Count-up, used only on the two numbers that answer "how much is in here".
 * Everything else would be noise, and the design has one signature, not seven. */
function animateCount(element, target) {
  const from = Number(element.dataset.countValue || 0);
  element.dataset.countValue = String(target);

  if (prefersReducedMotion || from === target) {
    element.textContent = count(target);
    return;
  }

  const started = performance.now();
  const span = Math.max(1, Math.abs(target - from));
  const step = (now) => {
    const progress = Math.min(1, (now - started) / 320);
    // Ease-out cubic: fast at the start, settling rather than stopping.
    const eased = 1 - (1 - progress) ** 3;
    element.textContent = count(Math.round(from + (target - from) * eased));
    if (progress < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

function chime(notes = [587.33, 880]) {
  if (!state.sound) return;
  try {
    const Context = window.AudioContext || window.webkitAudioContext;
    if (!Context) return;
    if (!state.audio) state.audio = new Context();
    if (state.audio.state === 'suspended') state.audio.resume();

    const now = state.audio.currentTime;
    notes.forEach((frequency, index) => {
      const at = now + index * 0.16;
      const oscillator = state.audio.createOscillator();
      const gain = state.audio.createGain();
      oscillator.type = 'sine';
      oscillator.frequency.value = frequency;
      gain.gain.setValueAtTime(0.0001, at);
      gain.gain.exponentialRampToValueAtTime(0.2, at + 0.02);
      gain.gain.exponentialRampToValueAtTime(0.0001, at + 0.3);
      oscillator.connect(gain).connect(state.audio.destination);
      oscillator.start(at);
      oscillator.stop(at + 0.32);
    });
  } catch (error) {
    console.warn('chime failed', error);
  }
}

/* ---------- notice ---------- */

function notice(message, tone = 'info') {
  const node = $('notice');
  if (!message) {
    node.hidden = true;
    return;
  }
  node.textContent = message;
  node.dataset.tone = tone;
  node.hidden = false;
}

/* ---------- chart ---------- */

let chartWidth = 0;

function drawSeries(series) {
  const canvas = $('series');
  const context = canvas.getContext('2d');
  const ratio = window.devicePixelRatio || 1;
  const width = chartWidth || canvas.clientWidth;
  const height = canvas.clientHeight;

  if (canvas.width !== width * ratio || canvas.height !== height * ratio) {
    canvas.width = width * ratio;
    canvas.height = height * ratio;
  }
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  if (!series.length || !width) return;

  const maxCount = Math.max(1, ...series.map((point) => point.count));
  const maxMean = Math.max(1, ...series.map((point) => point.mean_ms));
  const slot = width / series.length;
  const barWidth = Math.max(1.5, slot - 3);
  const baseline = height - 1;

  // A hairline baseline: structure without decoration. The chart is monochrome,
  // so quantity has to be carried by height, and the mean line carries latency by
  // its own scale.
  context.fillStyle = 'rgba(0, 0, 0, 0.18)';
  context.fillRect(0, baseline, width, 1);

  series.forEach((point, index) => {
    const x = index * slot;
    if (point.count === 0) {
      context.fillStyle = 'rgba(0, 0, 0, 0.06)';
      context.fillRect(x, baseline - 2, barWidth, 2);
      return;
    }
    const barHeight = Math.max(2, (point.count / maxCount) * (baseline - 8));
    context.fillStyle = 'rgba(27, 27, 27, 0.88)';
    context.fillRect(x, baseline - barHeight, barWidth, barHeight);
  });

  // The latency line is drawn only across buckets that actually served a query.
  //
  // Plotting it through empty buckets is what produced the spikes this used to
  // draw: a bucket with no queries has no measured latency, so it plotted as zero
  // and the next real bucket shot straight back up, drawing a tall triangle that
  // describes nothing we observed. A run of adjacent measured buckets becomes one
  // polyline, and a lone measured bucket becomes a dot, so a single measurement is
  // visible instead of invisible.
  context.strokeStyle = 'rgba(0, 0, 0, 0.35)';
  context.fillStyle = 'rgba(0, 0, 0, 0.55)';
  context.lineWidth = 1;

  const pointX = (index) => index * slot + barWidth / 2;
  const pointY = (point) => baseline - (point.mean_ms / maxMean) * (baseline - 8);
  let run = [];

  const flush = () => {
    if (run.length === 1) {
      // A single measured bucket gets a stem down to the axis rather than a bare
      // dot: floating in the plot area with no line, a dot reads as a speck, and
      // the stem is what says "this value belongs to this point in time".
      const x = pointX(run[0]);
      const y = pointY(series[run[0]]);
      context.beginPath();
      context.moveTo(x, y);
      context.lineTo(x, baseline);
      context.stroke();
      context.beginPath();
      context.arc(x, y, 1.5, 0, Math.PI * 2);
      context.fill();
    } else if (run.length > 1) {
      context.beginPath();
      run.forEach((index, position) => {
        const x = pointX(index);
        const y = pointY(series[index]);
        if (position === 0) context.moveTo(x, y);
        else context.lineTo(x, y);
      });
      context.stroke();
    }
    run = [];
  };

  series.forEach((point, index) => {
    if (point.count === 0) {
      flush();
      return;
    }
    if (run.length && run[run.length - 1] !== index - 1) flush();
    run.push(index);
  });
  flush();
}

/* ---------- rendering ---------- */

function renderLede(snapshot) {
  const index = snapshot.index;
  const queries = snapshot.queries;

  animateCount($('lede-docs'), index ? index.doc_count : 0);
  setText(
    'lede-meta',
    index
      ? `${bytes(index.index_bytes)} on disk, mapped read-only and shared by every worker`
      : 'No index yet — start the crawler, then the indexer.',
  );

  const total = queries.series.reduce((sum, point) => sum + point.count, 0);
  const busiest = queries.series.reduce((best, point) => (point.count > best.count ? point : best), { count: 0 });
  setText(
    'series-legend',
    total === 0
      ? 'No queries recorded in this window.'
      : `${count(total)} ${total === 1 ? 'query' : 'queries'} in this window · bars are queries per bucket · line is mean latency · p95 ${ms(queries.latency_ms.p95)}${NON_BREAKING}ms`,
  );
  setText('series-window', `last 5 minutes · ${ABSOLUTE_TIME.format(new Date())}`);
}

function renderCards(snapshot) {
  const index = snapshot.index;
  animateCount($('m-docs'), index ? index.doc_count : 0);
  setText('m-index-bytes', index ? `${bytes(index.index_bytes)} on disk` : 'no index yet');
  setText(
    'm-revision',
    index && index.fingerprint != null
      ? `revision ${(index.fingerprint >>> 0).toString(16)} · ${count(index.cache_entries)} cached queries`
      : '—',
  );

  const queries = snapshot.queries;
  animateCount($('m-queries'), queries.total_recorded);
  setText('m-qps', `${queries.queries_per_second.toFixed(2)}${NON_BREAKING}q/s · up ${duration(queries.uptime_seconds)}`);
  setText('m-cache', `${(queries.cache_hit_rate * 100).toFixed(0)}% cache hits · ${count(queries.failed)} failed`);

  setText('m-p50', `${ms(queries.latency_ms.p50)}${NON_BREAKING}ms`);
  setText('m-p95', `p95 ${ms(queries.latency_ms.p95)}${NON_BREAKING}ms · mean ${ms(queries.latency_ms.mean)}${NON_BREAKING}ms`);
  setText('m-worst', `worst ${ms(queries.latency_ms.max)}${NON_BREAKING}ms`);

  const crawler = snapshot.crawler;
  animateCount($('m-indexed'), crawler ? crawler.indexed : 0);
  setText(
    'm-crawl',
    crawler
      ? `${count(crawler.fetched)} fetched · ${count(crawler.queued)} queued · ${count(crawler.failed)} failed`
      : 'The crawler has not run yet.',
  );
  const crawlAge = $('m-crawl-age');
  if (crawler) {
    // Three states, and only one of them is a problem.
    //
    // A stale progress file with an empty queue is a crawl that *finished* -- the
    // normal end state. Treating staleness alone as a warning painted a successful
    // run red, so "the crawler is doing nothing" and "the crawler died with work
    // left" looked identical. They are not: the alarm belongs to the second one.
    const pending = crawler.frontier_pending;
    if (!crawler.stale) {
      crawlAge.textContent =
        `Active: seen ${ago(crawler.age_seconds)}, ` +
        `${count(pending)} URLs across ${count(crawler.hosts)} hosts.`;
      crawlAge.className = 'card__meta';
    } else if (pending > 0) {
      crawlAge.textContent =
        `Stopped ${ago(crawler.age_seconds)} with ` +
        `${count(pending)} URLs still queued across ${count(crawler.hosts)} hosts.`;
      crawlAge.className = 'card__meta card__meta--warn';
    } else {
      crawlAge.textContent =
        `Finished ${ago(crawler.age_seconds)} — queue empty, ` +
        `${count(crawler.hosts)} host(s) crawled.`;
      crawlAge.className = 'card__meta';
    }
  } else {
    crawlAge.textContent = `Watching ${snapshot.spool.path}`;
    crawlAge.className = 'card__meta';
  }

  const spool = snapshot.spool;
  animateCount($('m-sealed'), spool.sealed_segments);
  setText('m-sealed-bytes', `${bytes(spool.sealed_bytes)} awaiting the indexer`);
  setText('m-open', `${count(spool.open_segments)} segment(s) still being written`);

  setText('m-rss', `${(snapshot.process.rss_bytes / 1024 / 1024).toFixed(1)}${NON_BREAKING}MB`);
  setText('m-cpu', `${snapshot.process.cpu_percent.toFixed(1)}% cpu · ${count(snapshot.process.threads)} threads`);
  setText('m-uptime', `started ${ago(snapshot.process.uptime_seconds)}`);
}

function fillTable(tbodyId, rows, buildRow, emptyMessage, columns) {
  const tbody = $(tbodyId);
  tbody.replaceChildren();

  if (!rows.length) {
    const tr = document.createElement('tr');
    tr.className = 'empty';
    const td = document.createElement('td');
    td.colSpan = columns;
    td.className = 'empty';
    td.textContent = emptyMessage;
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }

  for (const row of rows) tbody.appendChild(buildRow(row));
}

function cell(text, className, label) {
  const td = document.createElement('td');
  td.textContent = text;
  if (className) td.className = className;
  // The header a cell belongs to. Narrow screens drop the table layout for a
  // stacked one, and each stack needs its own visible label; `data-label` is what
  // the stylesheet renders, so the two never drift apart.
  if (label) td.dataset.label = label;
  return td;
}

function tag(kind, label) {
  const span = document.createElement('span');
  span.className = `tag${kind ? ` tag--${kind}` : ''}`;
  span.textContent = label;
  return span;
}

function renderTables(snapshot) {
  const queries = snapshot.queries;

  fillTable(
    'table-top',
    queries.top_queries,
    (row) => {
      const tr = document.createElement('tr');
      tr.appendChild(cell(row.query, 'query', 'Query'));
      tr.appendChild(cell(count(row.count), 'num', 'Count'));
      return tr;
    },
    'No queries served yet.',
    2,
  );

  fillTable(
    'table-slowest',
    queries.slowest,
    (row) => {
      const tr = document.createElement('tr');
      tr.appendChild(cell(row.query, 'query', 'Query'));
      tr.appendChild(cell(ms(row.took_ms), 'num', 'ms'));
      tr.appendChild(cell(count(row.total_matches), 'num', 'Matches'));
      return tr;
    },
    'Nothing slow recorded yet.',
    3,
  );

  fillTable(
    'table-recent',
    queries.recent,
    (row) => {
      const tr = document.createElement('tr');
      tr.appendChild(cell(ago(row.age_seconds), null, 'When'));
      tr.appendChild(cell(row.query, 'query', 'Query'));
      tr.appendChild(cell(ms(row.took_ms), 'num', 'ms'));
      tr.appendChild(cell(count(row.total_matches), 'num', 'Matches'));
      tr.appendChild(cell(count(row.returned), 'num', 'Shown'));

      const td = document.createElement('td');
      td.dataset.label = 'Cache';
      if (row.failed) td.appendChild(tag('error', 'error'));
      else if (row.cached) td.appendChild(tag('hit', 'hit'));
      else td.appendChild(tag('', 'miss'));
      tr.appendChild(td);
      return tr;
    },
    'Waiting for the first query.',
    6,
  );

  setText(
    'recent-note',
    `${count(queries.retained)} of ${count(queries.capacity)} retained · ${count(queries.total_recorded)} total`,
  );
}

/* ---------- search ---------- */

function escapeHtml(text) {
  const holder = document.createElement('div');
  holder.textContent = text;
  return holder.innerHTML;
}

/* The snippet is the one place markup is wanted, and the allowance is narrow:
 * escape everything, then restore exactly `<b>` and `</b>`, which is all the
 * engine emits. A crawled page cannot smuggle anything else through. */
function snippetHtml(snippet) {
  return escapeHtml(snippet).replace(/&lt;(\/?)b&gt;/g, '<$1b>');
}

function resultRow(hit) {
  const article = document.createElement('article');
  article.className = 'result';

  const link = document.createElement('a');
  link.className = 'result__title';
  link.href = hit.url;
  link.rel = 'noopener noreferrer';
  link.target = '_blank';
  link.textContent = hit.title || hit.url;

  const heading = document.createElement('h3');
  heading.style.margin = '0';
  heading.appendChild(link);

  const url = document.createElement('span');
  url.className = 'result__url';
  url.textContent = hit.url;

  const snippet = document.createElement('p');
  snippet.className = 'result__snippet';
  snippet.innerHTML = snippetHtml(hit.snippet);

  const meta = document.createElement('p');
  meta.className = 'result__meta';
  meta.textContent =
    `score ${hit.score.toFixed(3)} · fetched ${hit.fetched_at ? ABSOLUTE_DATE.format(new Date(hit.fetched_at * 1000)) : 'unknown'}`;

  article.append(heading, url, snippet, meta);
  return article;
}

function renderSummary(body) {
  const note = $('results-note');
  const wrapper = document.createElement('div');
  wrapper.className = 'results__summary';

  const line = document.createElement('span');
  line.textContent =
    `${count(body.total)} ${body.total === 1 ? 'match' : 'matches'} · ${ms(body.took_ms)}${NON_BREAKING}ms`;
  wrapper.appendChild(line);
  wrapper.appendChild(tag(body.cached ? 'hit' : '', body.cached ? 'cache hit' : 'cache miss'));
  // Duplicates, not results the cap removed: the per-site cap decides what fits on one
  // page and those candidates stay reachable on the next one, so calling them hidden
  // would report them as lost.
  if (body.ranking && body.ranking.suppressed > 0) {
    wrapper.appendChild(tag('', `${count(body.ranking.suppressed)} duplicate collapsed`));
  }
  if (body.ranking && body.reachable && body.reachable < body.total) {
    wrapper.appendChild(tag('', `${count(body.reachable)} reachable by paging`));
  }

  note.replaceChildren(wrapper);
}

function resultMessage(text, tone) {
  const note = $('results-note');
  const paragraph = document.createElement('span');
  paragraph.textContent = text;
  if (tone === 'error') paragraph.style.color = 'var(--error)';
  note.replaceChildren(paragraph);
}

async function runQuery(query, { pushState = true } = {}) {
  const results = $('results');
  const submit = $('search-submit');

  if (pushState) {
    const url = new URL(window.location.href);
    url.searchParams.set('q', query);
    window.history.replaceState({}, '', url);
  }

  submit.setAttribute('aria-busy', 'true');
  submit.textContent = 'Searching…';
  results.setAttribute('aria-busy', 'true');
  resultMessage('Searching the index…');

  try {
    const response = await fetch(`/api/search?q=${encodeURIComponent(query)}&limit=10`);
    const body = await response.json().catch(() => null);

    if (!response.ok) {
      const detail = body && (body.detail || body.error);
      resultMessage(
        detail
          ? `Search failed: ${detail}`
          : `Search failed with HTTP ${response.status}. Check that the API is running.`,
        'error',
      );
      results.replaceChildren();
      return;
    }

    renderSummary(body);
    results.replaceChildren();
    if (!body.results.length) {
      const empty = document.createElement('p');
      empty.className = 'results__empty';
      empty.textContent = `Nothing matched “${body.query}”. Try a broader term, or check that the crawler has indexed this topic yet.`;
      results.appendChild(empty);
      return;
    }
    for (const hit of body.results) results.appendChild(resultRow(hit));
  } catch (error) {
    resultMessage(`Could not reach the dashboard backend: ${error}`, 'error');
  } finally {
    submit.removeAttribute('aria-busy');
    submit.textContent = 'Search';
    results.setAttribute('aria-busy', 'false');
  }
}

/* ---------- polling ---------- */

async function refresh() {
  let snapshot;
  try {
    const response = await fetch('/api/overview', { cache: 'no-store' });
    const body = await response.json().catch(() => null);
    if (!response.ok) {
      throw new Error((body && (body.detail || body.error)) || `HTTP ${response.status}`);
    }
    snapshot = body;
    state.failures = 0;
  } catch (error) {
    state.failures += 1;
    if (state.lastBackendOk !== false) chime([880, 587.33]);
    state.lastBackendOk = false;
    notice(
      `The API is not answering (${state.failures} failed ${state.failures === 1 ? 'attempt' : 'attempts'}): ${error}. This page keeps retrying.`,
      'error',
    );
    setText('foot-left', 'Retrying the API…');
    return;
  }

  if (state.lastBackendOk === false) {
    notice('');
    chime([587.33, 880]);
  }
  state.lastBackendOk = true;
  notice('');

  renderLede(snapshot);
  renderCards(snapshot);
  renderTables(snapshot);
  drawSeries(snapshot.queries.series);

  const newHit = snapshot.queries.recent.length ? snapshot.queries.recent[0].at : null;
  if (state.sound && newHit !== null && state.lastQueryAt !== null && newHit > state.lastQueryAt) {
    chime([880, 1174.66]);
  }
  state.lastQueryAt = newHit;

  setText(
    'foot-left',
    state.paused
      ? 'Polling paused'
      : `Polling the API every ${(POLL_MS / 1000).toFixed(0)}s · last update ${ABSOLUTE_TIME.format(new Date())}`,
  );
}

/* ---------- wiring ---------- */

$('search-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const query = $('q').value.trim();
  if (query) runQuery(query);
  else resultMessage('Enter a search term first.', 'error');
});

$('pause-button').addEventListener('click', (event) => {
  state.paused = !state.paused;
  const button = event.currentTarget;
  button.textContent = state.paused ? 'Resume' : 'Pause';
  button.setAttribute('aria-pressed', String(state.paused));
  setText('foot-left', state.paused ? 'Polling paused' : 'Polling the API…');
});

$('sound-toggle').addEventListener('change', (event) => {
  state.sound = event.target.checked;
  if (state.sound) chime();
});

/* Two-step rather than immediate, and the armed state expires: a destructive
 * control should not be one stray click away, and it should not stay armed
 * forever either. */
$('reset-button').addEventListener('click', async (event) => {
  const button = event.currentTarget;
  if (!state.resetArmed) {
    state.resetArmed = true;
    button.textContent = 'Confirm reset';
    clearTimeout(state.resetTimer);
    state.resetTimer = setTimeout(() => {
      state.resetArmed = false;
      button.textContent = 'Reset log';
    }, 4000);
    return;
  }

  clearTimeout(state.resetTimer);
  state.resetArmed = false;
  button.textContent = 'Reset log';

  const response = await fetch('/api/reset', { method: 'POST' });
  if (!response.ok) {
    const body = await response.json().catch(() => null);
    notice(`Could not reset the query log: ${(body && (body.detail || body.error)) || response.status}`, 'error');
    return;
  }
  state.lastQueryAt = null;
  refresh();
});

// The chart is sized from its container, not from its own bitmap, and the read
// happens on resize rather than during a render pass.
const chart = $('series');
const measure = () => {
  chartWidth = chart.clientWidth;
};
new ResizeObserver(measure).observe(chart);
measure();

setInterval(() => {
  if (!state.paused) refresh();
}, POLL_MS);

refresh();

// A query in the URL is run on load, so a search is a link someone can send.
const initialQuery = new URL(window.location.href).searchParams.get('q');
if (initialQuery) {
  $('q').value = initialQuery;
  runQuery(initialQuery, { pushState: false });
}
