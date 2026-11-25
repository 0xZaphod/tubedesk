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
const PLAY_COMMANDS = new Set(['play', 'play_mix']);

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
  history: [],
  current: null,
};

// ---- navigation ----------------------------------------------------------------

const view = $('view');

function navigate(route, push = true) {
  if (push && state.current) state.history.push(state.current);
  state.current = route;
  $('back').disabled = state.history.length === 0;
  for (const b of document.querySelectorAll('.nav')) {
    b.classList.toggle('active', b.dataset.view === route.name);
  }
  render(route);
}
$('back').addEventListener('click', () => {
  const prev = state.history.pop();
  if (prev) navigate(prev, false);
});
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
    case 'home': return show(...(await renderHome()));
    case 'search': return show(...(await renderSearch(route.query)));
    case 'liked': return show(...(await renderLibraryTracks('liked', 'Liked songs')));
    case 'history': return show(...(await renderLibraryTracks('history', 'Recently played')));
    case 'playlists': return show(...(await renderLibraryCards('playlists', 'Playlists', playlistCard)));
    case 'albums': return show(...(await renderLibraryCards('albums', 'Albums', albumCard)));
    case 'artists': return show(...(await renderLibraryCards('artists', 'Artists', artistCard)));
    case 'playlist': return show(...(await renderPlaylist(route.id)));
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

function trackList(tracks) {
  if (!tracks.length) return h('div', { class: 'empty', text: 'No tracks.' });
  const playingId = state.now && state.now.track && state.now.track.id;
  return h('ol', { class: 'tracks' }, tracks.map((t, i) => h('li', {
    class: `track${t.id === playingId ? ' current' : ''}`,
    'data-id': t.id,
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
      h('button', { class: 'text', title: 'Play next', text: 'Next', onclick: () => { call('enqueue', { tracks: [t], next: true }); toast('Playing next'); } }),
      h('button', { class: 'text', title: 'Add to queue', text: '+ Queue', onclick: () => { call('enqueue', { tracks: [t], next: false }); toast('Added to queue'); } }),
    ),
  )));
}

function playButtons(tracks) {
  if (!tracks.length) return null;
  const shuffled = () => [...tracks].sort(() => Math.random() - 0.5);
  return h('div', { class: 'row-actions' },
    h('button', { class: 'btn', text: 'Play', onclick: () => call('play', { tracks, start: 0 }) }),
    h('button', { class: 'btn secondary', text: 'Shuffle', onclick: () => call('play', { tracks: shuffled(), start: 0 }) }),
    h('button', { class: 'btn secondary', text: 'Add to queue', onclick: () => { call('enqueue', { tracks, next: false }); toast(`Added ${tracks.length} tracks`); } }),
  );
}

function card(list, title, sub, onclick, round, playable) {
  return h('button', { class: `card${round ? ' round' : ''}${playable ? ' playable' : ''}`, onclick },
    h('div', { class: 'thumb' }, img(list)), h('div', { class: 'ellipsis', text: title }), h('div', { class: 'muted ellipsis', text: sub || '' }));
}
const albumCard = (a) => card(a.cover, a.name, [a.year, (a.artists || []).map((x) => x.name).join(', ')].filter(Boolean).join(' · '), () => navigate({ name: 'album', id: a.id }));
const playlistCard = (p) => card(p.thumbnail, p.name, [p.channel && p.channel.name, p.track_count && `${p.track_count} tracks`].filter(Boolean).join(' · '), () => navigate({ name: 'playlist', id: p.id }));
const artistCard = (a) => card(a.avatar, a.name, 'Artist', () => navigate({ name: 'artist', id: a.id }), true);

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
  switch (item.kind) {
  case 'playlist': return card(item.thumbnails, item.title, item.subtitle, () => navigate({ name: 'playlist', id: item.id }));
  case 'album': return card(item.thumbnails, item.title, item.subtitle, () => navigate({ name: 'album', id: item.id }));
  case 'artist': return card(item.thumbnails, item.title, item.subtitle, () => navigate({ name: 'artist', id: item.id }), true);
  case 'mix': return card(item.thumbnails, item.title, item.subtitle, () => { toast(`Starting ${item.title}…`); call('play_mix', { id: item.id }); }, false, true);
  case 'track': return item.track
    ? card(item.thumbnails, item.title, item.subtitle, () => call('play', { tracks: [item.track], start: 0 }), false, true)
    : null;
  default: return null;
  }
}

async function renderHome() {
  const sections = await call('home');
  if (!sections.length) return [h('div', { class: 'empty', text: 'YouTube Music returned an empty home page.' })];
  const out = [h('h1', { text: 'Home' })];
  if (!state.loggedIn) {
    out.push(h('div', { class: 'muted' }, 'Sign in for your pinned items, mixes and personal recommendations. ',
      h('span', { class: 'link', text: 'Sign in', onclick: () => navigate({ name: 'account' }) })));
  }
  for (const s of sections) {
    const body = s.layout === 'tracks'
      ? trackList(s.items.map((i) => i.track).filter(Boolean))
      : h('div', { class: 'shelf' }, s.items.map(homeCard).filter(Boolean));
    out.push(h('section', { class: 'home-section' }, h('h2', { text: s.title || 'For you' }), body));
  }
  return out;
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

async function renderPlaylist(id) {
  const p = await call('playlist', { id });
  const tracks = p.tracks.items;
  return [detailHead(p.thumbnail, p.name, [p.channel && p.channel.name, `${p.track_count || tracks.length} tracks`].filter(Boolean).join(' · ')), playButtons(tracks), trackList(tracks)];
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
seek.addEventListener('change', () => { state.seeking = false; call('seek', { position: Number(seek.value) }); });
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
  if (!state.seeking) {
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
  $('account-nav').textContent = a.logged_in ? 'Account ✓' : 'Sign in';
  if (a.error) toast(a.error);
  if (changed && state.current) navigate(state.current, false);
}

// ---- start -----------------------------------------------------------------------

(async () => {
  await listen('state', (e) => applyState(e.payload));
  await listen('queue', (e) => applyQueue(e.payload));
  await listen('auth', (e) => applyAuth(e.payload));
  invoke('sync');
  try { applyAuth(await invoke('auth_status')); } catch (e) { /* offline */ }
  navigate({ name: 'home' }, false);
})();
