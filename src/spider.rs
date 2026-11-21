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
  create: function(defaults){
    defaults = defaults || {};
    var inst = {
      defaults: defaults,
      get: function(url, cfg){ cfg = Object.assign({}, defaults, cfg || {}); return __axios.get(url, cfg); },
      post: function(url, data, cfg){ cfg = Object.assign({}, defaults, cfg || {}); return __axios.post(url, data, cfg); },
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
      if (o === "base64" || o === "base64url") return __b64encode(s, o);
      if (o === "hex") return __hexencode(s);
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
    var s = __b64decode(String(data), enc), out=[];
    for (var i=0;i<s.length;i++) out.push(s.charCodeAt(i)&255);
    return out;
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
  keys: function(){ return this._m.map(function(p){ return p[0]; }); },
  values: function(){ return this._m.map(function(p){ return p[1]; }); }
};
globalThis.URL = function(u, base){
  if (base && !/^https?:/i.test(String(u))){
    var b = String(base).replace(/\/[^/]*$/, "/");
    u = b + String(u).replace(/^\.\//,"").replace(/^\//,"");
  }
  this.href = String(u);
  var m = /^(https?:)\/\/([^\/?#]+)([^?#]*)(\?[^#]*)?(#.*)?$/.exec(String(u));
  if (m){
    this.protocol = m[1]; this.host = m[2]; this.pathname = m[3] || "/";
    this.search = m[4] || ""; this.hash = m[5] || "";
    this.searchParams = new globalThis.URLSearchParams(this.search || "");
  } else {
    this.protocol=""; this.host=""; this.pathname=String(u); this.search=""; this.hash="";
    this.searchParams = new globalThis.URLSearchParams();
  }
};
globalThis.encodeURIComponent = encodeURIComponent;
globalThis.decodeURIComponent = decodeURIComponent;

// http/https 占位（含 Agent 构造器，避免 new http.Agent() 报错）
var __agentMod = { Agent: function(opts){ this.options = opts || {}; } };
__mods["http"] = __agentMod;
__mods["https"] = __agentMod;

// ---- 其他模块占位 ----
var __fsMem = {};
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
  existsSync: function(p){ return Object.prototype.hasOwnProperty.call(__fsMem, String(p)); },
  readFileSync: function(p){ return __fsMem[String(p)] || ""; },
  writeFileSync: function(p, d){ __fsMem[String(p)] = String(d); },
  mkdirSync: function(){},
  readdirSync: function(){ return []; }
};
// ---- crypto 基础兼容层（md5 / randomBytes；AES/RSA 需原生 Node，脚本调用会走 stub）----
// md5 用 Rust 原语；randomBytes 用时间戳+随机数
var __cryptoMod = {
  createHash: function(alg){
    var parts = [];
    return {
      update: function(d){ parts.push(typeof d === "string" ? d : (d && d.toString ? d.toString("utf8") : String(d))); return this; },
      digest: function(enc){
        var hex = __md5(parts.join(""));
        return enc === "hex" ? hex : __mkbuf(__hexToBytes(hex));
      }
    };
  },
  randomBytes: function(n){
    n = Number(n) || 16; var out = [];
    for (var i=0;i<n;i++) out.push(Math.floor(Math.random()*256));
    return __mkbuf(out);
  },
  constants: { RSA_PKCS1_PADDING: 1, RSA_NO_PADDING: 3 },
  createCipheriv: function(){ throw new Error("AES 加密在当前运行环境不支持"); },
  createDecipheriv: function(){ throw new Error("AES 解密在当前运行环境不支持"); },
  publicEncrypt: function(){ throw new Error("RSA 加密在当前运行环境不支持"); },
  privateDecrypt: function(){ throw new Error("RSA 解密在当前运行环境不支持"); }
};
function __hexToBytes(h){ var out=[]; for(var i=0;i+1<h.length;i+=2) out.push(parseInt(h.substr(i,2),16)); return out; }
__mods["crypto"] = __cryptoMod;

__mods["os"] = __osMod;
__mods["path"] = __pathMod;
__mods["fs"] = __fsMod;
["cheerio","util","child_process","vm","url","querystring","crypto-js","stream","events","buffer","net","tls","zlib"].forEach(function(m){
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
if (typeof setTimeout === "undefined") globalThis.setTimeout = function(){ return 0; };
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
                Ok(promise) => promise
                    .into_future::<String>()
                    .await
                    .catch(&ctx)
                    .unwrap_or_else(|e| {
                        serde_json::json!({"__err": format!("JS 执行异常: {:?}", e)}).to_string()
                    }),
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
    if let Some(body) = opts.get("body").and_then(JsonValue::as_str) {
        req = req.body(body.to_string());
    }

    let resp = req.send().await.map_err(|e| anyhow::anyhow!("请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let headers: serde_json::Map<String, JsonValue> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), JsonValue::String(s.to_string()))))
        .collect();
    let body = resp.text().await.map_err(|e| anyhow::anyhow!("读取响应失败: {e}"))?;
    let body = if body.len() > 5 * 1024 * 1024 {
        body[..5 * 1024 * 1024].to_string()
    } else {
        body
    };
    Ok(serde_json::json!({"statusCode": status, "body": body, "headers": headers}))
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
