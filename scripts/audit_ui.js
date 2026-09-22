/**
 * Layout audit, evaluated in the page by scripts/audit_ui.py.
 *
 * Kept as its own file rather than a string literal inside the Python script: as a
 * string it had no syntax highlighting, no linting, and its lines were subject to
 * a 100-character Python limit it had no reason to care about.
 *
 * Everything here is measured from the laid-out page. Nothing is inferred from
 * source, because the bugs this exists to catch -- a grid that pushes a column off
 * a phone screen, two boxes landing on top of each other at one width only -- are
 * invisible in the CSS that causes them.
 *
 * Returns a plain object, serialised back to Python by Playwright.
 */
() => {
  /** One report per problem kind, so a broken component cannot flood the output. */
  const out = {
    overflow: [],
    overlaps: [],
    tapTargets: [],
    tinyText: [],
    contrast: [],
    clipped: [],
    pageOverflow: 0,
  };

  const viewportWidth = document.documentElement.clientWidth;

  /** A short, greppable description of an element: tag, id, a few classes, text. */
  const describe = (el) => {
    const id = el.id ? `#${el.id}` : '';
    const cls =
      typeof el.className === 'string' && el.className.trim()
        ? `.${el.className.trim().split(/\s+/).slice(0, 3).join('.')}`
        : '';
    const text = (el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 40);
    return `${el.tagName.toLowerCase()}${id}${cls}${text ? ` "${text}"` : ''}`;
  };

  const visible = (el) => {
    const style = getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden') return false;
    if (Number(style.opacity) === 0) return false;
    const box = el.getBoundingClientRect();
    return box.width > 0.5 && box.height > 0.5;
  };

  /** Whether an element sits inside a horizontally scrollable container. */
  const inScrollContainer = (el) => {
    let node = el.parentElement;
    while (node && node !== document.body) {
      const style = getComputedStyle(node);
      if (style.overflowX === 'auto' || style.overflowX === 'scroll') return true;
      node = node.parentElement;
    }
    return false;
  };

  /**
   * Whether an element is inside a visually-hidden helper (the `sr-only` pattern).
   *
   * That pattern deliberately holds content larger than its own 1x1 clipped box,
   * so both the clip and the overflow it implies are intentional -- and the content
   * stays exposed to assistive tech, which is the whole reason it exists instead of
   * `display: none`.
   */
  const visuallyHidden = (el) => {
    let node = el;
    while (node && node !== document.body) {
      const style = getComputedStyle(node);
      const clipped =
        style.clipPath === 'inset(50%)' || style.clip === 'rect(0px, 0px, 0px, 0px)';
      const boxed =
        style.position === 'absolute' &&
        Number.parseFloat(style.width) <= 1.5 &&
        Number.parseFloat(style.height) <= 1.5;
      if (clipped && boxed) return true;
      node = node.parentElement;
    }
    return false;
  };

  const all = Array.from(document.querySelectorAll('*')).filter(visible);

  // 1. Page-level horizontal overflow: the thing that makes a phone scroll sideways.
  const root = document.documentElement;
  out.pageOverflow = Math.max(0, root.scrollWidth - viewportWidth);

  // 2. Elements whose box escapes the viewport horizontally. Content inside a
  //    scroll container is exempt: it is reachable, and reporting it would drown
  //    the real findings.
  for (const el of all) {
    if (inScrollContainer(el) || visuallyHidden(el)) continue;
    const box = el.getBoundingClientRect();
    if (box.right > viewportWidth + 1 || box.left < -1) {
      out.overflow.push({
        el: describe(el),
        left: Math.round(box.left),
        right: Math.round(box.right),
        width: Math.round(box.width),
        overflowPx: Math.round(Math.max(box.right - viewportWidth, -box.left)),
      });
    }
  }

  // 3. Overlapping siblings. Siblings are the meaningful case: elements in
  //    different stacking contexts are meant to overlap (a badge on a card), but
  //    two boxes emitted by the same container are not.
  //
  //    Elements inside one <svg> are excluded, because a glyph drawn on top of its
  //    own background shape is the definition of an icon rather than a collision.
  //    Flagging that was this audit's own first false positive.
  const sameDrawing = (a, b) => {
    const svgOf = (el) => el.closest('svg');
    const root = svgOf(a);
    return root !== null && root === svgOf(b);
  };

  /**
   * Whether both elements belong to one component that stacks its own parts.
   *
   * The search field is the case: a real input cannot have its glyphs transformed, so
   * the clear-with-dissolve transition paints the value into a mirrored layer over the
   * input and puts the clear button inside the pill. Those are siblings that overlap on
   * purpose. Rather than have the auditor guess that from position and pointer-events --
   * which would also excuse a genuinely colliding absolute box -- the component says so
   * in the markup with `data-overlay`, and only pairs inside the same one are excused.
   */
  const deliberateOverlay = (a, b) => {
    const group = a.closest('[data-overlay]');
    return group !== null && group === b.closest('[data-overlay]');
  };

  /**
   * The rectangles to test, which for a wrapped inline is one per line.
   *
   * `getBoundingClientRect` on an inline element that spans two lines returns the union
   * of its line boxes -- a rectangle that covers both lines and, with them, the text
   * beside it. Comparing those unions reported a wrapped `<code>` in a sentence as
   * overlapping the `<code>` on the next line, which is a sentence doing what sentences
   * do. Per-line rects keep the check honest in both directions: an inline that really
   * is covered by a floating box is still caught, because one of its lines overlaps it.
   */
  const rectsOf = (el) => {
    const list = Array.from(el.getClientRects());
    return list.length > 0 ? list : [el.getBoundingClientRect()];
  };

  const containers = new Set(all.map((el) => el.parentElement).filter(Boolean));
  for (const parent of containers) {
    const kids = Array.from(parent.children).filter(visible);
    for (let i = 0; i < kids.length; i += 1) {
      for (let j = i + 1; j < kids.length; j += 1) {
        if (sameDrawing(kids[i], kids[j])) continue;
        if (deliberateOverlay(kids[i], kids[j])) continue;

        let worst = null;
        for (const a of rectsOf(kids[i])) {
          for (const b of rectsOf(kids[j])) {
            const overlapX = Math.min(a.right, b.right) - Math.max(a.left, b.left);
            const overlapY = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
            // A couple of pixels of rounding is not an overlap; a real one is visible.
            if (overlapX <= 2 || overlapY <= 2) continue;
            if (!worst || overlapX * overlapY > worst.x * worst.y) worst = { x: overlapX, y: overlapY };
          }
        }

        if (worst) {
          out.overlaps.push({
            a: describe(kids[i]),
            b: describe(kids[j]),
            overlap: `${Math.round(worst.x)}x${Math.round(worst.y)}px`,
          });
        }
      }
    }
  }

  // 4. Tap targets. 44x44 is the floor for a *touch* pointer, so it is only
  //    enforced where the pointer is coarse: a text link in a desktop column is not
  //    a tap target, and demanding 44px of it would change the typography for no
  //    one's benefit.
  //
  //    Links inside a sentence are exempt, which is the one exception the guidelines
  //    themselves make (WCAG 2.5.8, "inline": a target in a block of text is not held
  //    to the minimum because growing it would break the line it belongs to). The
  //    recovery link in the error notice is exactly that: 61x19 because it is two words
  //    in a sentence, and padding it to 44px would push the sentence's lines apart to
  //    make a link look like a button. The exemption is narrow on purpose -- it needs
  //    `display: inline` on a static element, so a control that merely happens to be
  //    small is still reported.
  const inProse = (el) => {
    if (el.tagName !== 'A') return false;
    const style = getComputedStyle(el);
    return style.display === 'inline' && style.position === 'static';
  };

  if (window.matchMedia('(pointer: coarse)').matches) {
    const interactive = document.querySelectorAll(
      'a, button, input, select, textarea, [role="button"], [role="tab"]',
    );
    for (const el of interactive) {
      if (!visible(el)) continue;
      if (inProse(el)) continue;
      const box = el.getBoundingClientRect();
      if (box.width < 44 || box.height < 44) {
        out.tapTargets.push({
          el: describe(el),
          size: `${Math.round(box.width)}x${Math.round(box.height)}`,
        });
      }
    }
  }

  /** Elements that render text of their own, as opposed to only wrapping others. */
  const textElements = Array.from(
    document.querySelectorAll(
      'p, span, li, td, th, a, h1, h2, h3, h4, label, dd, dt, figcaption, caption, small, code, button',
    ),
  ).filter((el) => {
    if (!visible(el)) return false;
    return Array.from(el.childNodes).some(
      (node) => node.nodeType === 3 && node.textContent.trim().length > 0,
    );
  });

  // 5. Text size, against the 12px floor.
  for (const el of textElements) {
    const size = Number.parseFloat(getComputedStyle(el).fontSize);
    if (size < 12) out.tinyText.push({ el: describe(el), fontSize: size });
  }

  // 6. Contrast against the nearest opaque background: WCAG AA for body text.
  const parseColor = (value) => {
    const match = value.match(/rgba?\(([^)]+)\)/);
    if (!match) return null;
    const parts = match[1].split(',').map((part) => Number.parseFloat(part));
    return { r: parts[0], g: parts[1], b: parts[2], a: parts.length > 3 ? parts[3] : 1 };
  };

  const luminance = (color) => {
    const channel = (value) => {
      const v = value / 255;
      return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
    };
    return 0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b);
  };

  const opaqueBackground = (el) => {
    let node = el;
    while (node) {
      const color = parseColor(getComputedStyle(node).backgroundColor);
      if (color && color.a === 1) return color;
      node = node.parentElement;
    }
    return { r: 255, g: 255, b: 255, a: 1 };
  };

  for (const el of textElements) {
    const style = getComputedStyle(el);
    const foreground = parseColor(style.color);
    if (!foreground || foreground.a === 0) continue;
    const background = opaqueBackground(el);
    const l1 = luminance(foreground);
    const l2 = luminance(background);
    const ratio = (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
    const size = Number.parseFloat(style.fontSize);
    const weight = Number(style.fontWeight) || 400;
    const large = size >= 24 || (size >= 18.66 && weight >= 700);
    const required = large ? 3.0 : 4.5;
    if (ratio < required) {
      out.contrast.push({
        el: describe(el),
        ratio: Math.round(ratio * 100) / 100,
        required,
        fontSize: size,
      });
    }
  }

  // 7. Content clipped by an ancestor that hides overflow.
  for (const el of all) {
    if (visuallyHidden(el)) continue;
    let node = el.parentElement;
    while (node && node !== document.body) {
      const style = getComputedStyle(node);
      // Inside a scroll container content is reachable, so it is not clipped in
      // the sense that matters. Only `hidden` loses content.
      if (style.overflowX === 'auto' || style.overflowX === 'scroll') break;
      if (style.overflow === 'hidden' || style.overflowY === 'hidden' || style.overflowX === 'hidden') {
        const parentBox = node.getBoundingClientRect();
        const box = el.getBoundingClientRect();
        const cutX = box.right > parentBox.right + 1 || box.left < parentBox.left - 1;
        const cutY = box.bottom > parentBox.bottom + 1 || box.top < parentBox.top - 1;
        if (cutX || cutY) {
          out.clipped.push({
            el: describe(el),
            clippedBy: describe(node),
            axis: [cutX ? 'x' : null, cutY ? 'y' : null].filter(Boolean).join('+'),
          });
        }
        break;
      }
      node = node.parentElement;
    }
  }

  return out;
}
