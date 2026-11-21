// node.rs —— Node.js 子进程运行时（常驻进程 + stdio NDJSON 双向 RPC）
// 与 spider.rs(QuickJS) 并列：脚本 meta.runtime == "node" 时走这里。
// Node 原生提供 crypto(AES/RSA/md5)、https/http、Buffer，覆盖 QuickJS 无法实现的加密协议源（如加密签名协议源）。
//
// 协议（每行一条 JSON）：
//   Rust -> Node : {"t":"call","id":N,"file":"...","method":"search","params":{},"env":{}}
//   Rust -> Node : {"t":"http_resp","id":N,"ok":true,"data":{"statusCode":200,"body":"...","headers":{}}}
//   Node -> Rust : {"t":"http_req","id":N,"url":"...","opts":{...}}
//   Node -> Rust : {"t":"log","level":"info","msg":"..."}
//   Node -> Rust : {"t":"call_result","id":N,"ok":true,"data":{...}}
//   Node 脚本内的 OmniBox.request / axios 都桥接回 Rust 发请求，沿用 is_private_host 安全边界。
//   第三方 npm 包（cheerio 等）由 runner.js 在 require 缺失时按需同步自动安装到 SPIDER_DIR/node_modules，
//   安装进度通过 {"t":"log"} 回传；白名单限制可自动安装的包。

use crate::registry;
use anyhow::{anyhow, Context as _, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use std::process::Stdio;
use tokio::sync::{oneshot, Mutex as AsyncMutex};

const RUNNER_JS: &str = include_str!("../static/runner.js");
const CRYPTO_JS_SHIM: &str = include_str!("../static/crypto-js-shim.js");
// 首次执行可能触发 npm install（如 cheerio），需放宽超时；安装只在首次发生，之后调用很快。
const CALL_TIMEOUT: Duration = Duration::from_secs(180);

struct Proc {
    child: Child,
    stdin: ChildStdin,
    next_req: AtomicU64,
    pending: HashMap<u64, oneshot::Sender<Result<Value, String>>>,
}

// 全局持有当前 Node 进程（可能为空）。Arc<AsyncMutex<Proc>> 便于 reader/http task 共享。
type ProcRef = Arc<AsyncMutex<Proc>>;
static NODE: OnceLock<AsyncMutex<Option<ProcRef>>> = OnceLock::new();

fn slot() -> &'static AsyncMutex<Option<ProcRef>> {
    NODE.get_or_init(|| AsyncMutex::new(None))
}

/// 检测 node 运行时是否可用
pub async fn available() -> (bool, String) {
    match Command::new("node").arg("--version").output().await {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (true, v)
        }
        Ok(out) => (false, format!("node 退出码 {}", out.status)),
        Err(e) => (false, format!("未找到 node: {e}")),
    }
}

fn runner_path() -> Result<std::path::PathBuf> {
    let dir = registry::debug_dir();
    let p = dir.join(".runner.js");
    std::fs::write(&p, RUNNER_JS).context("写入 node runner 失败")?;
    // crypto-js shim 与 runner 同目录，runner 通过 require('./.crypto-js-shim.js') 加载
    std::fs::write(dir.join(".crypto-js-shim.js"), CRYPTO_JS_SHIM)
        .context("写入 crypto-js shim 失败")?;
    Ok(p)
}

async fn spawn_proc() -> Result<ProcRef> {
    let path = runner_path()?;
    let data_dir = registry::debug_dir();
    // 让脚本能 require 到 data/spiders/node_modules 里安装的 cheerio 等包
    let node_modules = data_dir.join("node_modules");
    let node_path = match std::env::var("NODE_PATH") {
        Ok(existing) if !existing.is_empty() => format!("{}:{}", node_modules.display(), existing),
        _ => node_modules.to_string_lossy().to_string(),
    };
    // 传给 runner 绝对的脚本目录，避免相对路径歧义，用于按需安装依赖到其 node_modules。
    let spider_dir = std::fs::canonicalize(&data_dir).unwrap_or_else(|_| data_dir.clone());
    let mut child = Command::new("node")
        .arg(&path)
        .env("DATA_DIR", std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into()))
        .env("SPIDER_DIR", &spider_dir)
        .env("NODE_PATH", node_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 node 子进程失败（请确认已安装 Node.js 18+）")?;
    let stdin = child.stdin.take().context("node stdin 不可用")?;
    let stdout = child.stdout.take().context("node stdout 不可用")?;
    let stderr = child.stderr.take().context("node stderr 不可用")?;

    let proc = Arc::new(AsyncMutex::new(Proc {
        child,
        stdin,
        next_req: AtomicU64::new(1),
        pending: HashMap::new(),
    }));

    // stderr -> 日志
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[node] {line}");
        }
    });

    // stdout -> 消息分发
    let proc_reader = proc.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let t = msg.get("t").and_then(Value::as_str).unwrap_or("");
            match t {
                "http_req" => {
                    let id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let url = msg.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                    let opts = msg.get("opts").cloned().unwrap_or_else(|| json!({}));
                    let p = proc_reader.clone();
                    tokio::spawn(async move {
                        let data = http_request(&url, &opts).await;
                        let reply = match data {
                            Ok(v) => json!({"t":"http_resp","id":id,"ok":true,"data":v}),
                            Err(e) => json!({"t":"http_resp","id":id,"ok":false,"error":e.to_string()}),
                        };
                        let mut guard = p.lock().await;
                        let _ = write_line(&mut guard.stdin, &reply).await;
                    });
                }
                "log" => {
                    let level = msg.get("level").and_then(Value::as_str).unwrap_or("info");
                    let m = msg.get("msg").and_then(Value::as_str).unwrap_or("");
                    eprintln!("[node:{level}] {m}");
                }
                "call_result" => {
                    let id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let ok = msg.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let tx = guard_pending(&proc_reader, id).await;
                    if let Some(tx) = tx {
                        let result = if ok {
                            Ok(msg.get("data").cloned().unwrap_or_else(|| json!(null)))
                        } else {
                            Err(msg.get("error").and_then(Value::as_str).unwrap_or("脚本执行失败").to_string())
                        };
                        let _ = tx.send(result);
                    }
                }
                _ => {}
            }
        }
        // 进程退出：清空全局并让所有等待中的调用失败，下次调用会自动重启
        let mut g = slot().lock().await;
        if g.as_ref().map(|p| Arc::ptr_eq(p, &proc_reader)).unwrap_or(false) {
            *g = None;
        }
        drop(g);
        let mut guard = proc_reader.lock().await;
        for (_, tx) in guard.pending.drain() {
            let _ = tx.send(Err("node 进程已退出".into()));
        }
    });

    Ok(proc)
}

