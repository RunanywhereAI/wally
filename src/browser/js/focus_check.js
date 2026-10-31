// After the click on a field and before any keystroke: which element will
// receive the text? Followed through shadow roots and same-origin frames. A
// page can move focus in a click or focus handler (Google's search box opens
// a larger input), so a different element is described for the guards to
// judge again; a cross-origin frame cannot be described and is refused.
(node => {
  const c=window.__jevFast, target=c?.nodes.get(node);
  let active=document.activeElement;
  for (let depth=0; active && depth<32; depth++) {
    if (active.shadowRoot?.activeElement) { active=active.shadowRoot.activeElement; continue; }
    if (active.tagName==='IFRAME' || active.tagName==='FRAME') {
      let doc=null;
      try { doc=active.contentDocument; } catch (_) { doc=null; }
      if (!doc) return {focus:'cross-origin frame'};
      active=doc.activeElement;
      continue;
    }
    break;
  }
  if (target && active===target) return {focus:'same'};
  if (!active || active===active.ownerDocument.body) return {focus:'nothing'};
  const text=['text','search','email','url','tel','number',''].includes(active.type ?? '') &&
    (['INPUT','TEXTAREA'].includes(active.tagName) || active.isContentEditable) &&
    !active.readOnly && !active.disabled;
  return {focus:'other', text, type:active.type ?? '',
    label:(c?.name ? c.name(active) : '') || active.getAttribute('aria-label') || '',
    autocomplete:active.getAttribute('autocomplete') || ''};
})
