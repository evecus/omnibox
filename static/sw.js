// OmniBox Service Worker：HTML/API network-first，静态资源 cache-first
const CACHE = 'omnibox-v2';
const SHELL = ['/', '/search.html', '/play.html', '/spiders.html', '/app.css', '/manifest.json', '/hls.min.js', '/icon-192.png', '/icon-512.png'];
const HTML_PAGES = ['/', '/search.html', '/play.html', '/spiders.html'];

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
  // API 与 HTML：network-first（保证脚本管理页/播放页拿到最新版本）
  if (url.pathname.startsWith('/api/') || HTML_PAGES.indexOf(url.pathname) >= 0) {
    e.respondWith(fetch(req).then(resp => {
      if (resp && resp.ok) {
        const copy = resp.clone();
        caches.open(CACHE).then(c => c.put(req, copy)).catch(()=>{});
      }
      return resp;
    }).catch(() => caches.match(req).then(r => r || new Response(JSON.stringify({error:'离线'}), {status:503, headers:{'content-type':'application/json'}}))));
    return;
  }
  // 其余静态资源：cache-first
  e.respondWith(caches.match(req).then(cached => cached || fetch(req).then(resp => {
    const copy = resp.clone();
    caches.open(CACHE).then(c => c.put(req, copy)).catch(()=>{});
    return resp;
  })));
});
