(function () {
  'use strict';
  var MAX_ITEMS = 60;
  var MAX_WORDS = 30;
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
    for (var i = 0; node && node.nodeType === 1 && i < 8; i++, node = node.parentElement) {
      var style = getComputedStyle(node);
      if (style.display === 'none') return false;
      if (i === 0 && style.visibility !== 'visible') return false;
      if (parseFloat(style.opacity || '1') < 0.1) return false;
    }
    return true;
  }

  // The box of `el` a pointer can reach now: on screen, shown, and not
  // covered by something else at its centre.
  function reach(el) {
    var box = bestBox(el);
    if (!box || box.w < 6 || box.h < 6 || !shows(el)) return null;
    var top = document.elementFromPoint(box.x + box.w / 2, box.y + box.h / 2);
    var holder = el.closest('a,button');
    if (!top || !(top === el || el.contains(top) || (holder && top.closest && top.closest('a,button') === holder))) return null;
    return box;
  }

  function below(el) {
    return el.getBoundingClientRect().top >= vh;
  }

  function hostOf(href) {
    try {
      var url = new URL(href, location.href);
      if (/(^|\.)google\./.test(url.hostname)) {
        var inner = url.searchParams.get('url') || url.searchParams.get('q');
        if (inner && /^https?:/.test(inner)) return new URL(inner).hostname;
      }
      return url.hostname;
    } catch (e) {
      return '';
    }
  }

  function first(selector, scope) {
    return (scope || document).querySelector(selector);
  }

  function all(selector, scope) {
    return Array.prototype.slice.call((scope || document).querySelectorAll(selector));
  }

  function field() {
    var el = first('#gh-ac, input[name=_nkw], textarea[name=q], input[name=q]');
    if (!el) return null;
    var box = reach(el);
    if (!box) return null;
    var form = el.form || el.closest('form');
    var button = form ? first('#gh-search-btn, #gh-btn, button[type=submit], input[type=submit]', form) : null;
    var submit = button ? reach(button) : null;
    return {
      rect: box,
      value: clean(el.value, 200),
      focused: document.activeElement === el,
      submit: submit
    };
  }

  // The search box's suggestions for what was typed. The shop's carry their
  // text in `data-value` and are read even after the list has closed; any
  // other list counts only while it shows.
  function suggestions() {
    var out = [];
    var seen = {};
    var shop = all('#ebay-autocomplete li[role=option][data-value]');
    // Without suggestions the shop's list holds the person's recent searches.
    if (!shop.length && /(^|\.)ebay\./.test(location.hostname)) return out;
    var options = shop.length ? shop : all('[role=listbox] [role=option]');
    for (var i = 0; i < options.length && out.length < MAX_WORDS; i++) {
      var option = options[i];
      var label = option.getAttribute('data-value') || String(option.innerText || option.textContent || '').split('\n')[0];
      var text = clean(label, 80);
      if (!text || seen[text]) continue;
      var box = reach(option);
      if (!box && !shop.length) continue;
      seen[text] = true;
      out.push({ text: text, rect: box });
    }
    return out;
  }

  // Google's results that lead to the shop, by the address they show.
  function results() {
    var out = [];
    var later = 0;
    var heads = all('#rso a h3, #search a h3');
    for (var i = 0; i < heads.length && out.length < MAX_ITEMS; i++) {
      var a = heads[i].closest('a');
      if (!a) continue;
      var block = a.closest('[data-hveid], .g, .MjjYud') || a;
      var cite = first('cite', block);
      var shown = clean(cite ? cite.innerText || cite.textContent : '', 120).toLowerCase();
      var host = hostOf(a.href).toLowerCase();
      var shop = /(^|\.)ebay\./.test(host) || shown.indexOf('ebay.') !== -1;
      if (!shop) continue;
      if (below(heads[i])) {
        later++;
        continue;
      }
      var box = reach(heads[i]);
      if (!box) continue;
      out.push({ title: clean(heads[i].innerText || heads[i].textContent, 200), rect: box });
    }
    return { found: out, later: later };
  }

  function itemTitle(text) {
    return clean(
      String(text || '')
        .replace(/opens in a new window or tab/gi, ' ')
        .replace(/^\s*(new listing|sponsored)\s*/i, ' '),
      200
    );
  }

  // The shop's listings on screen: their titles and the link that opens them.
  function items() {
    var out = [];
    var seen = {};
    var later = 0;
    var cards = all('ul.srp-results li[data-listingid], ul.srp-results li.s-item, li.s-card[data-listingid]');
    var links = [];
    cards.forEach(function (card) {
      var link = first('a.s-card__link[href*="/itm/"], a.s-item__link[href*="/itm/"], a[href*="/itm/"]', card);
      var title = first('.s-card__title, .s-item__title', card);
      if (link) links.push({ link: link, title: title ? title.innerText || title.textContent : link.innerText });
    });
    if (!links.length) {
      all('a[href*="/itm/"]').forEach(function (link) {
        links.push({ link: link, title: link.innerText || link.getAttribute('aria-label') || '' });
      });
    }
    for (var i = 0; i < links.length && out.length < MAX_ITEMS; i++) {
      var match = /\/itm\/(?:[^/?#]*\/)?(\d{6,})/.exec(links[i].link.href);
      if (!match || seen[match[1]]) continue;
      var title = itemTitle(links[i].title);
      if (title.length < 12) continue;
      seen[match[1]] = true;
      if (below(links[i].link)) {
        later++;
        continue;
      }
      var box = reach(links[i].link);
      if (!box) continue;
      out.push({ id: match[1], title: title, rect: box });
    }
    return { found: out, later: later };
  }

  function related() {
    var out = [];
    all('.srp-related-searches a, [class*=related-searches] a').forEach(function (a) {
      var text = clean(a.innerText || a.textContent, 80);
      if (text && out.length < MAX_WORDS) out.push(text);
    });
    return out;
  }

  // An item page: its title, the thumbnails of its pictures, the arrow that
  // shows the next one.
  function item() {
    var heading = first('h1.x-item-title__mainTitle, .x-item-title h1, h1[itemprop=name]');
    if (!heading) return null;
    var pictures = [];
    all('button.ux-image-grid-item, .ux-image-filmstrip-carousel-item, [data-testid=ux-image-grid-item]').forEach(function (thumb) {
      if (pictures.length >= 24) return;
      var box = reach(thumb);
      if (box) pictures.push(box);
    });
    var next = first('button[aria-label^="Next image"], .ux-image-carousel-container button.next, button.btn-next');
    var main = first('.ux-image-carousel, .ux-image-carousel-container');
    return {
      title: itemTitle(heading.innerText || heading.textContent),
      pictures: pictures,
      next: next ? reach(next) : null,
      picture: main ? reach(main) : null
    };
  }

  function challenged() {
    var title = String(document.title || '').toLowerCase();
    var href = location.href.toLowerCase();
    if (/\/splashui\/|captcha|google\.[^/]+\/sorry\//.test(href)) return true;
    if (/pardon our interruption|unusual traffic|are you a robot|verify you are (a )?human/.test(title)) return true;
    if (first('#captcha-form, form[action*="captcha"]')) return true;
    // Sites keep an invisible scoring frame on every page; only a puzzle shown
    // to the person is a check.
    return all('iframe[src*="recaptcha"], iframe[src*="hcaptcha"], iframe[src*="captcha-delivery"]').some(function (frame) {
      var box = frame.getBoundingClientRect();
      return box.width >= 100 && box.height >= 60 && shows(frame);
    });
  }

  // What covers the page and has to be dealt with first: a modal dialog, such
  // as the shop asking where to ship, or a tour bubble pointing at a feature.
  // A dialog's main button is offered only for the shipping question.
  function blocker() {
    var dialogs = all('[role=dialog][aria-modal=true], dialog[open]');
    for (var i = 0; i < dialogs.length; i++) {
      var dialog = dialogs[i];
      var box = dialog.getBoundingClientRect();
      if (box.width < 120 || box.height < 80 || !shows(dialog)) continue;
      var text = String(dialog.innerText || '').toLowerCase();
      var close = first('.lightbox-dialog__close, button[aria-label^="Close"], button[aria-label^="Dismiss"]', dialog);
      var primary = first('.btn--primary', dialog);
      var shipping = /shipping to|ship to|deliver(ing)? to/.test(text);
      return {
        close: close ? reach(close) : null,
        confirm: shipping && primary ? reach(primary) : null
      };
    }
    var tips = all('.tourtip__overlay');
    for (var j = 0; j < tips.length; j++) {
      var tip = tips[j];
      var area = tip.getBoundingClientRect();
      if (area.width < 60 || area.height < 30 || !shows(tip)) continue;
      var shut = first('.tourtip__close', tip);
      var spot = shut ? reach(shut) : null;
      if (spot) return { close: spot, confirm: null };
    }
    return null;
  }

  var googleResults = results();
  var listings = items();
  return {
    url: location.href,
    vw: vw,
    vh: vh,
    dpr: window.devicePixelRatio || 1,
    scrollY: window.scrollY || window.pageYOffset || 0,
    scrollH: Math.max(doc.scrollHeight, document.body ? document.body.scrollHeight : 0),
    challenged: challenged(),
    blocker: blocker(),
    field: field(),
    suggestions: suggestions(),
    results: googleResults.found,
    resultsBelow: googleResults.later,
    items: listings.found,
    itemsBelow: listings.later,
    related: related(),
    item: item()
  };
})()
