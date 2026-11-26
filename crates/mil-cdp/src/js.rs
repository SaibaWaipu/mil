//! JavaScript used to turn a fully rendered DOM into flat, document-space
//! rectangles.
//!
//! The CDP `DOMSnapshot` domain returns large string tables that are awkward to
//! correlate with text runs. A `TreeWalker` inside the page is both smaller and
//! more faithful to what the user sees: each text node is measured with a
//! `Range`, so CSS transforms, pseudo-layout, and JS-injected content are all
//! already accounted for. All rectangles are reported in *document* pixel
//! coordinates (viewport position plus scroll offset) so the rest of the crate
//! can map them onto cells without knowing where the user has scrolled.

use serde::Deserialize;

use crate::CdpSpan;

/// Result of [`EXTRACT_JS`], serialised by `JSON.stringify` in the page.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Extracted {
    #[serde(default)]
    pub spans: Vec<CdpSpan>,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub scroll_x: f64,
    #[serde(default)]
    pub scroll_y: f64,
}

/// IIFE that returns a JSON string describing every visible text run and image.
///
/// Long text nodes are split into per-line runs by measuring one character at
/// a time; this keeps a wrapped paragraph from being stamped onto every line
/// rectangle. The span count is capped so a pathological page cannot stall the
/// CDP connection.
pub(crate) const EXTRACT_JS: &str = r##"(() => {
  const sx = window.scrollX || 0;
  const sy = window.scrollY || 0;
  const doc = document.documentElement || {};
  const body = document.body || doc;
  const out = {
    spans: [],
    width: Math.max(1, Math.floor(Math.max(doc.scrollWidth || 0, body.scrollWidth || 0, window.innerWidth || 0))),
    height: Math.max(1, Math.floor(Math.max(doc.scrollHeight || 0, body.scrollHeight || 0, window.innerHeight || 0))),
    title: document.title || '',
    url: location.href || '',
    scroll_x: sx,
    scroll_y: sy
  };
  const MAX = 20000;
  const skip = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS']);
  const emit = (text, r, parent, fg, bg, bold, italic, underline, href, src, alt) => {
    if (out.spans.length >= MAX) return;
    if (!r || !(r.width > 0) || !(r.height > 0)) return;
    out.spans.push({
      text: text,
      x: r.left + sx, y: r.top + sy, w: r.width, h: r.height,
      fg: fg, bg: bg, bold: bold, italic: italic, underline: underline,
      href: href, src: src, alt: alt
    });
  };
  let walker = null;
  try {
    walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        const p = node.parentElement;
        if (!p || skip.has(p.tagName)) return NodeFilter.FILTER_REJECT;
        try {
          const cs = getComputedStyle(p);
          if (cs.display === 'none' || cs.visibility === 'hidden' || cs.visibility === 'collapse') {
            return NodeFilter.FILTER_REJECT;
          }
        } catch (_) { /* keep going */ }
        return NodeFilter.FILTER_ACCEPT;
      }
    });
  } catch (_) { return JSON.stringify(out); }

  const nodeFilterAccept = (node) => {
    try { return !!node.nodeValue && node.nodeValue.trim().length > 0; }
    catch (_) { return false; }
  };
  let node = null;
  try {
    while (out.spans.length < MAX && (node = walker.nextNode()) !== null) {
      if (!nodeFilterAccept(node)) continue;
      const text = String(node.nodeValue || '');
      const parent = node.parentElement;
      if (!parent) continue;
      let cs;
      try { cs = getComputedStyle(parent); } catch (_) { cs = null; }
      let anchor = null;
      try { anchor = parent.closest ? parent.closest('a[href]') : null; } catch (_) { anchor = null; }
      const href = anchor ? (anchor.href || null) : null;
      const fg = cs ? (cs.color || null) : null;
      const bg = cs ? (cs.backgroundColor || null) : null;
      const weight = cs ? String(cs.fontWeight || '') : '';
      const bold = weight === 'bold' || weight === 'bolder' || Number(weight) >= 600;
      const italic = cs ? /italic|oblique/.test(String(cs.fontStyle || '')) : false;
      const decoration = cs ? String(cs.textDecorationLine || cs.textDecoration || '') : '';
      const underline = /underline/.test(decoration);

      let range = null;
      try {
        range = document.createRange();
        range.selectNodeContents(node);
      } catch (_) { continue; }
      let rects;
      try { rects = range.getClientRects(); } catch (_) { continue; }
      if (!rects || rects.length === 0) continue;

      if (rects.length === 1) {
        emit(text, rects[0], parent, fg, bg, bold, italic, underline, href, null, null);
        continue;
      }
      // Multi-line text node: group characters by their line box.
      let line = '';
      let top = null;
      let left = 0;
      let right = 0;
      let height = 0;
      let pos = 0;
      const one = document.createRange();
      for (const ch of text) {
        try {
          one.setStart(node, pos);
          one.setEnd(node, pos + ch.length);
        } catch (_) { pos += ch.length; continue; }
        pos += ch.length;
        const r = one.getBoundingClientRect();
        if (!(r.width > 0) && !(r.height > 0)) continue;
        if (top === null || Math.abs(r.top - top) > Math.max(2, r.height * 0.4)) {
          if (line) emit(line, {left: left, top: top, width: right - left, height: height}, parent, fg, bg, bold, italic, underline, href, null, null);
          line = '';
          top = r.top;
          left = r.left;
          right = r.right;
          height = r.height;
        }
        line += ch;
        if (r.right > right) right = r.right;
        if (r.height > height) height = r.height;
      }
      if (line) emit(line, {left: left, top: top, width: right - left, height: height}, parent, fg, bg, bold, italic, underline, href, null, null);
    }
  } catch (_) { /* partial extraction is still useful */ }

  try {
    const images = document.querySelectorAll('img');
    for (let i = 0; i < images.length && out.spans.length < MAX; i++) {
      const img = images[i];
      const r = img.getBoundingClientRect();
      if (!(r.width > 0) || !(r.height > 0)) continue;
      let link = null;
      try { link = img.closest ? img.closest('a[href]') : null; } catch (_) { link = null; }
      emit(null, r, img, null, null, false, false, false,
           link ? (link.href || null) : null,
           img.currentSrc || img.src || null,
           img.alt || '');
    }
  } catch (_) { /* images are optional */ }

  return JSON.stringify(out);
})()"##;
