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
globalThis.Buffer = {
  from: function(data, enc){
    enc = enc || "utf8";
    if (typeof data === "number") data = "";
    var bytes = (enc === "base64" || enc === "base64url") ? __b64decode(String(data), enc)
              : (enc === "hex" ? __hexdecode(String(data)) : String(data));
    bytes = String(bytes);
    return {
      toString: function(o){
        o = o || "utf8";
        if (o === "base64" || o === "base64url") return __b64encode(bytes, o);
        if (o === "hex") return __hexencode(bytes);
        return bytes;
      },
      length: bytes.length
    };
  },
  byteLength: function(s){ return String(s).length; }
};

// ---- 其他模块占位 ----
["cheerio","http","https","crypto","fs","path","util","child_process","vm","url","querystring","crypto-js"].forEach(function(m){
  __mods[m] = {};
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
                    .unwrap_or_else(|_| r#"{"__err":"JS 执行异常"}"#.to_string()),
                Err(_) => r#"{"__err":"脚本求值失败（语法错误或不兼容的 API）"}"#.to_string(),
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
