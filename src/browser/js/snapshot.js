(() => {
  if (!document.body) return null;
  const cache = window.__jevFast ||= {ids:new WeakMap(), nodes:new Map(), next:1};
  const identity = e => {
    if (!cache.ids.has(e)) cache.ids.set(e,cache.next++);
    const id=cache.ids.get(e); cache.nodes.set(id,e); return id;
  };
  for (const [id,e] of cache.nodes) if (!e.isConnected) cache.nodes.delete(id);

  // Every tree the page is made of: the document, each open shadow root, and
  // each same-origin frame's document, recursively. A cross-origin frame (a
  // card field, an ad) and a closed shadow root are not readable from here,
  // which also keeps payment iframes out of the agent's reach.
  const MAX_ROOTS=200;
  cache.roots=()=>{
    const out=[];
    const visit=root=>{
      if (out.length>=MAX_ROOTS) return;
      out.push(root);
      for (const el of root.querySelectorAll('*')) {
        if (el.shadowRoot) visit(el.shadowRoot);
        if (el.tagName==='IFRAME' || el.tagName==='FRAME') {
          let doc=null;
          try { doc=el.contentDocument; } catch (_) { doc=null; }
          if (doc?.documentElement) visit(doc);
        }
      }
    };
    visit(document);
    return out;
  };
  const all=(roots,selector)=>roots.flatMap(root=>[...root.querySelectorAll(selector)]);
  // The next node up the composed tree: through slots, shadow hosts, and
  // from a frame's document to its frame element.
  const up=n=>{
    if (n.assignedSlot) return n.assignedSlot;
    const p=n.parentNode;
    if (!p) return null;
    if (p.nodeType===11 && p.host) return p.host;
    if (p.nodeType===9) { try { return p.defaultView?.frameElement||null; } catch (_) { return null; } }
    return p;
  };
  cache.closest=(e,selector)=>{
    for (let n=e; n; n=up(n)) if (n.nodeType===1 && n.matches(selector)) return n;
    return null;
  };
  cache.contains=(e,hit)=>{
    for (let n=hit; n; n=up(n)) if (n===e) return true;
    return false;
  };
  // An element's rectangle in the top viewport, and the frames it sits in.
  cache.rect=e=>{
    const r=e.getBoundingClientRect();
    let x=r.x, y=r.y, doc=e.ownerDocument;
    // Inside its own frame's viewport, not scrolled out of the frame.
    const view=doc.defaultView, cx=r.x+r.width/2, cy=r.y+r.height/2;
    const inside=doc===document || (cx>=0 && cy>=0 && cx<view.innerWidth && cy<view.innerHeight);
    while (doc!==document) {
      const frame=doc.defaultView?.frameElement;
      if (!frame) return null;
      const f=frame.getBoundingClientRect();
      x+=f.x+frame.clientLeft; y+=f.y+frame.clientTop;
      doc=frame.ownerDocument;
    }
    return {x,y,width:r.width,height:r.height,inside};
  };
  // elementFromPoint through shadow roots and same-origin frames.
  cache.hit=(x,y)=>{
    let root=document, px=x, py=y, el=null;
    for (let depth=0; depth<32; depth++) {
      const found=root.elementFromPoint(px,py);
      if (!found) return el;
      el=found;
      if (found.shadowRoot && found.shadowRoot!==root) {
        const inner=found.shadowRoot.elementFromPoint(px,py);
        if (inner && inner!==found) { root=found.shadowRoot; continue; }
      }
      if (found.tagName==='IFRAME' || found.tagName==='FRAME') {
        let doc=null;
        try { doc=found.contentDocument; } catch (_) { doc=null; }
        if (doc?.documentElement) {
          const f=found.getBoundingClientRect();
          px-=f.x+found.clientLeft; py-=f.y+found.clientTop; root=doc; continue;
        }
      }
      return el;
    }
    return el;
  };

  const safe = e => !['password','file','hidden'].includes(e.type);
  // Custom-styled checkboxes, radios and selects keep the real control in the
  // page at opacity 0 (or 0.01) under or over a drawn one, so an opacity check
  // would hide every one of them.
  const skinned = e => ['checkbox','radio'].includes(e.type) || e.tagName==='SELECT';
  const frameShown = doc => {
    while (doc!==document) {
      const frame=doc.defaultView?.frameElement;
      if (!frame || !frame.checkVisibility({checkOpacity:true,checkVisibilityCSS:true})) return false;
      doc=frame.ownerDocument;
    }
    return true;
  };
  const visible = e => !cache.closest(e,'[aria-hidden="true"],[inert]') &&
    e.checkVisibility({checkOpacity:!skinned(e),checkVisibilityCSS:true}) && frameShown(e.ownerDocument);
  cache.visible=visible;
  const byId=(e,id)=>{ const root=e.getRootNode(); return (root.getElementById ? root.getElementById(id) : null) || e.ownerDocument.getElementById(id); };
  const name = (e,seen=new Set()) => {
    if (!e || seen.has(e)) return '';
    seen.add(e);
    const referenced=(e.getAttribute('aria-labelledby')||'').split(/\s+/).filter(Boolean)
      .map(id=>name(byId(e,id),seen)).filter(Boolean).join(' ');
    const children=n=>[...n.childNodes].map(c=>c.nodeType===3 ? c.textContent :
      c.nodeType===1 && c.getAttribute('aria-hidden')!=='true' ? name(c,seen) : '').join(' ').trim();
    return referenced || e.getAttribute('aria-label') ||
      [...(e.labels||[])].map(l=>name(l,seen)).filter(Boolean).join(' ') ||
      (['button','submit','reset'].includes(e.type) ? e.value : '') || e.getAttribute('alt') ||
      (e.tagName==='INPUT' ? '' : children(e) || (e.shadowRoot ? children(e.shadowRoot) : '')) ||
      e.getAttribute('title') || e.getAttribute('placeholder') || '';
  };
  const roles=['button','link','checkbox','radio','switch','tab','menuitem','menuitemradio',
    'menuitemcheckbox','option','treeitem','gridcell','combobox','textbox','searchbox','spinbutton'];
  const selector='a[href],button,input,textarea,select,summary,[contenteditable="true"],'+
    roles.map(role=>'[role="'+role+'"]').join(',');
  const role = e => {
    const explicit=e.getAttribute('role');
    if (roles.includes(explicit)) return explicit;
    if (e.tagName==='BUTTON' || e.tagName==='SUMMARY') return 'button';
    if (e.tagName==='A') return 'link';
    if (e.tagName==='SELECT') return 'combobox';
    if (e.tagName==='TEXTAREA' || e.isContentEditable) return 'textbox';
    if (e.tagName==='INPUT') {
      if (['checkbox','radio'].includes(e.type)) return e.type;
      if (['button','submit','reset','image'].includes(e.type)) return 'button';
      if (e.type==='search') return 'searchbox';
      if (e.type==='number') return 'spinbutton';
      if (['text','email','url','tel'].includes(e.type)) return 'textbox';
    }
    if (cache.generic?.has(e)) return 'button';
    return null;
  };
  const roots=cache.roots();
  // Clickable without saying so: an onclick handler, a focusable tabindex, or
  // the top of a pointer-cursor region, with no real control inside or around
  // it. Many sites build buttons and menu items out of plain divs.
  const MAX_GENERIC=300;
  cache.generic=new WeakSet();
  let generic=0;
  for (const e of all(roots,'*')) {
    if (generic>=MAX_GENERIC) break;
    if (e.matches(selector) || ['HTML','BODY','IFRAME','FRAME','LABEL','SVG','PATH','IMG'].includes(e.tagName.toUpperCase())) continue;
    const r=e.getBoundingClientRect();
    if (r.width<=0 || r.height<=0 || r.width*r.height>200000) continue;
    const tab=e.getAttribute('tabindex');
    let clickable=e.hasAttribute('onclick') || (tab!==null && Number(tab)>=0);
    if (!clickable) {
      const view=e.ownerDocument.defaultView;
      const cursor=view.getComputedStyle(e).cursor;
      const parent=up(e);
      clickable=cursor==='pointer' && !(parent?.nodeType===1 && view.getComputedStyle(parent).cursor==='pointer');
    }
    if (!clickable || e.querySelector(selector) || (up(e) && cache.closest(up(e),selector))) continue;
    const label=name(e);
    if (!label || label.length>200) continue;
    cache.generic.add(e); generic++;
  }
  cache.pageKey=()=>[performance.timeOrigin,location.href,scrollX,scrollY,innerWidth,innerHeight,
    all(cache.roots(),'input,textarea,select').filter(safe)
      .map(e=>[identity(e),e.value,e.checked,e.selectedIndex,e.disabled,e.readOnly])];
  cache.guard=e=>{
    if (!e?.isConnected || !visible(e)) return null;
    const scope=cache.closest(e,'form,dialog,[role="dialog"],article,li,tr,[role="row"]') || up(e);
    return [identity(e),role(e),name(e),e.value??null,e.checked??null,e.selectedIndex??null,
      e.readOnly??null,e.matches(':disabled'),e.getAttribute('aria-disabled'),
      e.getAttribute('aria-expanded'),e.getAttribute('aria-checked'),e.getAttribute('aria-selected'),
      e.getAttribute('href'),scope?.innerText?.slice(0,6000)||''];
  };
  const candidates=all(roots,selector);
  for (const root of roots) for (const e of root.querySelectorAll('*')) if (cache.generic.has(e)) candidates.push(e);
  const actions=[];
  for (const e of candidates) {
    if (!safe(e) || !visible(e) || e.matches(':disabled') || cache.closest(e,'[aria-disabled="true"]')) continue;
    const r=cache.rect(e), rname=role(e);
    if (!r || !r.inside || !rname) continue;
    const x=r.x+r.width/2, y=r.y+r.height/2;
    if (r.width<=0 || r.height<=0 || x<0 || y<0 || x>=innerWidth || y>=innerHeight) continue;
    if (rname==='gridcell' && e.querySelector('button,[role="button"]')) continue;
    const base={node:identity(e),role:rname,label:name(e)||rname,
      rect:{x:r.x,y:r.y,w:r.width,h:r.height}};
    // The guards read this to tell a card or password field from an address one.
    const autocomplete=e.getAttribute('autocomplete');
    if (autocomplete) base.autocomplete=autocomplete;
    for (const key of ['checked','selected','expanded']) {
      const value=e.getAttribute('aria-'+key);
      if (value!==null) base[key]=value;
    }
    if (['checkbox','radio'].includes(e.type)) base.checked=String(e.checked);
    if (e.tagName==='SELECT') {
      for (const o of e.options) if (!o.selected && !o.disabled && !o.closest('optgroup[disabled]'))
        actions.push({...base,kind:'select',value:o.value,
          current_value:[...e.selectedOptions].map(o=>o.label).join(', '),label:base.label+' → '+o.label});
    } else {
      const editable=!e.readOnly && e.getAttribute('aria-readonly')!=='true' &&
        (['textbox','searchbox','spinbutton'].includes(rname) ||
          (rname==='combobox' && ['INPUT','TEXTAREA'].includes(e.tagName)));
      const value='value' in e ? String(e.value) :
        e.isContentEditable || rname==='combobox' ? e.innerText.trim() : '';
      actions.push({...base,kind:editable?'fill':'click',value});
      if (editable) actions.push({...base,kind:'click',value,label:'Open '+base.label});
    }
  }
  const words=[]; let length=0;
  for (const root of roots) {
    const top=root.nodeType===9 ? root.body : root;
    if (!top || length>=6000) continue;
    const doc=root.nodeType===9 ? root : root.ownerDocument;
    if (!frameShown(doc)) continue;
    const walker=doc.createTreeWalker(top,NodeFilter.SHOW_TEXT), range=doc.createRange();
    let node;
    while ((node=walker.nextNode()) && length<6000) {
      const value=node.textContent.trim(), parent=node.parentElement;
      if (!value || !parent || parent.closest('script,style,noscript,template') || !visible(parent)) continue;
      range.selectNodeContents(node);
      const local=range.getBoundingClientRect(), r=cache.rect(parent);
      if (!r || !r.inside) continue;
      const dx=r.x-parent.getBoundingClientRect().x, dy=r.y-parent.getBoundingClientRect().y;
      const left=local.left+dx, top2=local.top+dy;
      if (local.width>0 && local.height>0 && top2+local.height>0 && top2<innerHeight && left+local.width>0 && left<innerWidth) {
        words.push(value); length+=value.length;
      }
    }
  }
  const text=words.join('\n').slice(0,6000), height=document.documentElement.scrollHeight;
  const page_key=cache.pageKey(), guards={};
  for (const a of actions) if (!(a.node in guards)) guards[a.node]=cache.guard(cache.nodes.get(a.node));
  // Compare meaning and identity. Geometry is always resolved and hit-tested just before input.
  const semantics=actions.map(({rect,...action})=>action);
  const marker=[performance.timeOrigin,location.href,scrollX,scrollY,innerWidth,innerHeight,
    document.title,text,semantics,page_key[6]];
  const omitted_actions=Math.max(0,actions.length-250);
  actions.splice(250);
  actions.forEach((a,i)=>a.id='e'+(i+1));
  if (scrollY+innerHeight<height-2) actions.push({id:'scroll_down',kind:'scroll',label:'Scroll down',delta:560});
  if (scrollY>0) actions.push({id:'scroll_up',kind:'scroll',label:'Scroll up',delta:-560});
  actions.push({id:'wait',kind:'wait',label:'Wait for the page to update'});
  return {url:location.href,title:document.title,w:innerWidth,h:innerHeight,text,
    scroll:{y:scrollY,height},actions,marker,page_key,guards,omitted_actions};
})()
