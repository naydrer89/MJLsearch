/**
 * Progressive enhancement for the search page and the growth page.
 *
 * The pages are complete without this file: searching, paging, reading results and
 * reading every growth figure are all server-rendered, so a query is a real URL and
 * works with scripts blocked. What is added here is only the small conveniences that
 * need a live document -- and if this never runs, nothing is missing that a reader
 * needs.
 */
'use strict';

(() => {
  const body = document.body;
  const input = document.getElementById('q');

  // 1. The bar only lifts off the page once there is something to be lifted above.
  //    A threshold of a few pixels avoids flickering at the very top of a scroll.
  const syncScrolled = () => {
    body.classList.toggle('is-scrolled', window.scrollY > 4);
  };
  syncScrolled();
  window.addEventListener('scroll', syncScrolled, { passive: true });

  // 2. `/` focuses the box, the way a search page is expected to behave. Skipped
  //    while the visitor is already typing somewhere, or the character would be
  //    swallowed instead of entered.
  document.addEventListener('keydown', (event) => {
    if (event.key !== '/' || event.metaKey || event.ctrlKey || event.altKey) return;
    const active = document.activeElement;
    const typing = active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.isContentEditable);
    if (typing) return;
    if (!input) return;
    event.preventDefault();
    input.focus();
    input.select();
  });

  // 3. Landing to results moves the box from the middle of the page to the top,
  //    which counts as a navigation, so submit explicitly and let the browser treat
  //    it as one instead of relying on the form's default behaviour.
  const form = document.querySelector('.box');
  if (form) {
    form.addEventListener('submit', () => {
      const value = input ? input.value.trim() : '';
      if (!value) return;
      document.getElementById('results')?.setAttribute('aria-busy', 'true');
      const button = form.querySelector('.box__go');
      if (button) button.setAttribute('aria-busy', 'true');
    });
  }

  /* ---------- the growth feed ----------
   *
   * Polled rather than pushed: this is one small JSON document every few seconds
   * against a service that also serves queries, and a websocket would add a
   * connection per open tab to save a request that is already cheap.
   *
   * The numbers are tweened rather than replaced. A counter that jumps from 1 240 to
   * 1 283 is unreadable precisely when it is most interesting -- while it is moving --
   * and the animation is what makes a running indexer visible rather than something
   * you have to catch between page loads.
   */

  const groupThousands = (value) => String(Math.round(value)).replace(/\B(?=(\d{3})+(?!\d))/g, '\u2009');

  // The transitions themselves live in motion.js; this file decides when a value
  // changed. If that module never loads, the counters are still written -- plainly,
  // without the pop-in -- because a monitoring number is not allowed to depend on an
  // animation being available.
  const motion = window.MJL || null;

  const setCounter = (element, text) => {
    if (!element) return;
    if (motion) motion.popDigits(element, text);
    else element.textContent = text;
  };

  const setState = (element, text, idle, live) => {
    if (!element) return;
    element.textContent = text;
    element.classList.toggle('is-idle', Boolean(idle));
    // The shimmer claims "this is happening now", which is narrower than "not idle": a
    // crawl that stopped two minutes ago is neither, and showing it as live would make
    // the one animated label on the page the untrustworthy one.
    element.classList.toggle('growth__state--live', Boolean(live));
  };

  // Mirrors the server's phrasing exactly, including the part that matters most: a
  // rate is an average over a window, so it keeps describing a crawl that has stopped,
  // and saying "indexing" about that would be a present-tense claim the data does not
  // support. Past a minute of silence the average is labelled as one.
  const describe = (growth) => {
    if (!growth.available) return 'no index reading yet';
    if (growth.idle) return 'indexer idle';

    const rate = Number(growth.rate_per_minute || 0);
    const age = growth.age_seconds;
    const averaged = `${groupThousands(rate)}/min avg`;

    if (typeof age === 'number' && age > 60) return `quiet ${shortDuration(age)} · ${averaged}`;
    if (!(rate > 0)) return 'indexer running';
    return `indexing ${groupThousands(rate)}/min`;
  };

  const shortDuration = (seconds) => {
    if (seconds < 60) return `${Math.round(seconds)} s`;
    if (seconds < 3600) return `${Math.round(seconds / 60)} min`;
    return `${(seconds / 3600).toFixed(1)} h`;
  };

  const isLive = (growth) => {
    if (!growth.available || growth.idle) return false;
    const age = growth.age_seconds;
    return typeof age !== 'number' || age <= 60;
  };

  const paintStrip = (strip, growth) => {
    if (!strip) return;
    setCounter(strip.querySelector('[data-growth="total"]'), groupThousands(growth.total_documents || 0));
    setCounter(strip.querySelector('[data-growth="hour"]'), groupThousands(growth.documents_last_hour || 0));
    setState(
      strip.querySelector('[data-growth="state"]'),
      describe(growth),
      growth.idle || !growth.available,
      isLive(growth)
    );
    strip.querySelector('.growth__pulse')?.classList.toggle('growth__pulse--idle', growth.idle || !growth.available);
  };

  const paintStats = (root, growth, series) => {
    const formatNumber = (value) => groupThousands(value);
    const fields = {
      total: Number(growth.total_documents || 0),
      hour: Number(growth.documents_last_hour || 0),
      quarter: Number(growth.documents_last_quarter || 0),
      projected: Number(growth.projected_next_hour || 0),
    };

    for (const [key, value] of Object.entries(fields)) {
      setCounter(root.querySelector(`[data-stat="${key}"]`), formatNumber(value));
    }

    const rate = root.querySelector('[data-stat="rate"]');
    if (rate) setCounter(rate, Number(growth.rate_per_minute || 0).toFixed(1));

    // The headline figure rolls instead of popping in: one number on the page is an
    // event when it moves, and this is it. Its spoken twin is updated with plain text.
    const hero = root.querySelector('[data-reel="total"]');
    if (hero && motion) motion.reel(hero, formatNumber(fields.total));
    // Spaced the same way the reel is, so the two are not read out as different
    // numbers by the two things that read this page.
    const spoken = root.querySelector('[data-reel-text="total"]');
    if (spoken) spoken.textContent = `${formatNumber(fields.total)} documents in the index`;

    const age = root.querySelector('.panel__meta.mono');
    if (age) age.textContent = `last commit ${formatAge(growth.age_seconds)}`;

    if (!series) return;
    const peak = root.querySelector('[data-stat="peak"]');
    if (peak) setCounter(peak, groupThousands(series.peak || 0));

    const chart = root.querySelector('[data-chart]');
    if (chart) morphChart(chart, series);

    const rows = root.querySelectorAll('[data-recent] tr');
    const newest = series.points.slice(-rows.length).reverse();
    rows.forEach((row, index) => {
      const point = newest[index];
      if (!point) return;
      const label = row.querySelector('th');
      const value = row.querySelector('td');
      if (label) label.textContent = formatClock(point.at);
      if (value) {
        value.dataset.documents = String(point.documents);
        value.textContent = groupThousands(point.documents);
      }
    });
  };

  /* ---------- the line chart ----------
   *
   * The curve is computed on the server (see `chart_paths`), because a second
   * implementation of one curve is one more than can be right — that mistake is
   * already documented once in this codebase, and it was a chart that drew sixty
   * empty bars. What happens here is only interpolation: both paths come from the
   * same server function, so they always carry the same commands in the same order,
   * which means the numbers in one can simply be tweened into the numbers in the
   * other. No curve maths, no shape knowledge — just a numeric lerp of two strings.
   *
   * If the command structure ever differs (a window of a different length), the new
   * path is set outright. Snapping is a worse frame than a morph and a much better
   * one than a mangled path.
   */

  const NUMBERS = /-?\d+(?:\.\d+)?/g;

  const lerpPath = (from, to, t) => {
    const source = from.match(NUMBERS);
    const target = to.match(NUMBERS);
    if (!source || !target || source.length !== target.length) return to;

    let index = 0;
    return to.replace(NUMBERS, () => {
      const start = Number(source[index]);
      const end = Number(target[index]);
      index += 1;
      return (start + (end - start) * t).toFixed(3);
    });
  };

  // One animation per element, replaced rather than queued: a poll that lands mid-morph
  // should move the chart from where it is to the newest reading, not replay the
  // reading in between.
  const running = new WeakMap();

  const tweenPath = (element, attribute, target, duration) => {
    if (!element) return;
    const from = element.getAttribute(attribute) || '';
    running.get(element)?.cancel();

    if (!from || from === target || reduceMotion()) {
      element.setAttribute(attribute, target);
      return;
    }

    const started = performance.now();
    let frame = 0;

    const step = (now) => {
      const t = Math.min(1, (now - started) / duration);
      // Ease-out cubic, matching the curve the stylesheet uses for the bars that
      // used to be here, so a live page has one feel rather than two.
      const eased = 1 - (1 - t) ** 3;
      element.setAttribute(attribute, lerpPath(from, target, eased));
      if (t < 1) frame = requestAnimationFrame(step);
      else running.delete(element);
    };

    frame = requestAnimationFrame(step);
    running.set(element, { cancel: () => cancelAnimationFrame(frame) });
  };

  const reduceMotion = () =>
    motion ? motion.reduceMotion() : window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  const morphChart = (chart, series) => {
    const duration = Number(chart.dataset.morphMs || 600);
    tweenPath(chart.querySelector('[data-line]'), 'd', series.line || '', duration);
    tweenPath(chart.querySelector('[data-line-area]'), 'd', series.area || '', duration);
    tweenPath(chart.querySelector('[data-line-rate]'), 'd', series.rate_line || '', duration);
    chart.querySelectorAll('[data-line-dot]').forEach((dot) =>
      tweenPath(dot, 'd', series.dot || '', duration)
    );
    // The rate is written twice -- as the dashed line and as the figure beside it -- so
    // both are updated from the one reading, or one of them goes stale while the other
    // moves. The line's own `d` carries the same number, so they cannot disagree.
    setCounter(
      chart.parentElement?.querySelector('[data-stat="rate-inline"]'),
      Number(series.rate_per_minute || 0).toFixed(1)
    );
  };

  const formatClock = (seconds) => {
    const date = new Date(seconds * 1000);
    return `${String(date.getUTCHours()).padStart(2, '0')}:${String(date.getUTCMinutes()).padStart(2, '0')}`;
  };

  const formatAge = (seconds) => {
    if (seconds === null || seconds === undefined) return 'never';
    if (seconds < 5) return 'just now';
    return `${shortDuration(seconds)} ago`;
  };

  // The series arrives ready-made from the page's own `/api/growth`, which derives it
  // from the same function the server-rendered chart used. Deriving it here as well was
  // the bug that made a live page draw sixty empty bars: see the route's docstring.
  const seriesOf = (growth) => (growth && growth.series ? growth.series : null);

  /* ---------- the field's own motion ----------
   *
   * Attached once, on load: the clear affordance and its dissolve, plus the shake that
   * a rejected query arrives with. The server renders the error state, so the shake is
   * played rather than triggered by an interaction that already happened.
   */
  if (motion) {
    document.querySelectorAll('[data-clear]').forEach((field) => motion.attachClear(field));
    const rejected = document.querySelector('.t-clear.is-error');
    if (rejected) motion.playErrorShake(rejected);
  }

  const strip = document.getElementById('growth');
  const statsRoot = document.querySelector('[data-stats]');

  if (strip || statsRoot) {
    const pollMs = Number(strip?.dataset.pollMs || 5000);
    let stopped = false;

    const tick = async () => {
      if (stopped) return;
      const endpoint = statsRoot?.dataset.statSource || strip?.dataset.growthEndpoint;
      if (!endpoint) return;

      try {
        const response = await fetch(endpoint, { headers: { accept: 'application/json' } });
        if (!response.ok) throw new Error(`growth feed ${response.status}`);
        const growth = await response.json();
        paintStrip(strip, growth);
        if (statsRoot) paintStats(statsRoot, growth, seriesOf(growth));
      } catch {
        // Reported in place rather than thrown: the reading being unavailable is one of
        // the states this strip exists to display, and a console warning every few seconds
        // would be noise on top of a page that is already saying so.
        setState(strip?.querySelector('[data-growth="state"]'), 'growth feed unreachable', true, false);
        strip?.querySelector('.growth__pulse')?.classList.add('growth__pulse--idle');
      }

      if (!stopped) window.setTimeout(tick, pollMs);
    };

    // Paused while the tab is hidden, and picked up again on return: a page left open
    // in a background tab should not poll all day, and the first thing a returning
    // reader should see is a current number rather than a stale one.
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) {
        stopped = true;
      } else if (stopped) {
        stopped = false;
        tick();
      }
    });

    window.setTimeout(tick, pollMs);
  }
})();
