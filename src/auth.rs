// auth.rs —— 访问鉴权：启动参数 -auth <密码> 启用（默认关闭）
// 机制：随机会话 token（内存存储，7 天有效）+ 按 IP 防爆破锁定

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SESSION_COOKIE: &str = "omni_session";
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// 窗口内连续失败达到该次数即锁定
const MAX_FAILS: u32 = 5;
/// 失败计数窗口
const FAIL_WINDOW: Duration = Duration::from_secs(10 * 60);
/// 锁定基数，之后每次失败翻倍，封顶 15 分钟
const LOCK_BASE: Duration = Duration::from_secs(30);
const LOCK_MAX: Duration = Duration::from_secs(15 * 60);

struct FailRec {
    count: u32,
    first_fail: Instant,
    locked_until: Option<Instant>,
}

pub struct AuthState {
    pub enabled: bool,
    pass_hash: [u8; 32],
    sessions: Mutex<HashMap<String, Instant>>,
    fails: Mutex<HashMap<IpAddr, FailRec>>,
}

impl AuthState {
    pub fn new(enabled: bool, password: &str) -> Self {
        AuthState {
            enabled,
            pass_hash: sha256(password.as_bytes()),
            sessions: Mutex::new(HashMap::new()),
            fails: Mutex::new(HashMap::new()),
        }
    }

    fn verify(&self, password: &str) -> bool {
        let given = sha256(password.as_bytes());
        // 常量时间比较，避免时序侧信道
        let mut diff = 0u8;
        for (a, b) in given.iter().zip(self.pass_hash.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// 创建会话，返回 token
    pub fn create_session(&self) -> String {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        let token = hex(&buf);
        let mut sessions = self.sessions.lock().unwrap();
        // 顺带清理过期会话
        let now = Instant::now();
        sessions.retain(|_, exp| *exp > now);
        sessions.insert(token.clone(), now + SESSION_TTL);
        token
    }

    pub fn session_valid(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(token) {
            Some(exp) if *exp > Instant::now() => true,
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }

    pub fn drop_session(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    /// 登录尝试。返回 Ok(token) 或 Err(剩余锁定秒数)
    pub fn try_login(&self, ip: IpAddr, password: &str) -> Result<String, u64> {
        let mut fails = self.fails.lock().unwrap();
        let now = Instant::now();
        let rec = fails.entry(ip).or_insert(FailRec {
            count: 0,
            first_fail: now,
            locked_until: None,
        });

        // 窗口过期则重置
        if now.duration_since(rec.first_fail) > FAIL_WINDOW {
            rec.count = 0;
            rec.first_fail = now;
        }

        if let Some(until) = rec.locked_until {
            if now < until {
                return Err((until - now).as_secs().max(1));
            }
            rec.locked_until = None;
            rec.count = 0;
            rec.first_fail = now;
        }

        if self.verify(password) {
            fails.remove(&ip);
            Ok(self.create_session())
        } else {
            rec.count += 1;
            if rec.count >= MAX_FAILS {
                let lock = LOCK_BASE
                    .saturating_mul(1 << (rec.count - MAX_FAILS).min(10))
                    .min(LOCK_MAX);
                rec.locked_until = Some(now + lock);
                Err(lock.as_secs())
            } else {
                Err(0)
            }
        }
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 从 Cookie 头提取 omni_session
pub fn extract_token(parts: &axum::http::request::Parts) -> Option<String> {
    let v = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in v.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(SESSION_COOKIE).and_then(|r| r.strip_prefix('=')) {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 提取客户端 IP（无 ConnectInfo 时退化为全局单一 key，防爆破仍然生效）
pub fn client_ip(parts: &axum::http::request::Parts) -> IpAddr {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// 登录页 / 登录接口 / 静态资源不需要鉴权
const EXEMPT_PATHS: &[&str] = &[
    "/login.html",
    "/api/login",
    "/app.css",
    "/icon.svg",
    "/icon-192.png",
    "/icon-512.png",
    "/apple-touch-icon.png",
    "/favicon.ico",
];

/// 全局鉴权中间件：未登录访问 API 返回 401 JSON，访问页面 302 跳登录页
pub async fn middleware(State(auth): State<Arc<AuthState>>, req: Request, next: Next) -> Response {
    if !auth.enabled {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    if EXEMPT_PATHS.contains(&path.as_str()) {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
    let authed = extract_token(&parts)
        .map(|t| auth.session_valid(&t))
        .unwrap_or(false);
    if authed {
        let req = Request::from_parts(parts, body);
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"ok": false, "error": "未登录或会话已过期"})),
        )
            .into_response()
    } else {
        // 页面请求跳转登录页，并带上回跳地址
        let next_url = if path == "/" {
            String::new()
        } else {
            format!("?next={}", urlencoding::encode(&parts.uri.to_string()))
        };
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::FOUND;
        resp.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("/login.html{next_url}")).unwrap(),
        );
        resp
    }
}

/// 登录成功后种 cookie（HttpOnly + SameSite=Lax）
pub fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

pub fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}