async fn guard_pending(proc: &ProcRef, id: u64) -> Option<oneshot::Sender<Result<Value, String>>> {
    let mut guard = proc.lock().await;
    guard.pending.remove(&id)
}

async fn write_line(stdin: &mut ChildStdin, msg: &Value) -> Result<()> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn ensure_proc() -> Result<ProcRef> {
    let mut g = slot().lock().await;
    if let Some(p) = g.as_ref() {
        return Ok(p.clone());
    }
    let p = spawn_proc().await?;
    *g = Some(p.clone());
    Ok(p)
}

/// 终止当前 node 进程（超时/崩溃后调用），下次调用自动重启
async fn kill_proc() {
    let p = {
        let mut g = slot().lock().await;
        g.take()
    };
    if let Some(p) = p {
        let mut guard = p.lock().await;
        let _ = guard.child.kill().await;
        for (_, tx) in guard.pending.drain() {
            let _ = tx.send(Err("node 调用超时".into()));
        }
    }
}

/// 通过 Node 运行时调用脚本方法（按绝对路径加载脚本文件）
pub async fn call_script_file(file: &std::path::Path, method: &str, params: Value) -> Result<Value> {
    let proc = ensure_proc().await?;

    let (id, rx) = {
        let mut guard = proc.lock().await;
        let id = guard.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        guard.pending.insert(id, tx);
        (id, rx)
    };

    let env = json!({
        "SITE_API": std::env::var("SITE_API").unwrap_or_default(),
        "DANMU_API": std::env::var("DANMU_API").unwrap_or_default(),
    });
    let call = json!({
        "t": "call",
        "id": id,
        "file": file.to_string_lossy(),
        "method": method,
        "params": params,
        "env": env,
    });

    {
        let mut guard = proc.lock().await;
        if let Err(e) = write_line(&mut guard.stdin, &call).await {
            guard.pending.remove(&id);
            let _ = kill_proc().await;
            return Err(anyhow!("向 node 写入调用失败: {e}"));
        }
    }

    match tokio::time::timeout(CALL_TIMEOUT, rx).await {
        Ok(Ok(result)) => result.map_err(anyhow::Error::msg),
        Ok(Err(_)) => Err(anyhow!("node 响应通道关闭")),
        Err(_) => {
            kill_proc().await;
            Err(anyhow!("node 脚本调用超时（{}s）", CALL_TIMEOUT.as_secs()))
        }
    }
}

/// 便捷入口：按 registry 脚本 id 调用（读取其文件路径）
pub async fn call_script(script_id: &str, method: &str, params: Value) -> Result<Value> {
    let path = registry::source_path(script_id)
        .ok_or_else(|| anyhow!("脚本不存在: {script_id}"))?;
    call_script_file(&path, method, params).await
}

/// Node 侧 OmniBox.request 桥接回 Rust 执行，沿用 QuickJS 相同的安全边界。
async fn http_request(url: &str, opts: &Value) -> Result<Value> {
    let method = opts.get("method").and_then(Value::as_str).unwrap_or("GET");
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow!("URL 解析失败: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => anyhow::bail!("仅支持 http/https"),
    }
    if let Some(host) = parsed.host_str() {
        if registry::is_private_host(host) {
            anyhow::bail!("禁止访问内网地址");
        }
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|e| anyhow!("HTTP 客户端失败: {e}"))?;
    let mut req = client.request(method.parse().unwrap_or(reqwest::Method::GET), parsed);
    if let Some(headers) = opts.get("headers").and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(s) = v.as_str() {
                if let (Ok(name), Ok(val)) = (
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                    reqwest::header::HeaderValue::from_str(s),
                ) {
                    req = req.header(name, val);
                }
            }
        }
    }
    if let Some(body) = opts.get("body").and_then(Value::as_str) {
        req = req.body(body.to_string());
    }
    let resp = req.send().await.map_err(|e| anyhow!("请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let headers: serde_json::Map<String, Value> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), Value::String(s.to_string()))))
        .collect();
    let mut body = resp.text().await.map_err(|e| anyhow!("读取响应失败: {e}"))?;
    if body.len() > 5 * 1024 * 1024 {
        body.truncate(5 * 1024 * 1024);
    }
    Ok(json!({"statusCode": status, "body": body, "headers": headers}))
}
