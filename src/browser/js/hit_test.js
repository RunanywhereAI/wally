// Resolves an observed node to the top-viewport point to click, or null when
// it is gone, disabled, covered, or off screen. Works through shadow roots and
// same-origin frames with the snapshot's helpers. A select is set here and
// confirmed by the returned point. Node ids are the snapshot's own; nothing
// here takes a model-written selector.
(action => {
  const c=window.__jevFast, e=c?.nodes.get(action.node);
  if (!e?.isConnected || e.matches(':disabled') || c.closest(e,'[aria-disabled="true"],[inert]') ||
      !c.visible(e)) return null;
  if (action.kind==='fill' && (e.readOnly || e.getAttribute('aria-readonly')==='true')) return null;
  const r=c.rect(e);
  if (!r || !r.inside) return null;
  const x=r.x+r.width/2, y=r.y+r.height/2;
  if (!r.width || !r.height || x<0 || y<0 || x>=innerWidth || y>=innerHeight) return null;
  // A select is set in code, not clicked, so it need not be the topmost element.
  const select=e.tagName==='SELECT';
  if (!select && !c.contains(e,c.hit(x,y))) return null;
  if (select) {
    if (![...e.options].some(o=>o.value===action.value && !o.disabled && !o.closest('optgroup[disabled]')))
      return null;
    e.value=action.value;
    e.dispatchEvent(new Event('input',{bubbles:true}));
    e.dispatchEvent(new Event('change',{bubbles:true}));
  }
  return {x,y};
})
