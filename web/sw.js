// The service worker: keeps the app shell (page, wasm, fonts, icons)
// cached so the app opens offline. The vault itself lives in IndexedDB,
// which the app reads. The server's own paths (the sync API, sign-in,
// Claude's connector) share this origin and are never touched here.
// build.sh writes the build's version into CACHE, so a new build replaces
// the old shell on the next visit.
const CACHE = 'vault-shell-__VERSION__';
const SHELL = [
  './',
  'index.html',
  'app.js',
  'manifest.webmanifest',
  'cce_vault_web.js',
  'cce_vault_web_bg.wasm',
  'icons/icon-180.png',
  'icons/icon-192.png',
  'icons/icon-512.png',
  'fonts/NotoSans-Regular.ttf',
  'fonts/NotoSans-Bold.ttf',
  'fonts/NotoSans-Italic.ttf',
  'fonts/NotoSans-BoldItalic.ttf',
  'fonts/NotoSerif-Regular.ttf',
  'fonts/NotoSerif-Bold.ttf',
  'fonts/JetBrainsMono-Regular.ttf',
  'fonts/JetBrainsMono-Bold.ttf',
  'fonts/fa-regular-400.ttf',
];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== self.location.origin) return;
  if (/^\/(v1|oauth|mcp|\.well-known|health)(\/|$)/.test(url.pathname)) return;
  // The sign-in's return carries ?code=…: serve the page without
  // matching on the query.
  e.respondWith(caches.match(e.request, { ignoreSearch: true }).then((hit) => hit || fetch(e.request)));
});
