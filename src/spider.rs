// spider.rs —— QuickJS 运行时 + OmniBox SDK 兼容层
// 每个脚本调用创建独立 AsyncRuntime，注入 CommonJS 环境与 SDK polyfill，
// 通过 Rust 原语（__log / __request / __b64*）桥接网络与工具能力。

use crate::registry;
use anyhow::{Context as _, Result};
use rquickjs::prelude::*;
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Ctx, Promise};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

// 内嵌纯 JS 库（构建产物来自 libs/，见 libs/package.json 的 build 脚本）
const CHEERIO_BUNDLE: &str = include_str!("../libs/cheerio.bundle.js");
const CRYPTO_JS_BUNDLE: &str = include_str!("../libs/crypto-js.bundle.js");

/// JS 侧 polyfill：CommonJS + OmniBox SDK + Buffer + axios + URL
/// 所有能力最终落到 Rust 注入的 __log / __request / __b64* 原语上。
const POLYFILL: &str = r#"
var __mods = {};

// ---- omnibox_sdk ----
__mods["omnibox_sdk"] = {
  log: function(level, msg){
    try { __log(String(level), String(msg)); } catch(e) {}
  },
  request: async function(url, opts){
    var o = opts || {};
    var r = await __request(String(url), JSON.stringify(o));
    var parsed = JSON.parse(r);
    if (parsed && parsed.__err) throw new Error(parsed.__err);
    return parsed; // { statusCode, body, headers }
  },
  getCache: function(key){ try { return __cacheGet(String(key)); } catch(e){ return null; } },
  setCache: function(key, val, ttl){ try { __cacheSet(String(key), JSON.stringify(val), Number(ttl)||300); } catch(e){} },
  sniffVideo: async function(){ return []; },
  processScraping: async function(){ return null; },
  getScrapeMetadata: async function(){ return null; },
  getDanmakuByFileName: async function(){ return []; },
  getDriveFileList: async function(){ return []; },
  getDriveVideoPlayInfo: async function(){ return null; },
  getDriveInfoByShareURL: async function(){ return null; }
};

// ---- spider_runner ----
__mods["spider_runner"] = {
  run: function(){ /* 导出已在 script 末尾求值，无需 stdin 循环 */ }
};

// ---- axios 兼容层 ----
function __axiosResp(r){ return { status: r.statusCode, statusText: String(r.statusCode), data: (function(){
  try { return JSON.parse(r.body); } catch(e){ return r.body; }
})(), headers: r.headers || {}, config: {} }; }
var __axios = {
  request: async function(cfg){
    cfg = cfg || {};
    var url = String(cfg.url || "");
    if (cfg.params) {
      var q = Object.keys(cfg.params).filter(function(k){ return cfg.params[k] !== undefined && cfg.params[k] !== null && cfg.params[k] !== ""; })
        .map(function(k){ return encodeURIComponent(k) + "=" + encodeURIComponent(cfg.params[k]); }).join("&");
      if (q) url += (url.indexOf("?") >= 0 ? "&" : "?") + q;
    }
    var r = await __request(url, JSON.stringify({ method: cfg.method || "GET", headers: cfg.headers || {}, body: cfg.data }));
    var parsed = JSON.parse(r);
    if (parsed && parsed.__err) { var ge = new Error(parsed.__err); ge.config = cfg; throw ge; }
    if (cfg.validateStatus && !cfg.validateStatus(parsed.statusCode)) {
      var err = new Error("HTTP " + parsed.statusCode); err.response = __axiosResp(parsed); throw err;
    }
    return __axiosResp(parsed);
  },
  get: function(url, cfg){ cfg = cfg || {}; cfg.url = url; cfg.method = "GET"; return __axios.request(cfg); },
  post: function(url, data, cfg){ cfg = cfg || {}; cfg.url = url; cfg.method = "POST"; cfg.data = data; return __axios.request(cfg); },
  head: function(url, cfg){ cfg = cfg || {}; cfg.url = url; cfg.method = "HEAD"; return __axios.request(cfg); },
  put: function(url, data, cfg){ cfg = cfg || {}; cfg.url = url; cfg.method = "PUT"; cfg.data = data; return __axios.request(cfg); },
  create: function(defaults){
    defaults = defaults || {};
    var inst = {
      defaults: defaults,
      get: function(url, cfg){ cfg = Object.assign({}, defaults, cfg || {}); return __axios.get(url, cfg); },
      post: function(url, data, cfg){ cfg = Object.assign({}, defaults, cfg || {}); return __axios.post(url, data, cfg); },
      head: function(url, cfg){ cfg = Object.assign({}, defaults, cfg || {}); return __axios.head(url, cfg); },
      request: function(cfg){ cfg = Object.assign({}, defaults, cfg || {}); return __axios.request(cfg); }
    };
    return inst;
  }
};
__mods["axios"] = __axios;

