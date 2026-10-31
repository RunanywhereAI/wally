// The page key and one node's guard tuple, read again just before a click or
// select. Equal to what the snapshot recorded means the decision still holds.
(node => { const c=window.__jevFast; return c ? [c.pageKey(),c.guard(c.nodes.get(node))] : null; })
