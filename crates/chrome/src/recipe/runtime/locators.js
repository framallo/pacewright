// locators.js — the recipe engine's Playwright-style locator runtime.
//
// Injected once per session via Page.addScriptToEvaluateOnNewDocument so `__pw` exists on every
// document (surviving navigations). A recipe names locators as *data*; this engine-shipped,
// reviewed-and-versioned runtime resolves them. A downloaded recipe can therefore only name a
// locator, never run code.
//
// Dual-mode: in a browser it attaches `__pw` to the global; under Node (jsdom tests) it also
// exports the API and every function takes an explicit `doc` so tests can pass a jsdom document.
//
// A locator spec is the JSON serialization of the Rust `Locator` (src/recipe/model.rs):
//   { role?, name?, text?, label?, tag?, css?, level?, nth?, within?, fallback?, after?, near? }
// `text`/`name`/`label` match a `/regex/flags` raw string as a regex, else as a substring.
//
// API:
//   __pw.resolve(spec, doc?) -> { found, count, text }
//   __pw.extract(spec, many, doc?) -> string | string[] | null

(function (root) {
  function asArray(nodes) {
    return Array.prototype.slice.call(nodes);
  }

  function normText(el) {
    return ((el && el.textContent) || "").replace(/\s+/g, " ").trim();
  }

  // Best-effort visibility: jsdom has no layout, so we can only honor explicit hiding.
  function isVisible(el) {
    if (!el) return false;
    if (el.closest && el.closest('[hidden], [aria-hidden="true"]')) return false;
    var style = el.getAttribute && el.getAttribute("style");
    if (style && /display\s*:\s*none|visibility\s*:\s*hidden/i.test(style)) return false;
    return true;
  }

  var ROLE_BY_TAG = {
    a: "link", // only with href; handled below
    button: "button",
    nav: "navigation",
    main: "main",
    header: "banner",
    footer: "contentinfo",
    article: "article",
    section: "region",
    ul: "list",
    ol: "list",
    li: "listitem",
    table: "table",
    tr: "row",
    img: "img",
    input: "textbox", // rough; refine by type if needed
  };

  function roleOf(el) {
    var explicit = el.getAttribute && el.getAttribute("role");
    if (explicit) return explicit;
    var tag = el.tagName ? el.tagName.toLowerCase() : "";
    if (/^h[1-6]$/.test(tag)) return "heading";
    if (tag === "a") return el.hasAttribute && el.hasAttribute("href") ? "link" : null;
    return ROLE_BY_TAG[tag] || null;
  }

  function headingLevel(el) {
    var tag = el.tagName ? el.tagName.toLowerCase() : "";
    var m = /^h([1-6])$/.exec(tag);
    if (m) return parseInt(m[1], 10);
    var aria = el.getAttribute && el.getAttribute("aria-level");
    return aria ? parseInt(aria, 10) : null;
  }

  // Accessible name, roughly: aria-label > alt > trimmed text.
  function accessibleName(el) {
    var aria = el.getAttribute && el.getAttribute("aria-label");
    if (aria) return aria.trim();
    var alt = el.getAttribute && el.getAttribute("alt");
    if (alt) return alt.trim();
    return normText(el);
  }

  // Return a predicate for a `/regex/flags` string, else a substring test. `null` if no matcher.
  function stringMatcher(pattern) {
    if (pattern == null) return null;
    var m = /^\/(.*)\/([a-z]*)$/.exec(pattern);
    if (m) {
      var re = new RegExp(m[1], m[2]);
      return function (s) {
        return re.test(s);
      };
    }
    return function (s) {
      return s.indexOf(pattern) !== -1;
    };
  }

  function matches(el, spec) {
    if (spec.role && roleOf(el) !== spec.role) return false;
    if (spec.tag && (el.tagName || "").toLowerCase() !== spec.tag.toLowerCase()) return false;
    if (spec.level != null && headingLevel(el) !== spec.level) return false;
    var tm = stringMatcher(spec.text);
    if (tm && !tm(normText(el))) return false;
    var nm = stringMatcher(spec.name);
    if (nm && !nm(accessibleName(el))) return false;
    var lm = stringMatcher(spec.label);
    if (lm && !lm(accessibleName(el))) return false;
    return true;
  }

  // Document order rank, for relative anchors and stable ordering.
  function orderIndex(el, doc) {
    var all = doc.querySelectorAll("*");
    for (var i = 0; i < all.length; i++) {
      if (all[i] === el) return i;
    }
    return -1;
  }

  function follows(anchor, el) {
    // el comes strictly after anchor in document order.
    return !!(
      anchor.compareDocumentPosition(el) &
      (root.Node ? root.Node.DOCUMENT_POSITION_FOLLOWING : 4)
    );
  }

  function resolveList(spec, doc) {
    // Scope root (within): if the scope itself can't be resolved, nothing matches.
    var scope = doc;
    if (spec.within) {
      scope = resolveOne(spec.within, doc);
      if (!scope) return [];
    }

    var base = spec.css ? asArray(scope.querySelectorAll(spec.css)) : asArray(scope.querySelectorAll("*"));
    var els = base.filter(function (el) {
      return isVisible(el) && matches(el, spec);
    });

    // getByText/name/label semantics: a text predicate also matches every ancestor container
    // (its textContent ends the same way). Keep only the innermost matches — drop any element
    // that contains another matched element.
    if (spec.text || spec.name || spec.label) {
      els = els.filter(function (el) {
        return !els.some(function (other) {
          return other !== el && el.contains(other);
        });
      });
    }

    if (spec.after) {
      var afterAnchor = resolveOne(spec.after, doc);
      if (!afterAnchor) return [];
      els = els.filter(function (el) {
        return follows(afterAnchor, el);
      });
    }

    if (spec.near) {
      var nearAnchor = resolveOne(spec.near, doc);
      if (!nearAnchor) return [];
      var ai = orderIndex(nearAnchor, doc);
      els.sort(function (a, b) {
        return Math.abs(orderIndex(a, doc) - ai) - Math.abs(orderIndex(b, doc) - ai);
      });
    }

    // Fallback only when the primary resolved nothing.
    if (els.length === 0 && spec.fallback) {
      return resolveList(spec.fallback, doc);
    }

    if (spec.nth != null) {
      var picked = els[spec.nth];
      return picked ? [picked] : [];
    }
    return els;
  }

  function resolveOne(spec, doc) {
    return resolveList(spec, doc)[0] || null;
  }

  // Monotonic tag counter for mark(): stable within a document, no Math.random needed.
  var markSeq = 0;

  var __pw = {
    resolve: function (spec, doc) {
      doc = doc || root.document;
      var els = resolveList(spec, doc);
      var first = els[0] || null;
      return { found: !!first, count: els.length, text: first ? normText(first) : null };
    },
    extract: function (spec, many, doc) {
      doc = doc || root.document;
      var els = resolveList(spec, doc);
      if (many) return els.map(normText);
      return els[0] ? normText(els[0]) : null;
    },
    // Resolve one element and tag it with a unique `data-pw-recipe` attribute, returning a CSS
    // selector that targets exactly it. This is the seam for write verbs: the engine resolves a
    // locator *here* (semantic/relative tiers), then hands the returned selector to chrome-agent's
    // real-CDP-input `*_selector` functions. Returns null if the locator resolved nothing.
    mark: function (spec, doc) {
      doc = doc || root.document;
      var el = resolveOne(spec, doc);
      if (!el) return null;
      var id = "pw-" + markSeq++;
      el.setAttribute("data-pw-recipe", id);
      return '[data-pw-recipe="' + id + '"]';
    },
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = __pw;
  }
  if (root) {
    root.__pw = __pw;
  }
})(typeof globalThis !== "undefined" ? globalThis : this);