// ---- Buffer 兼容层（utf8 / base64 / base64url / hex）----
// 内部用 latin1 字节串表示，支持字节级操作（indexOf/slice/[i]/concat）
function __mkbuf(bytes){
  var s = "";
  for (var i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i] & 255);
  var b = {
    _bytes: bytes,
    length: bytes.length,
    toString: function(o){
      o = o || "utf8";
      if (o === "base64" || o === "base64url") return __hexToB64(__bytesToHex(bytes), o);
      if (o === "hex") return __bytesToHex(bytes);
      if (o === "latin1" || o === "binary") return s;
      return __latin1ToUtf8(s);
    },
    slice: function(a, b){ return __mkbuf(bytes.slice(a, b)); },
    subarray: function(a, b){ return __mkbuf(bytes.slice(a, b)); },
    indexOf: function(v, from){
      var x = (typeof v === "number") ? v : (v & 255);
      return bytes.indexOf(x, from || 0);
    }
  };
  for (var k = 0; k < bytes.length; k++) b[k] = bytes[k];
  return b;
}
function __bytesToHex(bytes){
  var h = "";
  for (var i = 0; i < bytes.length; i++){ var v = bytes[i] & 255; h += (v < 16 ? "0" : "") + v.toString(16); }
  return h;
}
function __utf8ToBytes(str){
  str = String(str);
  var out = [], i = 0;
  while (i < str.length){
    var c = str.charCodeAt(i);
    if (c < 128) { out.push(c); i++; }
    else if (c < 2048) { out.push(192|(c>>6), 128|(c&63)); i++; }
    else if (c >= 55296 && c <= 56319 && i+1 < str.length){
      var c2 = str.charCodeAt(i+1);
      var cp = 65536 + ((c-55296)<<10) + (c2-56320);
      out.push(240|(cp>>18), 128|((cp>>12)&63), 128|((cp>>6)&63), 128|(cp&63)); i+=2;
    } else { out.push(224|(c>>12), 128|((c>>6)&63), 128|(c&63)); i++; }
  }
  return out;
}
function __latin1ToUtf8(s){
  var bytes = [];
  for (var i=0;i<s.length;i++) bytes.push(s.charCodeAt(i)&255);
  return __bytesToUtf8(bytes);
}
function __bytesToUtf8(bytes){
  var out = "", i = 0;
  while (i < bytes.length){
    var c = bytes[i];
    if (c < 128) { out += String.fromCharCode(c); i++; }
    else if (c >= 192 && c < 224) { out += String.fromCharCode(((c&31)<<6)|(bytes[i+1]&63)); i+=2; }
    else if (c >= 224 && c < 240) { out += String.fromCharCode(((c&15)<<12)|((bytes[i+1]&63)<<6)|(bytes[i+2]&63)); i+=3; }
    else if (c >= 240) { var cp=((c&7)<<18)|((bytes[i+1]&63)<<12)|((bytes[i+2]&63)<<6)|(bytes[i+3]&63); cp-=65536; out += String.fromCharCode(55296|(cp>>10), 56320|(cp&63)); i+=4; }
    else { out += String.fromCharCode(c); i++; }
  }
  return out;
}
function __strToBytes(data, enc){
  enc = enc || "utf8";
  if (typeof data === "number") return new Array(Math.max(0, data|0)).fill(0);
  if (Array.isArray(data)) return data.map(function(x){ return x & 255; });
  if (data && data._bytes) return data._bytes.slice();
  if (enc === "base64" || enc === "base64url"){
    return __hexToBytes(__b64ToHex(String(data), enc));
  }
  if (enc === "hex"){
    var h = String(data), out=[];
    for (var i=0;i+1<h.length;i+=2){ var v=parseInt(h.substr(i,2),16); if(!isNaN(v)) out.push(v); }
    return out;
  }
  return __utf8ToBytes(String(data));
}
globalThis.Buffer = {
  from: function(data, enc){ return __mkbuf(__strToBytes(data, enc)); },
  concat: function(list){
    var out = [];
    for (var i=0;i<list.length;i++){
      var b = list[i]._bytes || [];
      for (var j=0;j<b.length;j++) out.push(b[j]&255);
    }
    return __mkbuf(out);
  },
  alloc: function(n){ return __mkbuf(new Array(Math.max(0,n|0)).fill(0)); },
  isBuffer: function(x){ return !!(x && x._bytes); },
  byteLength: function(s, enc){ return __strToBytes(s, enc || "utf8").length; }
};

