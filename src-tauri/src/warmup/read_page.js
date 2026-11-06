(function (options) {
  'use strict';
  var MAX_ANCHORS = 1500;
  var MAX_LINKS = 400;
  var MAX_BUTTONS = 40;
  var MAX_HOVER = 40;
  var MAX_NODES = 6000;
  var vw = window.innerWidth;
  var vh = window.innerHeight;
  var doc = document.documentElement;

  function clean(text, limit) {
    return String(text || '').normalize('NFC').replace(/\s+/g, ' ').trim().slice(0, limit);
  }

  function clip(rect) {
    var x1 = Math.max(0, rect.left);
    var y1 = Math.max(0, rect.top);
    var x2 = Math.min(vw, rect.right);
    var y2 = Math.min(vh, rect.bottom);
    return x2 > x1 && y2 > y1 ? { x: x1, y: y1, w: x2 - x1, h: y2 - y1 } : null;
  }

  // The part of an element's boxes that shows in the window: the largest line
  // box of an inline element, so a link wrapped over two lines is aimed at on
  // one of them.
  function bestBox(el) {
    var rects = el.getClientRects();
    var best = null;
    var bestArea = 0;
    for (var i = 0; i < rects.length; i++) {
      var shown = clip(rects[i]);
      if (!shown) continue;
      var area = shown.w * shown.h;
      if (area > bestArea) {
        best = shown;
        bestArea = area;
      }
    }
    return best;
  }

  function shows(el) {
    var node = el;
    var opacity = 1;
    for (var i = 0; node && node.nodeType === 1 && i < 8; i++, node = node.parentElement) {
      var style = getComputedStyle(node);
      if (style.display === 'none') return false;
      if (i === 0 && (style.visibility !== 'visible' || style.pointerEvents === 'none')) return false;
      opacity *= parseFloat(style.opacity || '1');
    }
    return opacity > 0.1;
  }

  function topAt(el, x, y) {
    var root = el.getRootNode && el.getRootNode();
    var scope = root && root.elementFromPoint ? root : document;
    return scope.elementFromPoint(x, y);
  }

  function reachable(el, box) {
    var top = topAt(el, box.x + box.w / 2, box.y + box.h / 2);
    return !!top && (top === el || el.contains(top) || (top.closest && top.closest('a,button') === el));
  }

  function regionOf(el) {
    if (el.closest('footer,[role=contentinfo]')) return 'footer';
    if (el.closest('nav,[role=navigation]')) return 'nav';
    if (el.closest('aside,[role=complementary]')) return 'aside';
    if (el.closest('header,[role=banner]')) return 'header';
    if (el.closest('main,article,[role=main]')) return 'content';
    return 'other';
  }

  function linkText(a) {
    var text = clean(a.innerText || a.textContent, 160);
    var label = clean(a.getAttribute('aria-label'), 80);
    var title = clean(a.getAttribute('title'), 80);
    var img = a.querySelector && a.querySelector('img[alt]');
    var alt = img ? clean(img.getAttribute('alt'), 80) : '';
    return clean([text, label, alt, title].filter(Boolean).join(' '), 200);
  }

  function collectLinks() {
    var out = [];
    var anchors = document.querySelectorAll('a[href]');
    var seen = Math.min(anchors.length, MAX_ANCHORS);
    for (var i = 0; i < seen && out.length < MAX_LINKS; i++) {
      var a = anchors[i];
      if (typeof a.href !== 'string') continue;
      var quick = a.getBoundingClientRect();
      if (quick.width < 4 || quick.height < 4 || quick.bottom < 0 || quick.top > vh || quick.right < 0 || quick.left > vw) continue;
      var box = bestBox(a);
      if (!box || box.w < 10 || box.h < 6) continue;
      if (!shows(a) || !reachable(a, box)) continue;
      out.push({
        href: a.href,
        text: linkText(a),
        rect: box,
        region: regionOf(a),
        newTab: a.target === '_blank',
        download: a.hasAttribute('download'),
        rel: a.rel || ''
      });
    }
    return out;
  }

  function deepAll(selector) {
    var found = [];
    var stack = [document];
    var scanned = 0;
    while (stack.length && scanned < MAX_NODES) {
      var root = stack.pop();
      var every = root.querySelectorAll('*');
      scanned += every.length;
      var matches = root.querySelectorAll(selector);
      for (var i = 0; i < matches.length; i++) found.push(matches[i]);
      for (var j = 0; j < every.length; j++) {
        if (every[j].shadowRoot) stack.push(every[j].shadowRoot);
      }
    }
    return found;
  }

  function parentOf(node) {
    if (node.parentElement) return node.parentElement;
    var root = node.getRootNode && node.getRootNode();
    return root && root.host ? root.host : null;
  }

  // The nearest dialog, or box laid over the page, that the element is in, and
  // the wording and ids on the way up to it.
  function bannerOf(el) {
    var node = el;
    var trail = '';
    for (var i = 0; node && node.nodeType === 1 && i < 14; i++, node = parentOf(node)) {
      var style = getComputedStyle(node);
      var role = node.getAttribute('role') || '';
      trail += ' ' + (node.id || '') + ' ' + (typeof node.className === 'string' ? node.className : '') + ' ' + role;
      var layer = parseInt(style.zIndex, 10);
      var overlay = style.position === 'fixed' || style.position === 'sticky' ||
        (style.position === 'absolute' && layer >= 10);
      var dialog = node.tagName === 'DIALOG' || role === 'dialog' || role === 'alertdialog' ||
        node.getAttribute('aria-modal') === 'true';
      if (overlay || dialog) {
        return { inBanner: true, context: clean(trail + ' ' + (node.innerText || node.textContent || ''), 600) };
      }
    }
    return { inBanner: false, context: clean(trail, 200) };
  }

  function nameOf(el) {
    return clean(el.getAttribute('aria-label') || el.innerText || el.value || el.getAttribute('title') || el.textContent, 80);
  }

  function collectButtons() {
    var out = [];
    var candidates = deepAll('button,[role=button],a[href],input[type=button],input[type=submit]');
    for (var i = 0; i < candidates.length && out.length < MAX_BUTTONS; i++) {
      var el = candidates[i];
      var quick = el.getBoundingClientRect();
      if (quick.width < 10 || quick.height < 8 || quick.bottom < 0 || quick.top > vh || quick.right < 0 || quick.left > vw) continue;
      var name = nameOf(el);
      if (!name || name.length > 70) continue;
      var box = bestBox(el);
      if (!box || !shows(el) || !reachable(el, box)) continue;
      var banner = bannerOf(el);
      if (!banner.inBanner) continue;
      out.push({ name: name, rect: box, inBanner: true, context: banner.context });
    }
    return out;
  }

  function collectHover() {
    var out = [];
    var els = document.querySelectorAll('h1,h2,h3,p,img,figure,li');
    var seen = Math.min(els.length, 700);
    for (var i = 0; i < seen && out.length < MAX_HOVER; i++) {
      var box = bestBox(els[i]);
      if (!box || box.w * box.h < 600 || box.w * box.h > vw * vh * 0.5) continue;
      if (!shows(els[i])) continue;
      out.push(box);
    }
    return out;
  }

  // How much text is on screen, in characters.
  function visibleChars() {
    var total = 0;
    var els = document.querySelectorAll('p,li,h1,h2,h3,h4,h5,h6,blockquote,td,dd,dt,figcaption,pre');
    var seen = Math.min(els.length, 800);
    for (var i = 0; i < seen && total < 20000; i++) {
      var rect = els[i].getBoundingClientRect();
      if (rect.height < 4 || rect.bottom < 0 || rect.top > vh) continue;
      var shown = Math.min(rect.bottom, vh) - Math.max(rect.top, 0);
      var fraction = Math.max(0, Math.min(1, shown / rect.height));
      total += Math.min((els[i].textContent || '').length, 3000) * fraction;
    }
    return Math.round(total);
  }

  var result = {
    url: location.href,
    vw: vw,
    vh: vh,
    dpr: window.devicePixelRatio || 1,
    scrollY: window.scrollY || window.pageYOffset || 0,
    scrollH: Math.max(doc ? doc.scrollHeight : 0, document.body ? document.body.scrollHeight : 0),
    visibleChars: visibleChars(),
    links: collectLinks(),
    hover: collectHover(),
    buttons: options && options.consent ? collectButtons() : []
  };
  return result;
})
