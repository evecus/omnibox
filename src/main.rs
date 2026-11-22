// main.rs —— OmniBox 入口：豆瓣首页 + QuickJS 动态脚本源 + 网页面板

mod auth;
mod cache;
mod douban;
mod history;
mod jsdom;
mod jscrypto;
mod proxy;
mod registry;
mod spider;
mod vod;

use axum::extract::{Multipart, Query};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("../static/index.html");
const SEARCH_HTML: &str = include_str!("../static/search.html");
const PLAY_HTML: &str = include_str!("../static/play.html");
const SPIDERS_HTML: &str = include_str!("../static/spiders.html");
const HISTORY_HTML: &str = include_str!("../static/history.html");
const LOGIN_HTML: &str = include_str!("../static/login.html");
const APP_CSS: &str = include_str!("../static/app.css");
const MANIFEST: &str = include_str!("../static/manifest.json");
const SW_JS: &str = include_str!("../static/sw.js");
const HLS_JS: &str = include_str!("../static/hls.min.js");
const ICON_SVG: &str = include_str!("../static/icon.svg");

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}
async fn search_page() -> Html<&'static str> {
    Html(SEARCH_HTML)
}
async fn play_page() -> Html<&'static str> {
    Html(PLAY_HTML)
}
async fn spiders_page() -> Html<&'static str> {
    Html(SPIDERS_HTML)
}
async fn history_page() -> Html<&'static str> {
    Html(HISTORY_HTML)
}
async fn login_page() -> Html<&'static str> {
    Html(LOGIN_HTML)
}
async fn css() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}
async fn icon_svg() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], ICON_SVG)
}
async fn manifest() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/manifest+json; charset=utf-8")], MANIFEST)
}
async fn sw() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/javascript; charset=utf-8")], SW_JS)
}
async fn hls_js() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/javascript; charset=utf-8")], HLS_JS)
}
async fn icon192() -> impl IntoResponse {
    ([("content-type", "image/png")], include_bytes!("../static/icon-192.png").as_slice())
}
async fn icon512() -> impl IntoResponse {
    ([("content-type", "image/png")], include_bytes!("../static/icon-512.png").as_slice())
}
async fn apple_touch_icon() -> impl IntoResponse {
    ([("content-type", "image/png")], include_bytes!("../static/apple-touch-icon.png").as_slice())
}
async fn favicon_ico() -> impl IntoResponse {
    // 现代浏览器直接使用 head 里声明的 /icon.svg；此路由兜底 /favicon.ico 直接请求
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], ICON_SVG)
}
async fn health() -> impl IntoResponse {
    axum::Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "engines": {
            "quickjs": true,
        }
    }))
}

// ===== 播放历史 API =====

async fn api_history_list() -> axum::Json<serde_json::Value> {
    axum::Json(json!({"ok": true, "list": history::list()}))
}