// ---- URLSearchParams / URL ----
globalThis.URLSearchParams = function(init){
  var m = [];
  if (typeof init === "string"){
    init = init.replace(/^\?/, "");
    init.split("&").forEach(function(pair){
      if (!pair) return;
      var i = pair.indexOf("=");
      var k = i < 0 ? pair : pair.slice(0, i);
      var v = i < 0 ? "" : pair.slice(i + 1);
      m.push([decodeURIComponent(k.replace(/\+/g," ")), decodeURIComponent(v.replace(/\+/g," "))]);
    });
  } else if (init && typeof init === "object"){
    Object.keys(init).forEach(function(k){ m.push([k, String(init[k])]); });
  }
  this._m = m;
};
globalThis.URLSearchParams.prototype = {
  toString: function(){
    return this._m.map(function(p){ return encodeURIComponent(p[0]) + "=" + encodeURIComponent(p[1]); }).join("&");
  },
  append: function(k,v){ this._m.push([String(k), String(v)]); },
  set: function(k,v){
    k = String(k); var found = false;
    for (var i=0;i<this._m.length;i++){ if (this._m[i][0]===k){ this._m[i][1]=String(v); } else if(!found){ /* noop */ } }
    var exists = this._m.some(function(p){ return p[0]===k; });
    if (!exists) this._m.push([k, String(v)]);
  },
  get: function(k){ for (var i=0;i<this._m.length;i++) if (this._m[i][0]===k) return this._m[i][1]; return null; },
  has: function(k){ return this._m.some(function(p){ return p[0]===k; }); },
  forEach: function(cb){ this._m.forEach(function(p){ cb(p[1], p[0]); }); },
  entries: function(){ return this._m.map(function(p){ return [p[0], p[1]]; })[Symbol.iterator](); },
  keys: function(){ return this._m.map(function(p){ return p[0]; }); },
  values: function(){ return this._m.map(function(p){ return p[1]; }); }
};
globalThis.URLSearchParams.prototype[Symbol.iterator] = globalThis.URLSearchParams.prototype.entries;
globalThis.URL = function(u, base){
  if (base && !/^https?:/i.test(String(u))){
    var b = String(base).replace(/\/[^/]*$/, "/");
    u = b + String(u).replace(/^\.\//,"").replace(/^\//,"");
  }
  this.href = String(u);
  var m = /^(https?:)\/\/([^\/?#]+)([^?#]*)(\?[^#]*)?(#.*)?$/.exec(String(u));
  if (m){
    this.protocol = m[1];
    var hostPart = m[2], ci = hostPart.lastIndexOf(":");
    if (ci > hostPart.lastIndexOf("]")){
      this.host = hostPart; this.hostname = hostPart.slice(0, ci);
      this.port = hostPart.slice(ci + 1);
    } else {
      this.host = hostPart; this.hostname = hostPart; this.port = "";
    }
    this.pathname = m[3] || "/";
    this.search = m[4] || ""; this.hash = m[5] || "";
    this.origin = this.protocol + "//" + this.host;
    this.searchParams = new globalThis.URLSearchParams(this.search || "");
  } else {
    this.protocol=""; this.host=""; this.hostname=""; this.port=""; this.origin="";
    this.pathname=String(u); this.search=""; this.hash="";
    this.searchParams = new globalThis.URLSearchParams();
  }
};
globalThis.encodeURIComponent = encodeURIComponent;
globalThis.decodeURIComponent = decodeURIComponent;

// ---- http/https：Node 风格 API，底层走 __request ----
// 支持 get(url|opts, cb)、request(url|opts, opts, cb)、req.write/end/on/setTimeout/destroy，
// res.statusCode/headers/setEncoding/on('data'/'end')；数据块用 Buffer（字符串拼接与 Buffer.concat 都兼容）。
var __agentMod = { Agent: function(opts){ this.options = opts || {}; } };
function __nodeHttp(secure){
  function Emitter(){ this._cbs = {}; }
  Emitter.prototype.on = function(ev, fn){ (this._cbs[ev] = this._cbs[ev] || []).push(fn); return this; };
  Emitter.prototype.emit = function(ev, a, b){
    var l = (this._cbs[ev] || []).slice();
    for (var i = 0; i < l.length; i++){ try { l[i](a, b); } catch(e) { try { __log("error", String(e && e.message || e)); } catch(_){} } }
    return this;
  };
  function request(urlOrOpts, opts, cb){
    var urlStr = null, options = {};
    if (typeof urlOrOpts === "string" || (urlOrOpts && urlOrOpts.href && urlOrOpts.pathname)){
      urlStr = String(urlOrOpts.href || urlOrOpts);
      if (typeof opts === "function") cb = opts;
      else if (opts && typeof opts === "object") options = opts;
    } else if (urlOrOpts && typeof urlOrOpts === "object"){
      options = urlOrOpts;
      if (typeof opts === "function") cb = opts;
    } else {
      throw new Error("http(s).request: 参数不合法");
    }
    if (!urlStr){
      var hostport = options.host || (options.hostname + (options.port ? ":" + options.port : ""));
      urlStr = (secure ? "https://" : "http://") + hostport + (options.path || "/");
    }
    var req = new Emitter();
    var bodyParts = [];
    var ended = false;
    var sent = false;
    req.write = function(d){ bodyParts.push(d); return true; };
    req.end = function(d){ if (d !== undefined && d !== null) bodyParts.push(d); ended = true; if (!sent){ sent = true; doReq(); } return req; };
    req.setTimeout = function(){ return req; }; // 超时由 Rust __request 的 15s 上限兜底，失败走 'error'
    req.destroy = function(){ return req; };
    req.abort = function(){ return req; };
    if (typeof cb === "function") req.on("response", cb);

    function doReq(){
      var optObj = { method: (options.method || "GET"), headers: options.headers || {} };
      if (bodyParts.length){
        var bytes = [];
        for (var i = 0; i < bodyParts.length; i++){
          var b = __strToBytes(bodyParts[i], typeof bodyParts[i] === "string" ? "utf8" : undefined);
          for (var j = 0; j < b.length; j++) bytes.push(b[j]);
        }
        optObj.__body_b64 = __hexToB64(__bytesToHex(bytes), "");
        optObj.body = "";
      }
      Promise.resolve().then(function(){ return __request(urlStr, JSON.stringify(optObj)); }).then(function(r){
        var parsed;
        try { parsed = JSON.parse(r); } catch(e) { req.emit("error", new Error("响应解析失败")); return; }
        if (parsed && parsed.__err){ req.emit("error", new Error(parsed.__err)); return; }
        var res = new Emitter();
        res.statusCode = parsed.statusCode;
        res.headers = parsed.headers || {};
        res.setEncoding = function(){};
        res.destroy = function(){};
        var chunks = [];
        if (parsed.__body_b64) chunks.push(__mkbuf(__hexToBytes(__b64ToHex(parsed.__body_b64))));
        else if (parsed.body != null) chunks.push(parsed.body);
        setTimeout(function(){
          for (var i = 0; i < chunks.length; i++) res.emit("data", chunks[i]);
          res.emit("end");
        }, 0);
        req.emit("response", res);
      }, function(e){ req.emit("error", e instanceof Error ? e : new Error(String(e))); });
    }
    return req;
  }
  var mod = { request: request, Agent: __agentMod.Agent, globalAgent: __agentMod.Agent };
  mod.get = function(urlOrOpts, opts, cb){
    var req = request(urlOrOpts, opts, cb);
    setTimeout(function(){ req.end(); }, 0);
    return req;
  };
  return mod;
}
__mods["http"] = __nodeHttp(false);
__mods["https"] = __nodeHttp(true);

// ---- 其他模块占位 ----
var __osMod = {
  tmpdir: function(){ return "/tmp"; },
  homedir: function(){ return "/tmp"; },
  platform: function(){ return "linux"; },
  EOL: "\n"
};
var __pathMod = {
  join: function(){ var a = Array.prototype.slice.call(arguments); return a.join("/").replace(/\/+/g,"/"); },
  resolve: function(){ var a = Array.prototype.slice.call(arguments); return a.join("/").replace(/\/+/g,"/"); },
  basename: function(p){ return String(p).split("/").pop(); },
  dirname: function(p){ var s=String(p).split("/"); s.pop(); return s.join("/")||"/"; },
  extname: function(p){ var b=__pathMod.basename(p); var i=b.lastIndexOf("."); return i<0?"":b.slice(i); }
};
var __fsMod = {
  // 路径只作为键，内容持久化在 数据目录/.fs/ 下（跨调用保留，如登录 token）
  existsSync: function(p){ return JSON.parse(__fsGet(String(p))) !== null; },
  readFileSync: function(p){ var v = JSON.parse(__fsGet(String(p))); return v === null ? "" : v; },
  writeFileSync: function(p, d){
    var str = (d && d._bytes) ? d.toString("utf8") : String(d);
    if (!__fsSet(String(p), str)) throw new Error("写入失败（超过 1MB 或目录不可写）: " + p);
  },
  unlinkSync: function(p){ __fsDel(String(p)); },
  mkdirSync: function(){},
  readdirSync: function(){ return []; }
};
// ---- crypto 兼容层：哈希/AES/RSA/随机数全部落到 Rust 原语（见 jscrypto.rs）----
function __hexToBytes(h){ var out=[]; for(var i=0;i+1<h.length;i+=2) out.push(parseInt(h.substr(i,2),16)); return out; }
function __chk(r){ if (typeof r === "string" && r.indexOf("ERR:") === 0) throw new Error(r.slice(4)); return r; }
function __encOut(bytes, enc){ var b = __mkbuf(bytes); return enc ? b.toString(enc) : b; }
function __pemOf(k){ return (k && typeof k === "object" && !k._bytes) ? String(k.key) : String(k); }
function __padOf(k, def){ return (k && typeof k === "object" && !k._bytes && k.padding != null) ? (k.padding|0) : def; }
function __Cipher(dec, alg, key, iv){
  this._dec = dec; this._alg = String(alg); this._pad = true; this._in = [];
  this._key = __bytesToHex(__strToBytes(key));
  this._iv = iv == null ? "" : __bytesToHex(__strToBytes(iv));
}
__Cipher.prototype.setAutoPadding = function(v){ this._pad = v !== false; return this; };
// 与 Node 的差别：update 只缓存，数据在 final 一次性处理；update()+final() 拼接后的结果与 Node 一致
__Cipher.prototype.update = function(data, inEnc, outEnc){
  var b = __strToBytes(data, inEnc);
  for (var i = 0; i < b.length; i++) this._in.push(b[i]);
  return outEnc ? "" : __mkbuf([]);
};
__Cipher.prototype.final = function(outEnc){
  var r = __chk(__aes(this._dec ? "dec" : "enc", this._alg, this._key, this._iv, __bytesToHex(this._in), this._pad));
  this._in = [];
  return __encOut(__hexToBytes(r), outEnc);
};
function __Hash(alg){ this._alg = String(alg); this._in = []; }
__Hash.prototype.update = function(d, enc){
  var b = __strToBytes(d, enc);
  for (var i = 0; i < b.length; i++) this._in.push(b[i]);
  return this;
};
__Hash.prototype.digest = function(enc){
  var r = __chk(__hash(this._alg, __bytesToHex(this._in)));
  return __encOut(__hexToBytes(r), enc);
};
var __cryptoMod = {
  createHash: function(alg){ return new __Hash(alg); },
  randomBytes: function(n){ return __mkbuf(__hexToBytes(__randHex(Number(n) || 0))); },
  randomFillSync: function(b){
    var n = (b && b._bytes) ? b._bytes.length : 0;
    if (n > 0){
      var bytes = __hexToBytes(__randHex(n));
      for (var i = 0; i < n; i++){ b._bytes[i] = bytes[i]; b[i] = bytes[i]; }
    }
    return b;
  },
  randomFill: function(b){ return __cryptoMod.randomFillSync(b); },
  constants: { RSA_PKCS1_PADDING: 1, RSA_NO_PADDING: 3, RSA_PKCS1_OAEP_PADDING: 4 },
  createCipheriv: function(alg, key, iv){ return new __Cipher(false, alg, key, iv); },
  createDecipheriv: function(alg, key, iv){ return new __Cipher(true, alg, key, iv); },
  createSign: function(alg){
    var inBuf = [];
    return {
      update: function(d, enc){
        var b = __strToBytes(d, enc);
        for (var i = 0; i < b.length; i++) inBuf.push(b[i]);
        return this;
      },
      sign: function(key, outEnc){
        var pem = __pemOf(key);
        var r = __chk(__rsaSign(String(alg), pem, __bytesToHex(inBuf)));
        return outEnc ? __encOut(__hexToBytes(r), outEnc) : __mkbuf(__hexToBytes(r));
      }
    };
  },
  // 注意 Node 的 publicEncrypt/privateDecrypt 默认填充是 OAEP(4)
  publicEncrypt: function(key, data){
    return __mkbuf(__hexToBytes(__chk(__rsa("pub_enc", __pemOf(key), __bytesToHex(__strToBytes(data)), __padOf(key, 4)))));
  },
  privateDecrypt: function(key, data){
    return __mkbuf(__hexToBytes(__chk(__rsa("priv_dec", __pemOf(key), __bytesToHex(__strToBytes(data)), __padOf(key, 4)))));
  }
};
__mods["crypto"] = __cryptoMod;

// ---- url / querystring / util ----
__mods["url"] = {
  URL: globalThis.URL,
  URLSearchParams: globalThis.URLSearchParams,
  parse: function(u){
    var p = new globalThis.URL(String(u));
    return { href: p.href, protocol: p.protocol, host: p.host, hostname: p.hostname,
      port: p.port, pathname: p.pathname, search: p.search, hash: p.hash,
      query: (p.search ? p.search.slice(1) : ""), origin: p.origin };
  },
  format: function(o){
    if (typeof o === "string") return o;
    var s = (o.protocol || "http:") + "//" + (o.host || (o.hostname + (o.port ? ":" + o.port : ""))) + (o.pathname || "/");
    if (o.search) s += o.search; if (o.hash) s += o.hash;
    return s;
  },
  resolve: function(base, rel){
    var b = String(base).replace(/\/[^/]*$/, "/");
    return /^https?:/i.test(String(rel)) ? String(rel) : b + String(rel).replace(/^\.\//,"").replace(/^\//,"");
  }
};
__mods["querystring"] = {
  parse: function(s){
    var out = {};
    String(s).replace(/^\?/, "").split("&").forEach(function(pair){
      if (!pair) return;
      var i = pair.indexOf("=");
      var k = i < 0 ? pair : pair.slice(0, i);
      var v = i < 0 ? "" : pair.slice(i + 1);
      try { k = decodeURIComponent(k); v = decodeURIComponent(v); } catch(e){}
      out[k] = v;
    });
    return out;
  },
  stringify: function(obj, sep, eq){
    sep = sep || "&"; eq = eq || "=";
    return Object.keys(obj || {}).filter(function(k){ return obj[k] !== undefined && obj[k] !== null; })
      .map(function(k){ return encodeURIComponent(k) + eq + encodeURIComponent(obj[k]); }).join(sep);
  },
  escape: encodeURIComponent,
  unescape: decodeURIComponent
};
__mods["util"] = {
  promisify: function(fn){
    return function(){
      var args = Array.prototype.slice.call(arguments);
      return new Promise(function(resolve, reject){
        fn.apply(null, args.concat([function(err, r){ err ? reject(err) : resolve(r); }]));
      });
    };
  },
  inspect: function(x){ try { return JSON.stringify(x); } catch(e){ return String(x); } },
  inherits: function(){},
  deprecate: function(fn){ return fn; },
  isArray: Array.isArray,
  isString: function(x){ return typeof x === "string"; },
  isObject: function(x){ return x !== null && typeof x === "object"; }
};
__mods["os"] = __osMod;
__mods["path"] = __pathMod;
__mods["fs"] = __fsMod;
["cheerio","crypto-js","opencc","chinese-conv","child_process","vm","stream","events","buffer","net","tls","zlib","http2","dns"].forEach(function(m){
  if (!__mods[m]) __mods[m] = {};
});

globalThis.require = function(name){
  if (Object.prototype.hasOwnProperty.call(__mods, name)) return __mods[name];
  return {};
};
globalThis.module = { exports: {} };
globalThis.exports = globalThis.module.exports;
globalThis.process = { env: __env || {}, version: "v20.0.0", platform: "linux" };
globalThis.console = {
  log: function(){ __log("info", Array.prototype.map.call(arguments, String).join(" ")); },
  warn: function(){ __log("warn", Array.prototype.map.call(arguments, String).join(" ")); },
  error: function(){ __log("error", Array.prototype.map.call(arguments, String).join(" ")); }
};
// ---- 定时器：Rust 侧定时泵驱动（__nextTimerDelay / __fireDueTimers，见 call_script_source）----
var __timers = {};
var __timerSeq = 1;
globalThis.setTimeout = function(fn, ms){
  if (typeof fn !== "function") return 0;
  var id = __timerSeq++;
  __timers[id] = { fn: fn, at: Date.now() + (Number(ms) || 0) };
  return id;
};
globalThis.clearTimeout = function(id){ delete __timers[id]; };
globalThis.setInterval = undefined;
globalThis.clearInterval = undefined;
globalThis.__nextTimerDelay = function(){
  var now = Date.now(), best = -1;
  for (var k in __timers){
    var d = __timers[k].at - now;
    if (best < 0 || d < best) best = d;
  }
  return best;
};
globalThis.__fireDueTimers = function(){
  var now = Date.now(), due = [];
  for (var k in __timers){ if (__timers[k].at <= now) due.push(k); }
  for (var i = 0; i < due.length; i++){
    var t = __timers[due[i]];
    delete __timers[due[i]];
    try { t.fn(); } catch(e) { try { __log("error", "timer: " + (e && e.message || e)); } catch(_){} }
  }
};
"#;

/// 把 serde_json::Value 转成 JS 可用的 JSON 字符串（作为调用参数）
fn params_to_js(params: &JsonValue) -> String {
    serde_json::to_string(params).unwrap_or_else(|_| "{}".into())
}

/// 执行一次脚本方法调用，返回 JS 返回值的 JSON
pub async fn call_script(script_id: &str, method: &str, params: JsonValue) -> Result<JsonValue> {
    let source = registry::read_source(script_id)
        .with_context(|| format!("读取脚本 {script_id} 失败"))?;
    call_script_source(&source, method, params).await
}

/// 用给定源码直接调用（测试接口用，不依赖 registry）
pub async fn call_script_source(
    source: &str,
    method: &str,
    params: JsonValue,
) -> Result<JsonValue> {
    let started = Instant::now();

    let rt = AsyncRuntime::new().context("创建 QuickJS Runtime 失败")?;
    rt.set_memory_limit(MEMORY_LIMIT).await;
    {
        let deadline = started + Duration::from_secs(20);
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
            .await;
    }
    let ctx = AsyncContext::full(&rt).await.context("创建 QuickJS Context 失败")?;

    let source = Arc::new(source.to_string());
    let method = Arc::new(method.to_string());
    let params_json = Arc::new(params_to_js(&params));

    let out: (String, Vec<String>) = ctx
        .async_with(async |ctx: Ctx| {
            let logs: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

            // ---- 注入原语 ----
            let lg = logs.clone();
            ctx.globals()
                .set("__log", Func::from(move |level: String, msg: String| {
                    if let Ok(mut v) = lg.lock() {
                        v.push(format!("[{level}] {msg}"));
                    }
                }))
                .unwrap();

            ctx.globals()
                .set("__request", Func::from(Async(js_request)))
                .unwrap();
            ctx.globals()
                .set("__cacheGet", Func::from(js_cache_get))
                .unwrap();
            ctx.globals()
                .set("__cacheSet", Func::from(js_cache_set))
                .unwrap();
            ctx.globals()
                .set("__b64encode", Func::from(js_b64_encode))
                .unwrap();
            ctx.globals()
                .set("__b64decode", Func::from(js_b64_decode))
                .unwrap();
            ctx.globals()
                .set("__hexencode", Func::from(js_hex_encode))
                .unwrap();
            ctx.globals()
                .set("__hexdecode", Func::from(js_hex_decode))
                .unwrap();
            ctx.globals()
                .set("__md5", Func::from(js_md5))
                .unwrap();
            ctx.globals().set("__aes", Func::from(crate::jscrypto::js_aes)).unwrap();
            ctx.globals().set("__rsa", Func::from(crate::jscrypto::js_rsa)).unwrap();
            ctx.globals().set("__rsaSign", Func::from(crate::jscrypto::js_rsa_sign)).unwrap();
            ctx.globals().set("__hash", Func::from(crate::jscrypto::js_hash)).unwrap();
            ctx.globals().set("__randHex", Func::from(crate::jscrypto::js_rand_hex)).unwrap();
            ctx.globals().set("__b64ToHex", Func::from(crate::jscrypto::js_b64_to_hex)).unwrap();
            ctx.globals().set("__hexToB64", Func::from(crate::jscrypto::js_hex_to_b64)).unwrap();
            ctx.globals().set("__fsGet", Func::from(js_fs_get)).unwrap();
            ctx.globals().set("__fsSet", Func::from(js_fs_set)).unwrap();
            ctx.globals().set("__fsDel", Func::from(js_fs_del)).unwrap();

            // 环境变量
            let env = serde_json::json!({
                "SITE_API": std::env::var("SITE_API").unwrap_or_default(),
                "DANMU_API": std::env::var("DANMU_API").unwrap_or_default(),
            });
            ctx.globals()
                .set("__env", env.to_string())
                .unwrap();

            // 先跑 polyfill，再跑脚本
            ctx.eval::<(), _>(POLYFILL).catch(&ctx).unwrap();

            // 内嵌库按需加载：只有脚本用到时才付出解析/求值开销
            // esbuild CJS 产物在闭包内求值（module/exports 就地提供），require 兜底走全局（node:stream 等返回 {}）
            if source.contains("cheerio") {
                let loader = format!(
                    "__mods[\"cheerio\"] = (function(){{ var module = {{ exports: {{}} }}, exports = module.exports; (function(){{{}}} )(); return module.exports; }})();",
                    CHEERIO_BUNDLE
                );
                ctx.eval::<(), _>(loader).catch(&ctx).unwrap();
            }
            if source.contains("crypto-js") {
                let loader = format!(
                    "__mods[\"crypto-js\"] = (function(){{ var module = {{ exports: {{}} }}, exports = module.exports; (function(){{{}}} )(); return module.exports; }})();",
                    CRYPTO_JS_BUNDLE
                );
                ctx.eval::<(), _>(loader).catch(&ctx).unwrap();
            }

            let call_expr = format!(
                r#"
var __src = {};
var __params = JSON.parse({});
var __method = {};
globalThis.module = {{ exports: {{}} }};
globalThis.exports = globalThis.module.exports;
(function(){{ return eval(__src); }})();
(async function(){{
  var fn = module.exports[__method];
  if (typeof fn !== "function") {{ return JSON.stringify({{__err: "脚本未导出方法 " + __method}}); }}
  try {{
    var r = await fn(__params, {{from:"web"}});
    return JSON.stringify(r === undefined ? null : r);
  }} catch(e) {{
    return JSON.stringify({{__err: String(e && e.message || e)}});
  }}
}})()
"#,
                json_literal(&source),
                json_literal(&params_json),
                json_literal(&method),
            );

            let result: String = match ctx.eval::<Promise, _>(call_expr.as_str()).catch(&ctx) {
                Ok(promise) => {
                    // 主 promise 等待期间驱动 setTimeout 定时泵；
                    // 整体 30s 上限（引擎内中断 20s 兜底 JS 执行，此处兜 promise 挂起）
                    let pump_ctx = ctx.clone();
                    let overall = async {
                        let mut main_fut = std::pin::pin!(promise.into_future::<String>());
                        loop {
                            let r = tokio::select! {
                                biased;
                                r = &mut main_fut => r,
                                _ = pump_step(&pump_ctx) => continue,
                            };
                            break r.catch(&ctx).unwrap_or_else(|e| {
                                serde_json::json!({"__err": format!("JS 执行异常: {:?}", e)}).to_string()
                            });
                        }
                    };
                    match tokio::time::timeout(Duration::from_secs(30), overall).await {
                        Ok(res) => res,
                        Err(_) => serde_json::json!({"__err": "脚本执行超时（30s，疑似定时器未决）"}).to_string(),
                    }
                }
                Err(e) => {
                    serde_json::json!({"__err": format!("脚本求值失败: {:?}", e)}).to_string()
                }
            };
            let logs = logs.lock().map(|v| v.clone()).unwrap_or_default();
            (result, logs)
        })
        .await;

    let (json_str, _logs) = out;
    let value: JsonValue = serde_json::from_str(&json_str).unwrap_or_else(|_| {
        serde_json::json!({ "__err": format!("返回值解析失败: {}", &json_str[..json_str.len().min(200)]) })
    });
    if let Some(err) = value.get("__err").and_then(JsonValue::as_str) {
        anyhow::bail!("{err}");
    }
    Ok(value)
}

/// 把字符串编码成 JS 字符串字面量（安全转义）
fn json_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// 定时泵单步：查最近定时器 → 睡到到期 → 触发回调。
/// 无挂起定时器时短暂让出，等待主 promise 的任务推进中可能新建的定时器。
async fn pump_step(ctx: &Ctx<'_>) {
    let d = ctx
        .eval::<f64, _>("__nextTimerDelay()")
        .catch(ctx)
        .unwrap_or(-1.0);
    if d < 0.0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        return;
    }
    tokio::time::sleep(Duration::from_millis(d.clamp(0.0, 100.0) as u64)).await;
    let _ = ctx
        .eval::<(), _>("__fireDueTimers()")
        .catch(ctx)
        .ok();
}

// ===== Rust 原语 =====

/// 网络请求原语。为绕过 rquickjs 对异步函数返回类型的严格约束，
/// 永远返回 Ok(String)：失败时 JSON 里带 __err，由 JS polyfill 抛出。
async fn js_request(url: String, opts_json: String) -> rquickjs::Result<String> {
    let result = js_request_inner(&url, &opts_json).await;
    Ok(match result {
        Ok(v) => v.to_string(),
        Err(e) => serde_json::json!({"__err": e.to_string()}).to_string(),
    })
}

async fn js_request_inner(url: &str, opts_json: &str) -> anyhow::Result<JsonValue> {
    let opts: JsonValue = serde_json::from_str(opts_json).unwrap_or_else(|_| serde_json::json!({}));
    let method = opts.get("method").and_then(JsonValue::as_str).unwrap_or("GET").to_string();
    let url_parsed = reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("URL 解析失败: {e}"))?;
    match url_parsed.scheme() {
        "http" | "https" => {}
        _ => anyhow::bail!("仅支持 http/https"),
    }
    if let Some(host) = url_parsed.host_str() {
        if registry::is_private_host(host) {
            anyhow::bail!("禁止访问内网地址");
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP 客户端失败: {e}"))?;

    let mut req = client.request(
        method.parse().unwrap_or(reqwest::Method::GET),
        url_parsed,
    );
    if let Some(headers) = opts.get("headers").and_then(JsonValue::as_object) {
        for (k, v) in headers {
            if let Some(s) = v.as_str() {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) {
                    if let Ok(val) = reqwest::header::HeaderValue::from_str(s) {
                        req = req.header(name, val);
                    }
                }
            }
        }
    }
    // 二进制请求体：http(s) shim 对 Buffer/protobuf 体用 __body_b64 传递（字符串编码会损坏字节）
    if let Some(b64) = opts.get("__body_b64").and_then(JsonValue::as_str) {
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => req = req.body(bytes),
            Err(_) => anyhow::bail!("__body_b64 解码失败"),
        }
    } else if let Some(body) = opts.get("body").and_then(JsonValue::as_str) {
        req = req.body(body.to_string());
    }

    let resp = req.send().await.map_err(|e| anyhow::anyhow!("请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let headers: serde_json::Map<String, JsonValue> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), JsonValue::String(s.to_string()))))
        .collect();
    let raw = resp.bytes().await.map_err(|e| anyhow::anyhow!("读取响应失败: {e}"))?;
    let raw = if raw.len() > 5 * 1024 * 1024 {
        raw[..5 * 1024 * 1024].to_vec()
    } else {
        raw.to_vec()
    };
    // 文本体（合法 UTF-8）直接给字符串；含非 UTF-8 字节（gbk 站点/二进制）额外给 base64
    let mut out = serde_json::json!({
        "statusCode": status,
        "body": String::from_utf8_lossy(&raw).into_owned(),
        "headers": headers,
    });
    if String::from_utf8(raw.clone()).is_err() {
        use base64::Engine as _;
        out["__body_b64"] = JsonValue::String(base64::engine::general_purpose::STANDARD.encode(&raw));
    }
    Ok(out)
}

// ---- fs 持久化：脚本里的任意路径只当作"键"，映射到 数据目录/.fs/ 下的固定文件，不会触达真实文件系统 ----
const FS_MAX_BYTES: usize = 1024 * 1024;

fn fs_file(path: &str) -> std::path::PathBuf {
    use sha2::Digest;
    let safe: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .rev()
        .take(60)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let h = sha2::Sha256::digest(path.as_bytes());
    let h: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    registry::debug_dir().join(".fs").join(format!("{safe}-{h}"))
}

/// 返回 JSON：文件内容字符串，或 null（不存在）
fn js_fs_get(path: String) -> String {
    match std::fs::read_to_string(fs_file(&path)) {
        Ok(s) => serde_json::to_string(&s).unwrap_or_else(|_| "null".into()),
        Err(_) => "null".into(),
    }
}

fn js_fs_set(path: String, data: String) -> bool {
    if data.len() > FS_MAX_BYTES {
        return false;
    }
    let f = fs_file(&path);
    if let Some(dir) = f.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return false;
        }
    }
    std::fs::write(f, data).is_ok()
}

fn js_fs_del(path: String) -> bool {
    std::fs::remove_file(fs_file(&path)).is_ok()
}

fn js_cache_get(key: String) -> String {
    match crate::cache::get(&key) {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}

fn js_cache_set(key: String, val_json: String, ttl: f64) {
    if let Ok(v) = serde_json::from_str::<JsonValue>(&val_json) {
        let ttl = if ttl <= 0.0 { 300.0 } else { ttl };
        crate::cache::set(&key, v, Duration::from_secs_f64(ttl));
    }
}

fn js_b64_encode(data: String, variant: String) -> String {
    use base64::Engine;
    let bytes = data.as_bytes();
    if variant == "base64url" {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    } else {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

fn js_b64_decode(data: String, variant: String) -> String {
    use base64::Engine;
    let r = if variant == "base64url" {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data.as_bytes())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data.as_bytes()))
    } else {
        base64::engine::general_purpose::STANDARD.decode(data.as_bytes())
    };
    String::from_utf8_lossy(&r.unwrap_or_default()).into_owned()
}

fn js_hex_encode(data: String) -> String {
    data.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn js_hex_decode(data: String) -> String {
    let mut out = Vec::new();
    let b = data.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(&b[i..i + 2]), 16) {
            out.push(v);
        }
        i += 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 轻量 MD5 实现（无外部依赖），返回小写 hex
fn js_md5(input: String) -> String {
    fn md5(bytes: &[u8]) -> [u8; 16] {
        const S: [u32; 64] = [
            7,12,17,22, 7,12,17,22, 7,12,17,22, 7,12,17,22,
            5, 9,14,20, 5, 9,14,20, 5, 9,14,20, 5, 9,14,20,
            4,11,16,23, 4,11,16,23, 4,11,16,23, 4,11,16,23,
            6,10,15,21, 6,10,15,21, 6,10,15,21, 6,10,15,21,
        ];
        const K: [u32; 64] = [
            0xd76aa478,0xe8c7b756,0x242070db,0xc1bdceee,0xf57c0faf,0x4787c62a,0xa8304613,0xfd469501,
            0x698098d8,0x8b44f7af,0xffff5bb1,0x895cd7be,0x6b901122,0xfd987193,0xa679438e,0x49b40821,
            0xf61e2562,0xc040b340,0x265e5a51,0xe9b6c7aa,0xd62f105d,0x02441453,0xd8a1e681,0xe7d3fbc8,
            0x21e1cde6,0xc33707d6,0xf4d50d87,0x455a14ed,0xa9e3e905,0xfcefa3f8,0x676f02d9,0x8d2a4c8a,
            0xfffa3942,0x8771f681,0x6d9d6122,0xfde5380c,0xa4beea44,0x4bdecfa9,0xf6bb4b60,0xbebfbc70,
            0x289b7ec6,0xeaa127fa,0xd4ef3085,0x04881d05,0xd9d4d039,0xe6db99e5,0x1fa27cf8,0xc4ac5665,
            0xf4292244,0x432aff97,0xab9423a7,0xfc93a039,0x655b59c3,0x8f0ccc92,0xffeff47d,0x85845dd1,
            0x6fa87e4f,0xfe2ce6e0,0xa3014314,0x4e0811a1,0xf7537e82,0xbd3af235,0x2ad7d2bb,0xeb86d391,
        ];
        let mut msg = bytes.to_vec();
        let bit_len = (msg.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 { msg.push(0); }
        msg.extend_from_slice(&bit_len.to_le_bytes());
        let (mut a0,mut b0,mut c0,mut d0) = (0x67452301u32,0xefcdab89u32,0x98badcfeu32,0x10325476u32);
        for chunk in msg.chunks(64) {
            let mut m = [0u32; 16];
            for i in 0..16 {
                m[i] = u32::from_le_bytes([chunk[i*4],chunk[i*4+1],chunk[i*4+2],chunk[i*4+3]]);
            }
            let (mut a,mut b,mut c,mut d) = (a0,b0,c0,d0);
            for i in 0..64 {
                let (f,g) = if i < 16 { ((b & c) | ((!b) & d), i) }
                    else if i < 32 { ((d & b) | ((!d) & c), (5*i+1)%16) }
                    else if i < 48 { (b ^ c ^ d, (3*i+5)%16) }
                    else { (c ^ (b | (!d)), (7*i)%16) };
                let tmp = d;
                d = c; c = b;
                b = b.wrapping_add(a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]).rotate_left(S[i]));
                a = tmp;
            }
            a0 = a0.wrapping_add(a); b0 = b0.wrapping_add(b);
            c0 = c0.wrapping_add(c); d0 = d0.wrapping_add(d);
        }
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&a0.to_le_bytes());
        out[4..8].copy_from_slice(&b0.to_le_bytes());
        out[8..12].copy_from_slice(&c0.to_le_bytes());
        out[12..16].copy_from_slice(&d0.to_le_bytes());
        out
    }
    md5(input.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}
