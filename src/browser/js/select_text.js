// Selects a field's current value in code, so the text that follows replaces
// it. A select-all key event would reach the page's keydown handlers, which
// can move focus after the focus check.
(node => {
  const e=window.__jevFast?.nodes.get(node);
  if (!e) return false;
  if (typeof e.select==='function') { e.select(); return true; }
  if (e.isContentEditable) {
    const range=e.ownerDocument.createRange(), selection=e.ownerDocument.getSelection();
    range.selectNodeContents(e); selection.removeAllRanges(); selection.addRange(range);
    return true;
  }
  return false;
})
