// FilmBox Service Worker：静态资源 cache-first，API network-first
const CACHE = 'filmbox-v1';
const SHELL = ['/', '/search.html', '/play.html', '/spiders.html', '/app.css', '/manifest.json', '/icon-192.png', '/icon-512.png'];

self.addEventListener('install', e => {
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(SHELL)).then(() => self.skipWaiting()).catch(() => self.skipWaiting()));
});
self.addEventListener('activate', e => {
  e.waitUntil(caches.keys().then(keys => Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k)))).then(() => self.clients.claim()));
});
self.addEventListener('fetch', e => {
  const req = e.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  // API：network-first，失败回落缓存（仅 GET 且缓存过）
  if (url.pathname.startsWith('/api/')) {
    e.respondWith(fetch(req).then(resp => {
      const copy = resp.clone();
      caches.open(CACHE).then(c => c.put(req, copy)).catch(()=>{});
      return resp;
    }).catch(() => caches.match(req).then(r => r || new Response(JSON.stringify({error:'离线'}), {status:503, headers:{'content-type':'application/json'}}))));
    return;
  }
  // 静态资源：cache-first
  e.respondWith(caches.match(req).then(cached => cached || fetch(req).then(resp => {
    const copy = resp.clone();
    caches.open(CACHE).then(c => c.put(req, copy)).catch(()=>{});
    return resp;
  })));
});
