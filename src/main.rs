// main.rs —— FilmBox 入口：豆瓣首页 + QuickJS 动态脚本源 + 网页面板

mod cache;
mod douban;
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

const INDEX_HTML: &str = include_str!("../static/index.html");
const SEARCH_HTML: &str = include_str!("../static/search.html");
const PLAY_HTML: &str = include_str!("../static/play.html");
const SPIDERS_HTML: &str = include_str!("../static/spiders.html");
const APP_CSS: &str = include_str!("../static/app.css");
const MANIFEST: &str = include_str!("../static/manifest.json");
const SW_JS: &str = include_str!("../static/sw.js");

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
async fn css() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}
async fn manifest() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/manifest+json; charset=utf-8")], MANIFEST)
}
async fn sw() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/javascript; charset=utf-8")], SW_JS)
}
async fn icon192() -> impl IntoResponse {
    ([("content-type", "image/png")], include_bytes!("../static/icon-192.png").as_slice())
}
async fn icon512() -> impl IntoResponse {
    ([("content-type", "image/png")], include_bytes!("../static/icon-512.png").as_slice())
}
async fn health() -> impl IntoResponse {
    axum::Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "engine": "quickjs"}))
}

// ===== 脚本管理 API（口令保护）=====

fn check_auth(q: &BTreeMap<String, String>) -> Result<(), axum::Json<serde_json::Value>> {
    if let Some(token) = std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.is_empty()) {
        match q.get("token") {
            Some(t) if t == &token => Ok(()),
            _ => Err(axum::Json(json!({"ok": false, "error": "口令错误"}))),
        }
    } else {
        // 未设置 ADMIN_TOKEN 时允许本地使用
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
            let source = match registry::read_source(&id) {
                Ok(s) => s,
                Err(e) => return axum::Json(json!({"ok": false, "error": e.to_string()})),
            };
            let kw = q.get("kw").cloned().unwrap_or_else(|| "测试".into());
            let params = json!({"keyword": kw, "wd": kw, "page": 1});
            match spider::call_script_source(&source, "search", params).await {
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

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index_page))
        .route("/search.html", get(search_page))
        .route("/play.html", get(play_page))
        .route("/spiders.html", get(spiders_page))
        .route("/app.css", get(css))
        .route("/manifest.json", get(manifest))
        .route("/sw.js", get(sw))
        .route("/icon-192.png", get(icon192))
        .route("/icon-512.png", get(icon512))
        .route("/api/health", get(health))
        .route("/api/douban/home", get(douban::home))
        .route("/api/douban/category", get(douban::category))
        .route("/api/vod/search", get(vod::search_handler))
        .route("/api/vod/detail", get(vod::detail_handler))
        .route("/api/vod/play", get(vod::play_handler))
        .route("/api/spiders", get(api_spiders))
        .route("/api/spiders/upload", post(api_spiders_upload))
        .route("/api/spiders/url", post(api_spiders_url))
        .route("/api/spiders/{id}/{action}", get(api_spiders_action))
        .route("/api/img", get(proxy::img))
        .route("/api/stream", get(proxy::stream));

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8021);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("端口绑定失败");

    println!("FilmBox 已启动: http://0.0.0.0:{port}");
    println!("  首页(豆瓣): /   搜索(脚本源): /search.html   脚本管理: /spiders.html");
    let _ = registry::debug_dir();
    axum::serve(listener, app).await.expect("服务运行失败");
}
