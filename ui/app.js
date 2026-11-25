'use strict';
// TubeDesk UI. All data comes from Rust commands; remote text is only ever
// inserted with textContent (never innerHTML).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// ---- helpers -----------------------------------------------------------------

function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'text') el.textContent = v;
    else el.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}
const $ = (id) => document.getElementById(id);

function thumb(list) {
  if (!list || !list.length) return null;
  const ok = list.filter((t) => t.width <= 600);
  const t = (ok.length ? ok : list).reduce((a, b) => (b.width > a.width ? b : a));
  return t.url;
}
function img(list, cls) {
  const src = thumb(list);
  return h('img', { class: cls, src: src || undefined, alt: '', loading: 'lazy', referrerpolicy: 'no-referrer' });
}
function fmt(sec) {
  sec = Math.max(0, Math.floor(sec || 0));
  const m = Math.floor(sec / 60), s = sec % 60;
  if (m >= 60) return `${Math.floor(m / 60)}:${String(m % 60).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  return `${m}:${String(s).padStart(2, '0')}`;
}
let toastTimer;
function toast(msg) {
  const t = $('toast');
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 4000);
}
function showQueue() {
  $('queue').hidden = false;
}
const PLAY_COMMANDS = new Set(['play', 'play_list']);

async function call(cmd, args) {
  if (PLAY_COMMANDS.has(cmd)) showQueue();
  try {
    return await invoke(cmd, args);
  } catch (e) {
    toast(String(e));
    throw e;
  }
}

// ---- state -------------------------------------------------------------------

const state = {
  loggedIn: false,
  now: null,
  queue: { tracks: [], index: null },
  seeking: false,
  current: null,
};

// ---- navigation ----------------------------------------------------------------

const view = $('view');

function navigate(route) {
  state.current = route;
  for (const b of document.querySelectorAll('.nav')) {
    b.classList.toggle('active', b.dataset.view === route.name);
  }
  render(route);
}
// Sidebar collapse, remembered per device (localStorage may be unavailable).
function setSidebar(collapsed) {
  $('app').classList.toggle('sidebar-collapsed', collapsed);
  $('sidebar-toggle').setAttribute('aria-expanded', String(!collapsed));
  $('sidebar-toggle').title = collapsed ? 'Show sidebar' : 'Hide sidebar';
  try { localStorage.setItem('sidebarCollapsed', collapsed ? '1' : '0'); } catch (e) { /* ignore */ }
}
$('sidebar-toggle').addEventListener('click', () => setSidebar(!$('app').classList.contains('sidebar-collapsed')));
try { setSidebar(localStorage.getItem('sidebarCollapsed') === '1'); } catch (e) { setSidebar(false); }
for (const b of document.querySelectorAll('.nav')) {
  b.addEventListener('click', () => navigate({ name: b.dataset.view }));
}
$('search-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const q = $('search-input').value.trim();
  if (q) navigate({ name: 'search', query: q });
});

let renderToken = 0;
async function render(route) {
  const token = ++renderToken;
  const show = (...nodes) => { if (token === renderToken) view.replaceChildren(...nodes); };
  show(h('div', { class: 'spinner', text: 'Loading…' }));
  try {
    switch (route.name) {
    case 'home': return await renderHome(token, show);
    case 'search': return show(...(await renderSearch(route.query)));
    case 'liked': return show(...(await renderLibraryTracks('liked', 'Liked songs')));
    case 'history': return show(...(await renderLibraryTracks('history', 'Recently played')));
    case 'playlists': return show(...(await renderLibraryCards('playlists', 'Playlists', playlistCard)));
    case 'albums': return show(...(await renderLibraryCards('albums', 'Albums', albumCard)));
    case 'artists': return show(...(await renderLibraryCards('artists', 'Artists', artistCard)));
    case 'playlist': return show(...(await renderPlaylist(route.id, route)));
    case 'album': return show(...(await renderAlbum(route.id)));
    case 'artist': return show(...(await renderArtist(route.id)));
    case 'account': return show(...renderAccount());
    }
  } catch (e) {
    show(h('div', { class: 'empty', text: `Could not load: ${e}` }));
  }
}

// ---- components ----------------------------------------------------------------

function artistLinks(artists) {
  const out = [];
  (artists || []).forEach((a, i) => {
    if (i) out.push(', ');
    out.push(a.id
      ? h('span', { class: 'link', text: a.name, onclick: (e) => { e.stopPropagation(); navigate({ name: 'artist', id: a.id }); } })
      : a.name);
  });
  return out;
}

// ---- pins ------------------------------------------------------------------------
// Your own pinned songs, playlists, albums and mixes, shown first on Home.

const pinKey = (kind, id) => `${kind}:${id}`;
const pinned = new Map(); // key → item (same shape as a home item)

function svgIcon(d, cls) {
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', cls);
  const path = document.createElementNS(NS, 'path');
  path.setAttribute('d', d);
  svg.append(path);
  return svg;
}
const PIN_PATH = 'M9 3h6l-1 6 4 3v2h-5v7l-1 1-1-1v-7H6v-2l4-3z';

const trackPinItem = (t) => ({
  kind: 'track', id: t.id, title: t.name,
  subtitle: (t.artists || []).map((a) => a.name).join(', '),
  thumbnails: t.cover || [], track: t,
});

function setPins(list) {
  pinned.clear();
  for (const p of list) pinned.set(pinKey(p.kind, p.id), p);
  for (const b of document.querySelectorAll('[data-pin]')) {
    const on = pinned.has(b.dataset.pin);
    b.classList.toggle('pinned', on);
    b.title = on ? 'Unpin' : 'Pin';
  }
  const slot = $('pinned-slot');
  if (slot) renderPinned(slot);
}

async function togglePin(item) {
  const on = pinned.has(pinKey(item.kind, item.id));
  const list = on ? await call('unpin', { kind: item.kind, id: item.id }) : await call('pin', { item });
  setPins(list);
  toast(on ? `Unpinned ${item.title}` : `Pinned ${item.title}`);
}

function pinButton(item, cls) {
  const key = pinKey(item.kind, item.id);
  const on = pinned.has(key);
  return h('button', {
    class: `pin-btn ${cls}${on ? ' pinned' : ''}`, 'data-pin': key, title: on ? 'Unpin' : 'Pin',
    'aria-label': `Pin ${item.title}`,
    onclick: (e) => { e.stopPropagation(); togglePin(item); },
  }, svgIcon(PIN_PATH, 'pin-icon'));
}

// Pinned lists are the likeliest picks: prepare them in the background at
// start-up (one after another, so the lookups don't all run at once).
async function preparePinned() {
  const lists = [...pinned.values()].filter((p) => ['playlist', 'album', 'mix'].includes(p.kind)).slice(0, 8);
  for (const p of lists) {
    const key = `l:${p.kind}:${p.id}`;
    hinted.set(key, Date.now());
    await invoke('prepare_list', { kind: p.kind, id: p.id }).catch(() => hinted.delete(key));
    // Each list starts two song lookups in the background; let them finish.
    await new Promise((r) => setTimeout(r, 3000));
  }
}

function renderPinned(slot) {
  const items = [...pinned.values()];
  slot.replaceChildren(...(items.length
    ? [h('section', { class: 'home-section' }, h('h2', { text: 'Pinned' }),
        h('div', { class: 'shelf' }, items.map(homeCard).filter(Boolean)))]
    : []));
}

// Hovering a song for a moment prepares its stream in the background, so a
// click right after usually plays immediately.
// key → time of the last hint; hints are repeated once the Rust side's
// prepared state may have expired (10 min).
const hinted = new Map();
const HINT_TTL = 9 * 60 * 1000;
let hoverTimer = null;
function onHover(key, fire) {
  return {
    onmouseenter: () => {
      clearTimeout(hoverTimer);
      if (Date.now() - (hinted.get(key) || 0) < HINT_TTL) return;
      hoverTimer = setTimeout(() => { hinted.set(key, Date.now()); fire(); }, 350);
    },
    onmouseleave: () => clearTimeout(hoverTimer),
  };
}
const hoverPrefetch = (id) => (id ? onHover(`t:${id}`, () => invoke('prefetch', { id })) : {});
// Hovering a playlist/album/mix card loads its first page and warms the first
// song of both its normal and its shuffled order.
const hoverPrepare = (list) => onHover(`l:${list.kind}:${list.id}`, () => {
  invoke('prepare_list', { kind: list.kind, id: list.id }).catch(() => hinted.delete(`l:${list.kind}:${list.id}`));
});

function trackList(tracks) {
  if (!tracks.length) return h('div', { class: 'empty', text: 'No tracks.' });
  const playingId = state.now && state.now.track && state.now.track.id;
  return h('ol', { class: 'tracks' }, tracks.map((t, i) => h('li', {
    class: `track${t.id === playingId ? ' current' : ''}`,
    'data-id': t.id,
    ...hoverPrefetch(t.id),
    // The title link plays on single click; double-clicking elsewhere on the row
    // plays too. Never both, or one double-click on the title would play 3 times.
    ondblclick: (e) => { if (!e.target.closest('.link, button')) call('play', { tracks, start: i }); },
  },
    h('button', { class: 'cover', title: 'Play', 'aria-label': `Play ${t.name}`, onclick: () => call('play', { tracks, start: i }) }, img(t.cover)),
    h('div', { class: 'ellipsis' },
      h('div', { class: 't-title ellipsis link', text: t.name, onclick: () => call('play', { tracks, start: i }) }),
      h('div', { class: 'muted ellipsis' }, artistLinks(t.artists)),
    ),
    h('div', { class: 'muted ellipsis' }, t.album
      ? h('span', { class: 'link', text: t.album.name, onclick: () => navigate({ name: 'album', id: t.album.id }) })
      : ''),
    h('div', { class: 't-dur', text: t.duration ? fmt(t.duration) : '' }),
    h('div', { class: 't-actions' },
      pinButton(trackPinItem(t), 'row'),
      h('button', { class: 'text', title: 'Play next', text: 'Next', onclick: () => { call('enqueue', { tracks: [t], next: true }); toast('Playing next'); } }),
      h('button', { class: 'text', title: 'Add to queue', text: '+ Queue', onclick: () => { call('enqueue', { tracks: [t], next: false }); toast('Added to queue'); } }),
    ),
  )));
}

function shuffledCopy(tracks) {
  const a = [...tracks];
  for (let i = a.length - 1; i > 0; i--) { // Fisher–Yates
    const j = Math.floor(Math.random() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

// The shuffle order is decided when the page opens, and the first song of
// both orders is prepared right away, so Play and Shuffle start instantly.
function playButtons(tracks) {
  if (!tracks.length) return null;
  let order = shuffledCopy(tracks);
  const prepare = () => {
    invoke('prefetch', { id: tracks[0].id, priority: true });
    invoke('prefetch', { id: order[0].id, priority: true });
  };
  prepare();
  return h('div', { class: 'row-actions' },
    h('button', { class: 'btn', text: 'Play', onclick: () => call('play', { tracks, start: 0 }) }),
    h('button', { class: 'btn secondary', text: 'Shuffle', onclick: () => {
      call('play', { tracks: order, start: 0 });
      order = shuffledCopy(tracks); // next Shuffle gets a new order, prepared too
      invoke('prefetch', { id: order[0].id, priority: true });
    } }),
    h('button', { class: 'btn secondary', text: 'Add to queue', onclick: () => { call('enqueue', { tracks, next: false }); toast(`Added ${tracks.length} tracks`); } }),
  );
}

// A card is a div (not a <button>) so it can contain the Play/Shuffle buttons.
//   list: { kind, id } → hover Play + Shuffle buttons that play the whole list
//   playable: show a single ▶ hint (the card itself plays on click)
function card(thumbs, title, sub, onclick, { round = false, list = null, playable = false, trackId = null, pin = null } = {}) {
  const act = (shuffle) => (e) => {
    e.stopPropagation();
    toast(shuffle ? `Shuffling ${title}…` : `Playing ${title}…`);
    call('play_list', { kind: list.kind, id: list.id, shuffle });
  };
  return h('div', {
    class: `card${round ? ' round' : ''}${playable ? ' playable' : ''}`,
    role: 'button', tabindex: '0', title,
    ...(trackId ? hoverPrefetch(trackId) : list ? hoverPrepare(list) : {}),
    onclick,
    onkeydown: (e) => { if (e.key === 'Enter') onclick(); },
  },
    h('div', { class: 'thumb' }, img(thumbs),
      pin && pinButton(pin, 'card'),
      list && h('div', { class: 'card-actions' },
        h('button', { class: 'card-btn', title: 'Play', 'aria-label': `Play ${title}`, text: '▶', onclick: act(false) }),
        h('button', { class: 'card-btn', title: 'Shuffle', 'aria-label': `Shuffle ${title}`, text: '⤮', onclick: act(true) }))),
    h('div', { class: 'ellipsis', text: title }),
    h('div', { class: 'muted ellipsis', text: sub || '' }));
}
const albumCard = (a) => {
  const sub = [a.year, (a.artists || []).map((x) => x.name).join(', ')].filter(Boolean).join(' · ');
  return card(a.cover, a.name, sub, () => navigate({ name: 'album', id: a.id }),
    { list: { kind: 'album', id: a.id }, pin: { kind: 'album', id: a.id, title: a.name, subtitle: sub, thumbnails: a.cover, track: null } });
};
const playlistCard = (p) => {
  const sub = [p.channel && p.channel.name, p.track_count && `${p.track_count} tracks`].filter(Boolean).join(' · ');
  return card(p.thumbnail, p.name, sub, () => navigate({ name: 'playlist', id: p.id, title: p.name, thumbs: p.thumbnail }),
    { list: { kind: 'playlist', id: p.id }, pin: { kind: 'playlist', id: p.id, title: p.name, subtitle: sub, thumbnails: p.thumbnail, track: null } });
};
const artistCard = (a) => card(a.avatar, a.name, 'Artist', () => navigate({ name: 'artist', id: a.id }), { round: true });

function detailHead(thumbs, title, sub) {
  return h('div', { class: 'detail-head' }, img(thumbs), h('div', { class: 'meta' }, h('h1', { text: title }), h('div', { class: 'muted' }, sub)));
}

function needLogin() {
  return [h('div', { class: 'empty' },
    'Sign in to see your YouTube Music library. ',
    h('span', { class: 'link', text: 'Go to Account', onclick: () => navigate({ name: 'account' }) }))];
}

// ---- views ---------------------------------------------------------------------

function homeCard(item) {
  const { thumbnails: th, title, subtitle: sub, id } = item;
  switch (item.kind) {
  case 'playlist': return card(th, title, sub, () => navigate({ name: 'playlist', id, title, thumbs: th }), { list: { kind: 'playlist', id }, pin: item });
  case 'album': return card(th, title, sub, () => navigate({ name: 'album', id }), { list: { kind: 'album', id }, pin: item });
  case 'artist': return card(th, title, sub, () => navigate({ name: 'artist', id }), { round: true });
  case 'mix': return card(th, title, sub, () => { toast(`Playing ${title}…`); call('play_list', { kind: 'mix', id, shuffle: false }); },
    { list: { kind: 'mix', id }, pin: item });
  case 'track': return item.track
    ? card(th, title, sub, () => call('play', { tracks: [item.track], start: 0 }), { playable: true, trackId: id, pin: item })
    : null;
  default: return null;
  }
}

function homeSection(s) {
  const body = s.layout === 'tracks'
    ? trackList(s.items.map((i) => i.track).filter(Boolean))
    : h('div', { class: 'shelf' }, s.items.map(homeCard).filter(Boolean));
  return h('section', { class: 'home-section' }, h('h2', { text: s.title || 'For you' }), body);
}

// Home renders progressively: the last saved copy appears instantly, then the
// live feed replaces it page by page as each page arrives.
async function renderHome(token, show) {
  const header = [h('h1', { text: 'Home' })];
  if (!state.loggedIn) {
    header.push(h('div', { class: 'muted' }, 'Sign in for your mixes and personal recommendations. ',
      h('span', { class: 'link', text: 'Sign in', onclick: () => navigate({ name: 'account' }) })));
  }
  const slot = h('div', { id: 'pinned-slot' });
  renderPinned(slot);
  header.push(slot);
  const stale = () => token !== renderToken;

  const cached = await invoke('home_cached').catch(() => []);
  if (stale()) return;
  if (cached.length) show(...header, ...cached.map(homeSection));

  let page = await call('home_page', { token: null });
  if (stale()) return;
  const live = h('div');
  live.append(...page.sections.map(homeSection));
  show(...header, live);
  if (!page.sections.length && !page.next) {
    show(h('div', { class: 'empty', text: 'YouTube Music returned an empty home page.' }));
    return;
  }
  for (let i = 0; i < 6 && page.next; i++) {
    page = await call('home_page', { token: page.next });
    if (stale()) return;
    live.append(...page.sections.map(homeSection));
  }
}

async function renderSearch(query) {
  if (!query) {
    $('search-input').focus();
    return [h('div', { class: 'empty', text: 'Search YouTube Music.' })];
  }
  const r = await call('search', { query });
  return [
    h('h1', { text: `Results for “${query}”` }),
    h('h2', { text: 'Songs' }), trackList(r.tracks.slice(0, 20)),
    r.albums.length ? [h('h2', { text: 'Albums' }), h('div', { class: 'grid' }, r.albums.slice(0, 12).map(albumCard))] : [],
    r.artists.length ? [h('h2', { text: 'Artists' }), h('div', { class: 'grid' }, r.artists.slice(0, 6).map(artistCard))] : [],
    r.playlists.length ? [h('h2', { text: 'Playlists' }), h('div', { class: 'grid' }, r.playlists.slice(0, 12).map(playlistCard))] : [],
  ].flat();
}

async function renderLibraryTracks(kind, title) {
  if (!state.loggedIn) return needLogin();
  const tracks = await call('library', { kind });
  return [h('h1', { text: title }), playButtons(tracks), trackList(tracks)];
}

async function renderLibraryCards(kind, title, mk) {
  if (!state.loggedIn) return needLogin();
  const items = await call('library', { kind });
  return [h('h1', { text: title }), items.length ? h('div', { class: 'grid' }, items.map(mk)) : h('div', { class: 'empty', text: 'Nothing here yet.' })];
}

async function renderPlaylist(id, fallback = {}) {
  const p = await call('playlist', { id });
  const tracks = p.tracks;
  const thumbs = p.thumbnail.length ? p.thumbnail : (fallback.thumbs || []);
  return [detailHead(thumbs, p.name || fallback.title || 'Mix', [p.channel && p.channel.name, `${p.track_count || tracks.length} tracks`].filter(Boolean).join(' · ')), playButtons(tracks), trackList(tracks)];
}

async function renderAlbum(id) {
  const a = await call('album', { id });
  // Album track items often lack the cover; borrow the album's.
  const tracks = a.tracks.map((t) => (t.cover && t.cover.length ? t : { ...t, cover: a.cover }));
  return [detailHead(a.cover, a.name, [artistLinks(a.artists), a.year ? ` · ${a.year}` : ''].flat()), playButtons(tracks), trackList(tracks)];
}

async function renderArtist(id) {
  const a = await call('artist', { id });
  return [
    detailHead(a.header_image, a.name, a.subscriber_count ? `${a.subscriber_count.toLocaleString()} subscribers` : ''),
    a.tracks.length ? [h('h2', { text: 'Top songs' }), playButtons(a.tracks), trackList(a.tracks)] : [],
    a.albums.length ? [h('h2', { text: 'Albums & singles' }), h('div', { class: 'grid' }, a.albums.map(albumCard))] : [],
    a.similar_artists.length ? [h('h2', { text: 'Similar artists' }), h('div', { class: 'grid' }, a.similar_artists.map(artistCard))] : [],
  ].flat();
}

function renderAccount() {
  const file = h('input', { type: 'file', accept: '.txt,text/plain', hidden: true, onchange: async () => {
    const f = file.files[0];
    if (!f) return;
    const text = await f.text();
    file.value = '';
    await call('auth_import', { cookies: text });
  } });
  if (state.loggedIn) {
    return [h('h1', { text: 'Account' }),
      h('p', { text: 'Signed in. Your YouTube Music library is available in the sidebar.' }),
      h('div', { class: 'row-actions' }, h('button', { class: 'btn secondary', text: 'Sign out', onclick: () => call('auth_logout') }))];
  }
  return [h('h1', { text: 'Account' }),
    h('p', { text: 'Sign in to use your YouTube Music library (liked songs, playlists, albums, artists, history). Search and playback work without an account.' }),
    h('div', { class: 'row-actions' },
      h('button', { class: 'btn', text: 'Sign in with Google', onclick: () => call('auth_login') }),
      h('button', { class: 'btn secondary', text: 'Import cookies.txt…', onclick: () => file.click() }), file),
    h('h2', { text: 'Signing in from your browser instead' }),
    h('p', { class: 'muted', text: 'If Google refuses to sign in here: open a private/incognito window in your browser, sign in at music.youtube.com, export the cookies for youtube.com in Netscape cookies.txt format (e.g. with a "cookies.txt" extension), close that private window, then import the file above. Closing the window matters: YouTube rotates the session while it stays open.' }),
  ];
}

// ---- player bar ------------------------------------------------------------------

$('toggle').addEventListener('click', () => call('control', { action: 'toggle' }));
$('next').addEventListener('click', () => call('control', { action: 'next' }));
$('prev').addEventListener('click', () => call('control', { action: 'previous' }));
const seek = $('seek');
seek.addEventListener('input', () => { state.seeking = true; $('pos').textContent = fmt(seek.value); });
seek.addEventListener('change', () => {
  state.seeking = false;
  // Hold the clicked position until the player reports it, so position
  // updates sent before the seek don't pull the bar back for a moment.
  state.seekHold = { target: Number(seek.value), until: Date.now() + 2000 };
  call('seek', { position: Number(seek.value) });
});
const vol = $('volume');
vol.addEventListener('input', () => call('set_volume', { volume: Number(vol.value), persist: false }));
vol.addEventListener('change', () => call('set_volume', { volume: Number(vol.value), persist: true }));
$('queue-btn').addEventListener('click', () => { $('queue').hidden = !$('queue').hidden; });
$('queue-clear').addEventListener('click', () => call('queue_clear'));
document.addEventListener('keydown', (e) => {
  if (e.target.tagName === 'INPUT') return;
  if (e.code === 'Space') { e.preventDefault(); call('control', { action: 'toggle' }); }
  if (e.key === '/' ) { e.preventDefault(); $('search-input').focus(); }
});

function applyState(now) {
  const prevId = state.now && state.now.track && state.now.track.id;
  if (!prevId && now.track) showQueue();
  state.now = now;
  const t = now.track;
  $('np-title').textContent = t ? t.name : 'Nothing playing';
  $('np-artist').replaceChildren(...(t ? artistLinks(t.artists) : []));
  const cover = t && thumb(t.cover);
  if (cover) $('np-cover').src = cover; else $('np-cover').removeAttribute('src');
  $('toggle').textContent = now.loading ? '…' : (now.playing ? '⏸' : '▶');
  const hold = state.seekHold;
  if (hold && (Date.now() > hold.until || Math.abs(now.position - hold.target) < 2)) state.seekHold = null;
  if (!state.seeking && !state.seekHold) {
    seek.max = Math.max(1, Math.floor(now.duration));
    seek.value = Math.floor(now.position);
    $('pos').textContent = fmt(now.position);
  }
  $('dur').textContent = fmt(now.duration);
  if (document.activeElement !== vol) vol.value = now.volume;
  if (now.error) toast(`Playback failed: ${now.error}`);
  const id = t && t.id;
  if (id !== prevId) {
    for (const row of document.querySelectorAll('.track')) row.classList.toggle('current', row.dataset.id === id);
  }
}

// Drag and drop reordering: drop on the upper half of a row = before it.
let dragFrom = null;
function clearDropMarks() {
  for (const li of $('queue-list').children) li.classList.remove('drop-before', 'drop-after');
}
function dropTarget(e, i) {
  const r = e.currentTarget.getBoundingClientRect();
  return e.clientY < r.top + r.height / 2 ? 'before' : 'after';
}

function applyQueue(q) {
  state.queue = q;
  $('queue-list').replaceChildren(...q.tracks.map((t, i) => h('li', {
    class: i === q.index ? 'current' : '',
    draggable: 'true',
    onclick: () => call('queue_jump', { index: i }),
    ondragstart: (e) => {
      dragFrom = i;
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', String(i));
      e.currentTarget.classList.add('dragging');
    },
    ondragend: (e) => { e.currentTarget.classList.remove('dragging'); clearDropMarks(); dragFrom = null; },
    ondragover: (e) => {
      if (dragFrom === null) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
      clearDropMarks();
      e.currentTarget.classList.add(dropTarget(e, i) === 'before' ? 'drop-before' : 'drop-after');
    },
    ondrop: (e) => {
      e.preventDefault();
      clearDropMarks();
      if (dragFrom === null) return;
      let to = dropTarget(e, i) === 'before' ? i : i + 1;
      if (dragFrom < to) to -= 1; // removing the dragged row shifts later rows up
      if (to !== dragFrom) call('queue_move', { from: dragFrom, to });
      dragFrom = null;
    },
  },
    img(t.cover),
    h('div', { class: 'ellipsis' },
      h('div', { class: 'q-title ellipsis', text: t.name }),
      h('div', { class: 'muted ellipsis', text: (t.artists || []).map((a) => a.name).join(', ') })),
    h('button', { class: 'q-remove', title: 'Remove', text: '×', onclick: (e) => { e.stopPropagation(); call('queue_remove', { index: i }); } }),
  )));
  const cur = $('queue-list').children[q.index];
  if (cur && !$('queue').hidden) cur.scrollIntoView({ block: 'nearest' });
}

function applyAuth(a) {
  const changed = state.loggedIn !== a.logged_in;
  state.loggedIn = a.logged_in;
  $('account-nav').querySelector('.label').textContent = a.logged_in ? 'Account' : 'Sign in';
  $('account-nav').title = a.logged_in ? 'Account (signed in)' : 'Sign in';
  if (a.error) toast(a.error);
  if (changed && state.current) navigate(state.current);
}

// ---- start -----------------------------------------------------------------------

(async () => {
  await listen('state', (e) => applyState(e.payload));
  await listen('queue', (e) => applyQueue(e.payload));
  await listen('auth', (e) => applyAuth(e.payload));
  invoke('sync');
  try { setPins(await invoke('pins')); } catch (e) { /* none yet */ }
  preparePinned();
  try { applyAuth(await invoke('auth_status')); } catch (e) { /* ignore */ }
  navigate({ name: 'home' });
})();
