# FilmBox — 豆瓣推荐 + QuickJS 动态影视源

豆瓣做首页推荐与分类筛选，**影视源全部由 QuickJS 运行用户导入的 OmniBox 格式脚本**提供。单二进制、零 Node.js / Python 依赖，内置网页面板。

> 本项目不内置任何影视爬虫脚本。脚本需用户自行上传或输入 URL 拉取，保存在本机 `data/spiders/`。请遵守当地法律法规，仅用于学习研究。

## 架构

```
浏览器  →  axum(Rust)  →  QuickJS 运行时（每次调用独立隔离）
            ├─ douban.rs   豆瓣首页/分类（保留）
            ├─ proxy.rs    图片/视频流代理（保留）
            ├─ registry.rs 脚本注册与存储
            ├─ spider.rs   QuickJS + OmniBox SDK 兼容层
            └─ vod.rs      调度 + 结果归一化
```

- **不内置影视源**：瓜子/如意/热播/荐片的 Rust 实现已全部移除。
- **脚本来源**：网页管理页上传 `.js` 或输入公网 URL 拉取。
- **SDK 兼容层**：`OmniBox.request/log/cache`、`axios`、`Buffer(base64url)`、`process.env` 等已实现；刮削/嗅探/弹幕等 OmniBox 专有能力 stub 为空（不影响搜索/详情/播放）。

## 快速开始

```bash
cargo build --release
PORT=8021 ./target/release/filmbox
```

浏览器打开 `http://127.0.0.1:8021`

1. 打开 `/spiders.html` 导入脚本（上传或 URL 拉取）。
2. 回到首页 `/`（豆瓣推荐），或 `/search.html?q=片名` 搜索。
3. 手机浏览器可「添加到主屏幕」安装为 PWA。

## 脚本格式

OmniBox 采集脚本，需导出以下方法（缺的方法视为不支持）：

```js
const OmniBox = require("omnibox_sdk");
module.exports = {
  async home(params) {},       // { class, filters, list }
  async category(params) {},   // { page, pagecount, list }
  async search(params) {},     // { list:[{vod_id, vod_name, vod_pic, vod_remarks}], pagecount }
  async detail(params) {},     // { list:[{ vod_play_sources:[{name, episodes:[{name, playId}]}] }] }
  async play(params) {},       // { urls:[{name, url}], header, parse }
};
```

- `search` 入参：`{ keyword, wd, page }`
- `detail` 入参：`{ videoId, id }`
- `play` 入参：`{ playId, flag }`（`flag` 为脚本 id）

兼容层提供的 require 模块：`omnibox_sdk`、`spider_runner`、`axios`；全局 `Buffer`、`process.env`、`console`。`cheerio` 等第一版不支持，请优先使用纯 JSON/API 类脚本。

## 环境变量

| 变量 | 说明 |
|---|---|
| `PORT` | 监听端口，默认 8021 |
| `ADMIN_TOKEN` | 脚本管理接口口令（建议局域网使用时设置） |
| `DATA_DIR` | 数据目录，默认 `./data`（脚本存于其下 `spiders/`） |
| `SITE_API` | 传给脚本 `process.env.SITE_API`（通用采集站模板用） |

## HTTP API

| 接口 | 说明 |
|---|---|
| `GET /api/health` | 健康检查 |
| `GET /api/douban/home` | 豆瓣首页（分类/筛选/Banner） |
| `GET /api/douban/category?cat=&page=&filters=` | 豆瓣分类 |
| `GET /api/vod/search?src=all\|<id>&wd=&page=` | 聚合搜索（all=所有启用脚本） |
| `GET /api/vod/detail?src=<id>&id=` | 详情（剧集已归一化） |
| `GET /api/vod/play?src=<id>&q=` | 播放解析 |
| `GET /api/spiders` | 脚本列表（需口令） |
| `POST /api/spiders/upload` | multipart 上传脚本（需口令） |
| `POST /api/spiders/url?url=` | URL 拉取脚本（需口令） |
| `GET /api/spiders/:id/:action` | toggle/remove/source/test（需口令） |
| `GET /api/img?url=` | 图片代理（豆瓣 Referer） |
| `GET /api/stream?url=&ua=&referer=` | 视频流代理（Range + m3u8 重写） |

## 安全

- 口令保护：设置 `ADMIN_TOKEN` 后，所有 `/api/spiders*` 需带 `?token=`。
- 脚本沙箱：每次调用独立 Runtime、64MB 内存上限、20s 中断超时、禁 `fs/child_process/eval`。
- 网络限制：脚本 `OmniBox.request` 仅允许 http/https，拦截内网/回环地址，防 SSRF。
- 建议仅在本机或可信局域网运行，勿直接暴露到公网。

## 已知限制

- 不支持 `cheerio` 等 Node HTML 解析库；纯 HTML 抓取类脚本需改用 JSON/API 类源。
- OmniBox 专有的刮削（TMDB）、弹幕、嗅探能力未实现，stub 为空。
- `parse=1` 的播放地址第一版按直连尝试；需第三方解析接口的源可能无法播放。
- 未导入任何脚本时，搜索页会提示前往脚本管理页。
