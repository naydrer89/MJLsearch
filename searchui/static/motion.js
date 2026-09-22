/**
 * The transitions.dev effects this project uses, adapted to its own tokens.
 *
 * Four of the library's transitions, each one copied from its recipe and then
 * narrowed to what this page needs:
 *
 *   1. number pop-in     -> `popDigits`      counters re-enter digit by digit
 *   2. input clear with
 *      dissolve          -> `attachClear`    the query flies out, the placeholder falls in
 *   3. error state shake -> `playErrorShake` a rejected query shoves the box
 *   4. spinning counter  -> `reel`           the headline number rolls to its new value
 *
 * The library's class names (`t-digit`, `t-clear*`, `t-input`, `t-reel*`) and variable
 * names are kept verbatim so a snippet can be re-copied over this file without a
 * translation step -- which is the whole point of a copy-and-paste transition library.
 *
 * Two rules from the recipes are load-bearing and are kept:
 *   * every animation is zeroed under `prefers-reduced-motion` (in the stylesheet, and
 *     here for the parts driven per frame);
 *   * a transition is only played when the value actually changed, because a counter
 *     that re-enters every poll is noise rather than emphasis.
 *
 * Nothing here is required for the page to work: the numbers are rendered by the
 * server, the field is a real input with a real clear affordance either way, and if
 * this file never runs, the page is complete and static.
 */
'use strict';