async fn api_history_add(Query(q): Query<BTreeMap<String, String>>) -> axum::Json<serde_json::Value> {
    let src = q.get("src").cloned().unwrap_or_default();
    let id = q.get("id").cloned().unwrap_or_default();
    if src.is_empty() || id.is_empty() {
        return axum::Json(json!({"ok": false, "error": "src/id 不能为空"}));
    }
    let name = q.get("name").cloned().unwrap_or_default();
    let pic = q.get("pic").cloned().unwrap_or_default();
    let ep_index = q.get("ep").and_then(|x| x.parse::<usize>().ok()).unwrap_or(0);
    let ep_name = q.get("epname").cloned().unwrap_or_default();
    match history::add(&src, &id, &name, &pic, ep_index, &ep_name) {
        Ok(list) => axum::Json(json!({"ok": true, "list": list})),
        Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_history_remove(Query(q): Query<BTreeMap<String, String>>) -> axum::Json<serde_json::Value> {
    let key = q.get("key").cloned().unwrap_or_default();
    if key.is_empty() {
        return axum::Json(json!({"ok": false, "error": "key 不能为空"}));
    }
    match history::remove(&key) {
        Ok(list) => axum::Json(json!({"ok": true, "list": list})),
        Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_history_clear() -> axum::Json<serde_json::Value> {
    match history::clear() {
        Ok(_) => axum::Json(json!({"ok": true})),
        Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ===== 脚本管理 API（口令保护）=====

fn token_ok(token: Option<&String>) -> bool {
    match std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.is_empty()) {
        Some(expect) => token.map(|t| t == &expect).unwrap_or(false),
        None => true,
    }
}

fn check_auth(q: &BTreeMap<String, String>) -> Result<(), axum::Json<serde_json::Value>> {
    if !token_ok(q.get("token")) {
        Err(axum::Json(json!({"ok": false, "error": "口令错误"})))
    } else {
        Ok(())
    }
}

async fn api_spiders(Query(q): Query<BTreeMap<String, String>>) -> axum::Json<serde_json::Value> {
    if let Err(e) = check_auth(&q) {
        return e;
    }
    axum::Json(json!({"ok": true, "list": registry::list()}))
}

async fn api_spiders_upload(
    Query(q): Query<BTreeMap<String, String>>,
    mut multipart: Multipart,
) -> axum::Json<serde_json::Value> {
    if let Err(e) = check_auth(&q) {
        return e;
    }
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.file_name().unwrap_or("script.js").to_string();
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return axum::Json(json!({"ok": false, "error": format!("读取文件失败: {e}")})),
        };
        let source = String::from_utf8_lossy(&bytes);
        return match registry::add_upload(&name, &source) {
            Ok(meta) => axum::Json(json!({"ok": true, "spider": meta})),
            Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
        };
    }
    axum::Json(json!({"ok": false, "error": "未收到文件"}))
}

async fn api_spiders_paste(
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let token = body.get("token").and_then(|v| v.as_str()).map(|s| s.to_string());
    if !token_ok(token.as_ref()) {
        return axum::Json(json!({"ok": false, "error": "口令错误"}));
    }
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("pasted.js");
    let source = body.get("source").and_then(|v| v.as_str()).unwrap_or("");
    if source.trim().is_empty() {
        return axum::Json(json!({"ok": false, "error": "源码不能为空"}));
    }
    match registry::add_upload(name, source) {
        Ok(meta) => axum::Json(json!({"ok": true, "spider": meta})),
        Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_spiders_url(Query(q): Query<BTreeMap<String, String>>) -> axum::Json<serde_json::Value> {
    if let Err(e) = check_auth(&q) {
        return e;
    }
    let url = q.get("url").cloned().unwrap_or_default();
    if url.is_empty() {
        return axum::Json(json!({"ok": false, "error": "url 不能为空"}));
    }
    match registry::add_from_url(&url).await {
        Ok(meta) => axum::Json(json!({"ok": true, "spider": meta})),
        Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn api_spiders_action(
    axum::extract::Path((id, action)): axum::extract::Path<(String, String)>,
    Query(q): Query<BTreeMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    if let Err(e) = check_auth(&q) {
        return e;
    }
    match action.as_str() {
        "toggle" => {
            let enabled = q.get("enabled").map(|s| s == "1" || s == "true").unwrap_or(true);
            match registry::toggle(&id, enabled) {
                Ok(m) => axum::Json(json!({"ok": true, "spider": m})),
                Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
            }
        }
        "remove" => match registry::remove(&id) {
            Ok(_) => axum::Json(json!({"ok": true})),
            Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
        },
        "source" => match registry::read_source(&id) {
            Ok(s) => axum::Json(json!({"ok": true, "source": s})),
            Err(e) => axum::Json(json!({"ok": false, "error": e.to_string()})),
        },
        "test" => {
            let kw = q.get("kw").cloned().unwrap_or_else(|| "测试".into());
            let params = json!({"keyword": kw, "wd": kw, "page": 1});
            match vod::dispatch(&id, "search", params).await {
                Ok(v) => {
                    registry::mark_result(&id, true, None);
                    axum::Json(json!({"ok": true, "result": v}))
                }
                Err(e) => {
                    registry::mark_result(&id, false, Some(e.to_string()));
                    axum::Json(json!({"ok": false, "error": e.to_string()}))
                }
            }
        }
        _ => axum::Json(json!({"ok": false, "error": "未知操作"})),
    }
}

// ===== 登录 / 登出（-auth 启用鉴权时生效）=====

async fn api_login(
    axum::extract::State(auth): axum::extract::State<Arc<auth::AuthState>>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    let json_err = |locked: Option<u64>, error: &str| {
        let body = match locked {
            Some(s) => json!({"ok": false, "locked": s, "error": error}),
            None => json!({"ok": false, "error": error}),
        };
        axum::response::IntoResponse::into_response(axum::Json(body))
    };
    if !auth.enabled {
        return json_err(None, "鉴权未启用");
    }
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if password.is_empty() {
        return json_err(None, "请输入密码");
    }
    match auth.try_login(addr.ip(), password) {
        Ok(token) => {
            let mut resp = axum::response::IntoResponse::into_response(axum::Json(
                json!({"ok": true}),
            ));
            resp.headers_mut().insert(
                axum::http::header::SET_COOKIE,
                axum::http::HeaderValue::from_str(&auth::session_cookie(&token)).unwrap(),
            );
            resp
        }
        Err(secs) if secs > 0 => json_err(
            Some(secs),
            "尝试次数过多，请稍后再试",
        ),
        Err(_) => json_err(None, "密码错误"),
    }
}

async fn api_logout(
    axum::extract::State(auth): axum::extract::State<Arc<auth::AuthState>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let (parts, _) = req.into_parts();
    if let Some(token) = auth::extract_token(&parts) {
        auth.drop_session(&token);
    }
    let mut resp = axum::response::IntoResponse::into_response(axum::Json(json!({"ok": true})));
    resp.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&auth::clear_cookie()).unwrap(),
    );
    resp
}

/// 解析命令行参数，返回 (端口, 数据目录, 访问密码)。
/// 用法：omnibox [-p 端口] [-d 数据目录] [-auth 密码]
/// 命令行优先级高于 PORT / DATA_DIR / AUTH 环境变量。
fn parse_args() -> (u16, String, Option<String>) {
    let mut port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3080);
    let mut data_dir: String = std::env::var("DATA_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "data".to_string());
    let mut auth_password: Option<String> = std::env::var("AUTH")
        .ok()
        .filter(|p| !p.is_empty());

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-h" || a == "--help" {
            println!("OmniBox — 豆瓣推荐 + 动态影视源（内嵌 QuickJS）\n\
                用法: omnibox [选项]\n\
                  -p, --port <端口>       监听端口（默认 3080）\n\
                  -d, --data-dir <目录>   数据目录，存放脚本/播放历史（默认 ./data）\n\
                  -auth, --auth <密码>    启用访问鉴权，未登录无法访问（默认不启用）\n\
                  -h, --help              显示帮助\n\
                示例: omnibox -p 5050 -d /path/to/data -auth mypassword");
            std::process::exit(0);
        } else if a == "-p" || a == "--port" {
            if let Some(v) = args.get(i + 1) {
                match v.parse::<u16>() {
                    Ok(n) => port = n,
                    Err(_) => eprintln!("警告: 无效端口 {v}，使用 {port}"),
                }
                i += 1;
            }
        } else if let Some(v) = a.strip_prefix("-p=").or_else(|| a.strip_prefix("--port=")) {
            match v.parse::<u16>() {
                Ok(n) => port = n,
                Err(_) => eprintln!("警告: 无效端口 {v}，使用 {port}"),
            }
        } else if a == "-d" || a == "--data-dir" {
            if let Some(v) = args.get(i + 1) {
                data_dir = v.clone();
                i += 1;
            }
        } else if let Some(v) = a.strip_prefix("-d=").or_else(|| a.strip_prefix("--data-dir=")) {
            data_dir = v.to_string();
        } else if a == "-auth" || a == "--auth" {
            if let Some(v) = args.get(i + 1).filter(|v| !v.starts_with('-')) {
                if v.is_empty() {
                    eprintln!("警告: 密码为空，鉴权未启用");
                } else {
                    auth_password = Some(v.clone());
                }
                i += 1;
            } else {
                eprintln!("警告: -auth 需要指定密码，鉴权未启用");
            }
        } else if let Some(v) = a.strip_prefix("-auth=").or_else(|| a.strip_prefix("--auth=")) {
            if v.is_empty() {
                eprintln!("警告: 密码为空，鉴权未启用");
            } else {
                auth_password = Some(v.to_string());
            }
        } else {
            eprintln!("警告: 未知参数 {a}（用 -h 查看帮助）");
        }
        i += 1;
    }
    (port, data_dir, auth_password)
}

#[tokio::main]
async fn main() {
    let (port, data_dir, auth_password) = parse_args();
    // 数据目录（脚本/历史数据都在此）尽早设置，供 registry/history 读取
    std::env::set_var("DATA_DIR", &data_dir);

    let auth = Arc::new(auth::AuthState::new(auth_password.is_some(), auth_password.as_deref().unwrap_or("")));

    let app = Router::new()
        .route("/", get(index_page))
        .route("/search.html", get(search_page))
        .route("/play.html", get(play_page))
        .route("/spiders.html", get(spiders_page))
        .route("/history.html", get(history_page))
        .route("/login.html", get(login_page))
        .route("/app.css", get(css))
        .route("/manifest.json", get(manifest))
        .route("/sw.js", get(sw))
        .route("/hls.min.js", get(hls_js))
        .route("/icon.svg", get(icon_svg))
        .route("/icon-192.png", get(icon192))
        .route("/icon-512.png", get(icon512))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/favicon.ico", get(favicon_ico))
        .route("/api/health", get(health))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/history", get(api_history_list))
        .route("/api/history/add", get(api_history_add))
        .route("/api/history/remove", get(api_history_remove))
        .route("/api/history/clear", get(api_history_clear))
        .route("/api/douban/home", get(douban::home))
        .route("/api/douban/category", get(douban::category))
        .route("/api/vod/search", get(vod::search_handler))
        .route("/api/vod/detail", get(vod::detail_handler))
        .route("/api/vod/play", get(vod::play_handler))
        .route("/api/spiders", get(api_spiders))
        .route("/api/spiders/upload", post(api_spiders_upload))
        .route("/api/spiders/paste", post(api_spiders_paste))
        .route("/api/spiders/url", post(api_spiders_url))
        .route("/api/spiders/{id}/{action}", get(api_spiders_action))
        .route("/api/img", get(proxy::img))
        .route("/api/stream", get(proxy::stream))
        .layer(axum::middleware::from_fn_with_state(auth.clone(), auth::middleware))
        .with_state(auth);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("端口绑定失败");

    println!("OmniBox 已启动: http://0.0.0.0:{port}");
    println!("  数据目录: {data_dir}");
    if auth_password.is_some() {
        println!("  访问鉴权: 已启用（未登录将被跳转到 /login.html）");
    } else {
        println!("  访问鉴权: 未启用（用 -auth <密码> 开启）");
    }
    println!("  首页(豆瓣): /   搜索: /search.html   脚本管理: /spiders.html   播放历史: /history.html");
    let _ = registry::debug_dir();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("服务运行失败");
}
