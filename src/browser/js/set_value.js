// Sets a native date or time input's value in code, the way a person's pick
// in the browser's own picker would, and reports whether the input kept it.
// The value's format is checked in Rust first.
(([node, value]) => {
  const e=window.__jevFast?.nodes.get(node);
  if (!e || e.tagName!=='INPUT' || e.disabled || e.readOnly) return false;
  const setter=Object.getOwnPropertyDescriptor(e.ownerDocument.defaultView.HTMLInputElement.prototype,'value').set;
  setter.call(e,value);
  e.dispatchEvent(new Event('input',{bubbles:true}));
  e.dispatchEvent(new Event('change',{bubbles:true}));
  return e.value===value;
})
