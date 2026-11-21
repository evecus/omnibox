/*
 * runner.js —— FilmBox Node.js 子进程桥接器
 * 由 Rust(node.rs) 启动，通过 stdio NDJSON 双向 RPC 执行 OmniBox 脚本。
 *
 * 关键点：
 *  - omnibox_sdk / axios / spider_runner 用自实现 shim（网络请求桥接回 Rust）
 *  - crypto / https / http / buffer 等 Node 原生模块直接放行 —— 这是相对 QuickJS 的核心优势，
 *    瓜子脚本的 crypto.createCipheriv / publicEncrypt / privateDecrypt 可以真正运行。
 *  - 每个 call 用 Module 隔离加载脚本，避免模块缓存导致的状态串扰。
 */
'use strict';

const path = require('path');
const fs = require('fs');
const Module = require('module');

const pendingHttp = new Map();   // http req id -> {resolve, reject}
let nextOut = 1;

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

// ---- OmniBox.request 桥接回 Rust ----
function omniRequest(url, opts) {
  opts = opts || {};
  return new Promise((resolve, reject) => {
    const id = nextOut++;
    pendingHttp.set(id, { resolve, reject });
    send({ t: 'http_req', id, url: String(url), opts: normalizeOpts(opts) });
  });
}

function normalizeOpts(opts) {
  const o = {};
  if (opts.method) o.method = String(opts.method).toUpperCase();
  if (opts.headers && typeof opts.headers === 'object') {
    o.headers = {};
    for (const k of Object.keys(opts.headers)) o.headers[k] = String(opts.headers[k]);
  }
  if (opts.body !== undefined && opts.body !== null) o.body = String(opts.body);
  return o;
}

function log(level, msg) {
  send({ t: 'log', level: String(level || 'info'), msg: String(msg) });
}

// ---- omnibox_sdk shim ----
const memCache = new Map();
const omniboxSdk = {
  request: omniRequest,
  log: (level, msg) => log(level, msg),
  getCache: (key) => memCache.has(String(key)) ? memCache.get(String(key)) : null,
  setCache: (key, val, ttl) => {
    memCache.set(String(key), val);
    if (ttl && ttl > 0) {
      setTimeout(() => memCache.delete(String(key)), Number(ttl) * 1000).unref?.();
    }
  },
  // 以下为刮削/网盘/弹幕扩展，FilmBox 用不到，返回空值避免脚本报错
  sniffVideo: async () => [],
  processScraping: async () => null,
  getScrapeMetadata: async () => null,
  getDanmakuByFileName: async () => [],
  getDriveFileList: async () => [],
  getDriveVideoPlayInfo: async () => null,
  getDriveInfoByShareURL: async () => null,
};

// ---- axios shim（基于 omnibox_sdk.request）----
function axiosResp(r) {
  if (r && r.__err) { const e = new Error(r.__err); throw e; }
  const status = (r && r.statusCode) || 0;
  let data = r ? r.body : '';
  try { data = JSON.parse(data); } catch (_) {}
  return { status, statusText: String(status), data, headers: (r && r.headers) || {}, config: {} };
}
const axios = {
  request: async (cfg) => {
    cfg = cfg || {};
    let url = String(cfg.url || '');
    if (cfg.params) {
      const q = Object.keys(cfg.params)
        .filter(k => cfg.params[k] !== undefined && cfg.params[k] !== null && cfg.params[k] !== '')
        .map(k => encodeURIComponent(k) + '=' + encodeURIComponent(cfg.params[k]))
        .join('&');
      if (q) url += (url.indexOf('?') >= 0 ? '&' : '?') + q;
    }
    const r = await omniRequest(url, {
      method: cfg.method || 'GET',
      headers: cfg.headers || {},
      body: cfg.data,
    });
    const resp = axiosResp(r);
    if (cfg.validateStatus && !cfg.validateStatus(resp.status)) {
      const err = new Error('HTTP ' + resp.status);
      err.response = resp;
      throw err;
    }
    return resp;
  },
  get: (url, cfg) => { cfg = cfg || {}; cfg.url = url; cfg.method = 'GET'; return axios.request(cfg); },
  post: (url, data, cfg) => { cfg = cfg || {}; cfg.url = url; cfg.method = 'POST'; cfg.data = data; return axios.request(cfg); },
  create: (defaults) => {
    defaults = defaults || {};
    const merge = (cfg) => Object.assign({}, defaults, cfg || {});
    return {
      defaults,
      get: (url, cfg) => axios.get(url, merge(cfg)),
      post: (url, data, cfg) => axios.post(url, data, merge(cfg)),
      request: (cfg) => axios.request(merge(cfg)),
    };
  },
};

// ---- spider_runner shim：FilmBox 主动调方法，无需脚本自注册 ----
const spiderRunner = { run: () => {} };

// ---- 拦截 require：SDK 走 shim，Node 原生模块放行，第三方包按需自动安装 ----
let cryptoJsShim;
try { cryptoJsShim = require('./.crypto-js-shim.js'); } catch (_) { cryptoJsShim = {}; }

