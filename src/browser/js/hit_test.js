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
  // A select is set in code, not clicked. A page often draws its own prompt
  // over it (Amazon's "Sort by"), so the select need not be topmost, but the
  // point must land inside its own wrapper: a modal on top refuses it.
  const select=e.tagName==='SELECT';
  const hit=c.hit(x,y);
  if (!select && !c.contains(e,hit)) return null;
  if (select) {
    const wrapper=e.parentElement?.parentElement || e.parentElement || e;
    if (!c.contains(e,hit) && !c.contains(wrapper,hit)) return null;
    // The value setter takes the first option with this value: check that one.
    const option=[...e.options].find(o=>o.value===action.value);
    if (!option || option.disabled || option.closest('optgroup[disabled]')) return null;
    e.value=action.value;
    e.dispatchEvent(new Event('input',{bubbles:true}));
    e.dispatchEvent(new Event('change',{bubbles:true}));
    if (e.value!==action.value) return null;
  }
  return {x,y};
})