window.MJL = (() => {
  const root = document.documentElement;

  const reduceMotion = () =>
    window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  const num = (name, fallback) => {
    const value = Number.parseFloat(getComputedStyle(root).getPropertyValue(name));
    return Number.isFinite(value) ? value : fallback;
  };

  /** Minimal cubic-bezier sampler, so JS easing matches the CSS curves exactly. */
  const bezier = (spec) => {
    const match = String(spec).match(/cubic-bezier\(([-\d.]+),([-\d.]+),([-\d.]+),([-\d.]+)\)/);
    if (!match) return (t) => t;
    const [x1, y1, x2, y2] = match.slice(1).map(Number.parseFloat);
    const cx = 3 * x1;
    const bx = 3 * (x2 - x1) - cx;
    const ax = 1 - cx - bx;
    const cy = 3 * y1;
    const by = 3 * (y2 - y1) - cy;
    const ay = 1 - cy - by;
    return (t) => {
      if (t <= 0) return 0;
      if (t >= 1) return 1;
      let s = t;
      for (let i = 0; i < 8; i += 1) {
        const dx = ((ax * s + bx) * s + cx) * s - t;
        const d = (3 * ax * s + 2 * bx) * s + cx;
        if (Math.abs(dx) < 1e-6 || d === 0) break;
        s -= dx / d;
      }
      return ((ay * s + by) * s + cy) * s;
    };
  };

  /* ---------- 1. number pop-in ---------- */

  /**
   * Renders `text` into `element` one span per character and replays the pop-in.
   *
   * Skipped when the text is unchanged: the counters are polled every few seconds, and
   * a number that re-enters on every poll would read as flicker rather than as news.
   * The comparison is against what the element already shows, so the first call after
   * a page load does not animate a value that did not move.
   */
  const popDigits = (element, text) => {
    if (!element) return;
    const next = String(text);
    const shown = element.dataset.value ?? element.textContent;

    if (shown === next) {
      element.dataset.value = next;
      return;
    }
    element.dataset.value = next;

    element.classList.add('t-digit-group');
    if (reduceMotion()) {
      element.textContent = next;
      return;
    }

    element.classList.remove('is-animating');
    const chars = Array.from(next);
    element.replaceChildren(
      ...chars.map((char, index) => {
        const span = document.createElement('span');
        span.className = 't-digit';
        span.textContent = char;
        // The last two digits ride in behind the leading ones, so the change lands on
        // the end of the number -- where it happened -- instead of all at once.
        if (index === chars.length - 2) span.dataset.stagger = '1';
        else if (index === chars.length - 1) span.dataset.stagger = '2';
        return span;
      })
    );

    void element.offsetHeight; // force reflow, or the class change is coalesced away
    element.classList.add('is-animating');
    window.setTimeout(() => element.classList.remove('is-animating'), num('--digit-dur', 500) + 200);
  };

  /* ---------- 4. spinning counter ---------- */

  /**
   * One cell of a reel: the laid-out height of a digit, measured rather than parsed.
   *
   * Measured because the recipe's `--reel-cell` is a length (`2.9rem`) and
   * `parseFloat('2.9rem')` is `2.9` -- a number with a unit stripped off it, which is
   * silently wrong in the one way that matters here: the strip then travels 2.9 pixels
   * per cell and every column shows a slice of four digits instead of one.
   */
  const reelCell = (strip) => {
    const digit = strip?.firstElementChild;
    if (digit) {
      const height = digit.getBoundingClientRect().height;
      if (height > 0) return height;
    }
    return 30;
  };

  const buildColumn = (char) => {
    const column = document.createElement('span');
    column.className = 't-reel-col';
    // A column holds ten digits to show one, so its text content is not the number.
    // Left readable, a screen reader reads "012345678901234567890123" for a single
    // `3`, and selecting the figure copies the whole reel. The value is exposed once,
    // on the reel itself (see `announce`), so every column is hidden from tools.
    column.setAttribute('aria-hidden', 'true');

    if (!/\d/.test(char)) {
      // A separator (a thin space, a full stop, a comma) is not a digit and does not
      // spin: rolling a space through ten positions would be motion about nothing.
      const cell = document.createElement('span');
      cell.className = 't-reel-digit';
      cell.textContent = char;
      column.appendChild(cell);
      column.dataset.static = '1';
      return column;
    }

    const strip = document.createElement('span');
    strip.className = 't-reel-strip';
    column.appendChild(strip);
    column.dataset.digit = char;
    return column;
  };

  /**
   * Exposes the value the reel is showing as text, once, for screen readers.
   *
   * It is a sibling of the columns rather than their text, which is why the column
   * count is taken from `.t-reel-col` and not from `childElementCount`.
   */
  const announce = (element, text) => {
    let label = element.querySelector(':scope > .sr-only');
    if (!label) {
      label = document.createElement('span');
      label.className = 'sr-only';
      element.appendChild(label);
    }
    label.textContent = text;
  };

  const spinColumn = (column, digit, index, animate) => {
    if (column.dataset.static === '1') return;
    if (column.dataset.digit === digit && column.dataset.spun === '1') return;

    const spins = 2; // two full turns, the way the recipe lands on a target digit
    const target = Number.parseInt(digit, 10);
    const strip = column.querySelector('.t-reel-strip') || column.appendChild(
      Object.assign(document.createElement('span'), { className: 't-reel-strip' })
    );
    strip.replaceChildren(
      ...Array.from({ length: spins * 10 + target + 1 }, (_, position) => {
        const cellElement = document.createElement('span');
        cellElement.className = 't-reel-digit';
        cellElement.textContent = String(position % 10);
        return cellElement;
      })
    );

    column.dataset.digit = digit;
    column.dataset.spun = '1';

    // Measured after the strip exists, because a cell's height is a layout fact: at 44px
    // of type it is 46px, at 16px it is 18px, and only the DOM knows which.
    const cell = reelCell(strip);

    strip.style.transition = 'none';
    strip.style.transform = 'translateY(0)';
    // First paint lands on the value and stays there. The recipe's rule is that a
    // transition plays when the value *changed*, and on the first paint nothing has:
    // rolling a number the visitor has never seen before from zero reads as a fault
    // rather than as emphasis, and it leaves the figure unreadable for a second.
    if (reduceMotion() || !animate) {
      strip.style.transform = `translateY(${-(spins * 10 + target) * cell}px)`;
      return;
    }

    const duration = num('--reel-dur', 1100);
    const stagger = num('--reel-stagger', 70);
    const delay = index * stagger;

    // The streak. A vertical-only blur needs an SVG filter (`stdDeviation="0 y"`), which
    // the stylesheet defines and this toggles: a CSS blur would smear the digits
    // sideways, which reads as a rendering fault rather than as speed.
    strip.style.filter = 'url(#mjl-reel-blur)';
    void strip.offsetHeight;
    strip.style.transition = `transform ${duration}ms var(--reel-ease) ${delay}ms`;
    strip.style.transform = `translateY(${-(spins * 10 + target) * cell}px)`;

    window.setTimeout(() => {
      strip.style.filter = '';
      strip.style.transition = '';
    }, delay + duration + 20);
  };

  /**
   * Rolls `element` to `text`, spinning only the columns whose digit changed.
   *
   * A rebuilt reel (first paint, or the number gained a digit) spins every column,
   * because a changed digit count moves every column along and animating a diff would
   * put the digits in positions they never occupied.
   */
  const reel = (element, text) => {
    if (!element) return;
    const next = String(text);
    if (element.dataset.value === next) return;

    const first = element.dataset.value === undefined;
    element.dataset.value = next;

    const chars = Array.from(next);
    let columns = [...element.querySelectorAll(':scope > .t-reel-col')];
    const rebuild = columns.length !== chars.length;

    if (rebuild) {
      announce(element, next);
      const label = element.querySelector(':scope > .sr-only');
      element.replaceChildren(...chars.map(buildColumn), label);
      columns = [...element.querySelectorAll(':scope > .t-reel-col')];
      columns.forEach((column) => delete column.dataset.spun);
    }

    announce(element, next);
    chars.forEach((char, index) => spinColumn(columns[index], char, index, !first));
  };

  /* ---------- 3. error state shake ---------- */

  /** Shakes `field` once, so a rejected query is felt before it is read. */
  const playErrorShake = (field) => {
    if (!field || reduceMotion()) return;
    field.classList.remove('is-shaking');
    void field.offsetWidth; // replay from a clean baseline
    field.classList.add('is-shaking');

    const total = num('--shake-dur-a', 80) * 2 + num('--shake-dur-b', 60) * 2;
    window.setTimeout(() => field.classList.remove('is-shaking'), total + 20);
  };

  /* ---------- 2. input clear with dissolve ---------- */

  /**
   * Wires one search field: mirror the value, dissolve it on clear, fall the
   * placeholder back in, and ignite a per-word streak under the field while it goes.
   *
   * The mirror owns the glyphs while the field has a value (the input's own text is
   * painted transparent), because a real input's text cannot be transformed. That is
   * only safe while the text fits the field: an overflowing input scrolls to its caret,
   * and a static mirror would show the beginning of a line the field is showing the end
   * of. So the mirror takes over only when the value fits, and a clear of a long query
   * is instant instead of a convincing-looking lie.
   */
  const attachClear = (clear) => {
    const input = clear?.querySelector('input');
    const mirror = clear?.querySelector('.t-clear-mirror');
    const placeholder = clear?.querySelector('.t-clear-placeholder');
    const glow = clear?.querySelector('.t-clear-glow');
    const button = clear?.querySelector('.t-clear-btn');
    if (!clear || !input || !mirror || !glow || !button) return null;

    const canvas = document.createElement('canvas').getContext('2d');
    let clearing = false;

    // The field's own placeholder is kept in the markup so the page reads correctly
    // without scripts; once the mirror is in charge, the browser's copy would sit under
    // the fake one and the fall-in would not be visible.
    const nativePlaceholder = input.getAttribute('placeholder') || '';
    if (placeholder && !placeholder.textContent.trim()) placeholder.textContent = nativePlaceholder;
    input.setAttribute('placeholder', '');

    const fits = () => input.scrollWidth <= input.clientWidth + 1;

    const sync = () => {
      const hasValue = input.value.length > 0 && fits();
      clear.classList.toggle('has-value', hasValue);
      if (hasValue) mirror.textContent = input.value.replace(/ /g, '\u00a0');
    };

    /** Per-word streak: a stack of radial gradients, one per word, under the field. */
    const buildGlow = (text) => {
      canvas.font = getComputedStyle(input).font;
      const spread = num('--glow-spread', 1.5);
      const width = clear.clientWidth || 280;
      const padding = Number.parseFloat(getComputedStyle(input).paddingLeft) || 16;
      const layers = [];
      let offset = 0;

      text.split(/(\s+)/).forEach((segment) => {
        const segmentWidth = canvas.measureText(segment).width;
        if (segment.trim()) {
          const centre = padding + offset + segmentWidth / 2;
          const half = Math.max(segmentWidth * 0.45, 8) * spread;
          [
            [0, 0.8, 7, 0.22],
            [half * 0.45, 0.55, 8, 0.18],
            [-half * 0.4, 0.65, 6, 0.16],
            [half * 0.15, 0.9, 5, 0.14],
          ].forEach(([dx, widthRatio, height, alpha]) => {
            const at = (((centre + dx) / width) * 100).toFixed(2);
            layers.push(
              `radial-gradient(ellipse ${Math.max(half * widthRatio, 2).toFixed(1)}px ${height}px ` +
                `at ${at}% 100%, rgba(0,0,0,${alpha}), transparent)`
            );
          });
        }
        offset += segmentWidth;
      });

      return layers.join(', ');
    };

    const clearNow = () => {
      // Nothing to animate: no value, already animating, or a value too long for the
      // mirror to represent. The field is still cleared, which is the part that matters.
      input.value = '';
      clear.classList.remove('has-value');
      sync();
    };

    const clearWithAnimation = () => {
      if (clearing) return;
      if (!input.value || !fits() || reduceMotion()) {
        clearNow();
        return;
      }

      clearing = true;
      const keepFocus = document.activeElement === input;
      const text = input.value;

      const total = num('--clear-dur', 1000);
      const outDuration = num('--clear-out-dur', 400);
      const inDuration = num('--clear-in-dur', 400);
      const outFly = num('--clear-out-fly', 12);
      const inFly = num('--clear-in-fly', 12);
      const blur = num('--clear-blur', 2);
      const delay = num('--glow-delay', 50);
      const peakAt = num('--glow-peak-at', 0.15);
      const glowOpacity = num('--glow-opacity', 0.42);
      const easeOut = bezier(getComputedStyle(root).getPropertyValue('--clear-out-ease'));
      const easeIn = bezier(getComputedStyle(root).getPropertyValue('--clear-in-ease'));

      mirror.textContent = text.replace(/ /g, '\u00a0');
      input.value = '';
      clear.classList.remove('has-value');
      clear.classList.add('is-clearing');
      glow.style.background = buildGlow(text);
      glow.style.opacity = '0';

      if (placeholder) {
        placeholder.style.transform = `translateY(${-inFly}px)`;
        placeholder.style.opacity = '0.9';
        placeholder.style.filter = `blur(${blur}px)`;
      }

      const started = performance.now();
      const frame = (now) => {
        const elapsed = now - started;
        const out = easeOut(Math.min(1, elapsed / outDuration));
        mirror.style.transform = `translateY(${(out * outFly).toFixed(1)}px)`;
        mirror.style.opacity = (1 - out).toFixed(3);
        mirror.style.filter = `blur(${(out * blur).toFixed(1)}px)`;

        const backIn = easeIn(Math.min(1, elapsed / inDuration));
        if (placeholder) {
          placeholder.style.transform = `translateY(${(-inFly + backIn * inFly).toFixed(1)}px)`;
          placeholder.style.opacity = (0.9 + backIn * 0.1).toFixed(3);
          placeholder.style.filter = `blur(${(blur - backIn * blur).toFixed(1)}px)`;
        }

        // Rise, peak, fall: the streak is an envelope, which is why this is per frame
        // rather than a keyframe.
        let intensity = 0;
        if (elapsed > delay) {
          const progress = Math.min(1, (elapsed - delay) / Math.max(1, total - delay));
          intensity = progress < peakAt ? progress / peakAt : 1 - (progress - peakAt) / (1 - peakAt);
        }
        glow.style.opacity = (intensity * glowOpacity).toFixed(3);

        if (elapsed < total) {
          requestAnimationFrame(frame);
          return;
        }

        clear.classList.remove('is-clearing');
        mirror.style.cssText = '';
        mirror.textContent = '';
        glow.style.opacity = '0';
        glow.style.background = '';
        if (placeholder) placeholder.style.cssText = '';
        clearing = false;
        // The button is inside the field, so a click moved focus to it; putting focus
        // back where the typing happens is the difference between clearing and leaving.
        if (keepFocus) requestAnimationFrame(() => input.focus({ preventScroll: true }));
      };
      requestAnimationFrame(frame);
    };

    // Keeping focus on the field while the pointer is down on the button, or the clear
    // would blur the input and dismiss a mobile keyboard mid-interaction.
    const keepFocus = (event) => {
      if (document.activeElement === input) event.preventDefault();
    };
    button.addEventListener('pointerdown', keepFocus);
    button.addEventListener('mousedown', keepFocus);
    button.addEventListener('click', clearWithAnimation);

    input.addEventListener('input', sync);
    input.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      if (!input.value) return;
      // Escape is the keyboard's clear button, and it gets the same dissolve rather
      // than a second, different behaviour for the same action.
      event.preventDefault();
      clearWithAnimation();
    });
    // A resize can change whether the value fits, so the mirror's ownership is re-decided.
    window.addEventListener('resize', sync, { passive: true });
    sync();

    return { clear: clearWithAnimation, sync };
  };

  return { popDigits, reel, attachClear, playErrorShake, reduceMotion };
})();