const SHIMS = {
  omnibox_sdk: omniboxSdk,
  axios,
  spider_runner: spiderRunner,
  'omnibox-sdk': omniboxSdk,
  'crypto-js': cryptoJsShim,
};

// 脚本依赖目录：Rust 传入绝对路径 SPIDER_DIR；兼容旧的 DATA_DIR 相对写法。
const SPIDER_DIR = process.env.SPIDER_DIR || path.resolve(process.env.DATA_DIR || 'data', 'spiders');
const NODE_MODULES = path.join(SPIDER_DIR, 'node_modules');

// 白名单：脚本 require 到这些包且本机缺失时自动 npm install。
// 只放行明确列出的包，避免脚本借机安装任意 npm 包（供应链风险）。
const AUTO_INSTALL = {
  cheerio: 'cheerio@1.0.0',
  opencc: 'opencc-js',
  'opencc-js': 'opencc-js',
  'chinese-conv': 'chinese-conv',
  '@node-rs/opencc': '@node-rs/opencc',
};

function resolveInstalled(name) {
  try { return require.resolve(name, { paths: [SPIDER_DIR, NODE_MODULES] }); }
  catch (_) { return null; }
}

// 同步安装：require 本身是同步的，Node 单线程下 execFileSync 天然避免并发安装冲突。
// 仅在首次缺包时发生，装完后 resolveInstalled 命中，不会重复安装。
function autoInstall(request) {
  const spec = AUTO_INSTALL[request];
  if (!spec || resolveInstalled(request)) return;

  log('info', `检测到缺少依赖 "${request}"，正在自动安装 ${spec} ...`);
  try {
    fs.mkdirSync(SPIDER_DIR, { recursive: true });
    const pkg = path.join(SPIDER_DIR, 'package.json');
    if (!fs.existsSync(pkg)) fs.writeFileSync(pkg, '{"name":"filmbox-spiders","private":true}');

    const cp = require('child_process');
    const out = cp.execFileSync(
      'npm',
      ['install', spec, '--no-audit', '--no-fund', '--no-save', '--prefix', SPIDER_DIR],
      { cwd: SPIDER_DIR, timeout: 150000, maxBuffer: 4 * 1024 * 1024, encoding: 'utf8' }
    );
    // 刷新 require 的模块查找路径，让新装的包立即生效
    require('module').Module._initPaths();
    if (!resolveInstalled(request)) throw new Error('安装完成但仍未找到模块');
    const tail = String(out || '').trim().split('\n').slice(-2).join(' ').slice(0, 160);
    log('info', `依赖 "${request}" 自动安装成功${tail ? '：' + tail : ''}`);
  } catch (e) {
    const detail = String((e && (e.stderr || e.stdout)) || (e && e.message) || e).slice(0, 300);
    log('error', `自动安装 "${request}" 失败：${detail}`);
    throw new Error(`缺少依赖包 "${request}" 且自动安装失败（${detail}）。请检查服务端网络，或手动在 ${SPIDER_DIR} 执行 npm i ${spec}`);
  }
}

const origLoad = Module._load;
Module._load = function (request, parent, isMain) {
  if (Object.prototype.hasOwnProperty.call(SHIMS, request)) return SHIMS[request];
  if (AUTO_INSTALL[request] && !resolveInstalled(request)) autoInstall(request);
  return origLoad.apply(this, arguments);
};

// ---- 执行脚本方法 ----
async function handleCall(msg) {
  const { id, file, method, params, env } = msg;
  try {
    // 注入环境变量（部分脚本用 process.env.DANMU_API 等）
    if (env && typeof env === 'object') {
      for (const k of Object.keys(env)) process.env[k] = String(env[k]);
    }
    if (!file || !fs.existsSync(file)) throw new Error('脚本文件不存在: ' + file);

    // 每次调用用全新 Module 加载，避免状态串扰与缓存
    const abs = path.resolve(file);
    delete require.cache[require.resolve(abs)];
    const mod = require(abs);
    const fn = mod && mod[method];
    if (typeof fn !== 'function') throw new Error('脚本未导出方法: ' + method);

    const result = await fn(params || {}, { from: 'web' });
    send({ t: 'call_result', id, ok: true, data: result === undefined ? null : result });
  } catch (e) {
    send({ t: 'call_result', id, ok: false, error: String((e && e.stack) || e) });
  }
}

// ---- stdin 消息循环 ----
let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let idx;
  while ((idx = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, idx);
    buf = buf.slice(idx + 1);
    if (!line.trim()) continue;
    let msg;
    try { msg = JSON.parse(line); } catch (_) { continue; }

    if (msg.t === 'http_resp') {
      const p = pendingHttp.get(msg.id);
      if (p) {
        pendingHttp.delete(msg.id);
        if (msg.ok) p.resolve(msg.data);
        else p.reject(new Error(msg.error || 'HTTP 桥接失败'));
      }
    } else if (msg.t === 'call') {
      handleCall(msg).catch(e => send({ t: 'call_result', id: msg.id, ok: false, error: String(e) }));
    }
  }
});

process.stdin.on('end', () => process.exit(0));

// 通知 Rust 就绪
send({ t: 'log', level: 'info', msg: 'node runner ready, node=' + process.version });
