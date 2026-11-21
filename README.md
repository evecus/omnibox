# OmniBox — 豆瓣推荐 + 双引擎动态影视源

豆瓣做首页推荐与分类筛选，影视源全部由用户导入的 **OmniBox 格式脚本**提供，支持两种运行时：

- **QuickJS**（默认）：Rust 内嵌、单二进制、零外部依赖、每次调用独立隔离，适合普通 JSON/API 采集源（如意、乌云等）。
- **Node.js**（可选增强）：常驻子进程执行，提供原生 `crypto`（AES/RSA/md5）、`https/http`、`Buffer`，以及 `crypto-js` 兼容 shim（AES/MD5/SHA1/Base64）、`cheerio`（HTML 解析，需装依赖）。用于 QuickJS 无法覆盖的加密协议源（如各类加密签名协议源）和 HTML 抓取类脚本。**需服务端安装 Node.js 18+**。

> 本项目不内置任何影视爬虫脚本。脚本由用户自行上传、粘贴源码或 URL 拉取，保存在本机 `data/spiders/`。请遵守当地法律法规，仅用于学习研究。

## 运行时选择

在 `/spiders.html` 导入时选择引擎，导入后也可在每张脚本卡片上用「切到 Node / 切到 QuickJS」单独切换。

| 脚本类型 | 推荐引擎 | 说明 |
|---|---|---|
| 纯 JSON/API 采集（如意资源、乌云影视、MacCMS 站） | QuickJS | 轻量、隔离好、无需 Node |
| `crypto` AES/RSA（各类签名协议源） | **Node.js** | QuickJS 兼容层只有 md5 stub，AES/RSA 无法运行 |
| `crypto-js`（修罗/柯南/金牌等 8 个） | **Node.js** | 内置 crypto-js shim，零依赖 |
| `cheerio` HTML 解析（4KMP 等 37 个） | **Node.js** | 首次使用时自动安装 cheerio（约 8MB），无需手动操作 |

> 判断依据：脚本 `require('crypto')` 做 AES/RSA、`require('crypto-js')`、`require('cheerio')` 时选 Node.js。`/api/health` 的 `engines.node` 字段显示 Node 是否可用。第三方包（cheerio/opencc 等）在脚本首次 `require` 时**自动按需安装**到 `data/spiders/node_modules`，日志会显示「检测到缺少依赖…正在自动安装…安装成功」，只装实际用到的包，装一次后续复用。

## 架构

```
浏览器  →  axum(Rust)
            ├─ douban.rs   豆瓣首页/分类（保留）
            ├─ proxy.rs    图片/视频流代理（保留）
            ├─ registry.rs 脚本注册/存储（含 runtime 字段）
            ├─ vod.rs      按 runtime 分流 + 结果归一化
            ├─ spider.rs   QuickJS + OmniBox SDK 兼容层
            └─ node.rs     Node 子进程运行时（stdio NDJSON 双向 RPC）
                            └─ static/runner.js（SDK shim，网络桥接回 Rust）
```

两条执行链路共用同一套 `OmniBox.request` 安全边界（仅 http/https、拦内网、响应体限 5MB）。Node 运行时为常驻单进程，脚本方法通过 `{t:"call", id, file, method, params}` 调度，脚本内的网络请求经 `{t:"http_req", id}` 桥接回 Rust 发出，再以 `{t:"http_resp", id}` 回传；进程崩溃/调用超时（30s）会自动重启。

## 快速开始

```bash
cargo build --release
./target/release/omnibox
```

默认监听 **3080** 端口，浏览器打开 `http://127.0.0.1:3080`（如需改端口：`PORT=其他端口 ./target/release/omnibox`）

1. 打开 `/spiders.html`，先选引擎，再三种方式之一导入：
   - **上传文件**：选择本地 `.js`；
   - **粘贴源码**：把 `.js` 内容整段粘贴（手机端最稳）；
   - **URL 拉取**：填公网 raw 地址（如 GitHub raw）。
2. 加密源（加密源、crypto-js 类）选 **Node.js**；普通源用默认 QuickJS。点「测试」验证。
3. 首次测试 cheerio 类脚本时会自动安装依赖（约 8MB，日志可见进度，需服务端能访问 npm）；`crypto-js` 已内置 shim，无需安装。若网络不通，也可离线提前准备 `data/spiders/node_modules`，或运行一次 `./install-deps.sh` 预装（可选）。无需重启。
4. 回到首页 `/`（豆瓣推荐），或 `/search.html?q=片名` 搜索；手机可「添加到主屏幕」安装为 PWA。

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

**QuickJS 兼容层**提供：`omnibox_sdk`、`spider_runner`、`axios`、`Buffer`（含字节级操作）、`URLSearchParams`、`URL`、`process.env`、`console`；`crypto.createHash('md5')`、`crypto.randomBytes`；`os`/`path`/`fs` 内存 stub。不支持 AES/RSA、`cheerio`。

**Node.js 运行时**：`omnibox_sdk`/`axios`/`spider_runner` 为 shim（网络桥接回 Rust，与 QuickJS 同一安全边界）；`crypto`/`https`/`http`/`buffer` 等为 Node 原生模块，支持完整 AES/RSA。刮削/嗅探/弹幕等 OmniBox 专有能力 stub 为空（不影响搜索/详情/播放）。

## 环境变量

| 变量 | 说明 |
|---|---|
| `PORT` | 监听端口，默认 3080 |
| `ADMIN_TOKEN` | 脚本管理接口口令（建议局域网使用时设置） |
| `DATA_DIR` | 数据目录，默认 `./data`（脚本存于其下 `spiders/`） |
| `SITE_API` | 传给脚本 `process.env.SITE_API`（通用采集站模板用） |
| `DANMU_API` | 传给脚本 `process.env.DANMU_API`（弹幕匹配用） |

## HTTP API

| 接口 | 说明 |
|---|---|
| `GET /api/health` | 健康检查（含 `engines.quickjs` / `engines.node` / `engines.node_version`） |
| `GET /api/douban/home` | 豆瓣首页（分类/筛选/Banner） |
| `GET /api/douban/category?cat=&page=&filters=` | 豆瓣分类 |
| `GET /api/vod/search?src=all\|<id>&wd=&page=` | 聚合搜索（all=所有启用脚本） |
| `GET /api/vod/detail?src=<id>&id=` | 详情（剧集已归一化） |
| `GET /api/vod/play?src=<id>&q=` | 播放解析 |
| `GET /api/spiders` | 脚本列表（需口令） |
| `POST /api/spiders/upload?runtime=` | multipart 上传脚本（需口令） |
| `POST /api/spiders/paste` | JSON `{name, source, runtime}` 粘贴导入（需口令） |
| `POST /api/spiders/url?url=&runtime=` | URL 拉取脚本（需口令） |
| `GET /api/spiders/:id/:action` | toggle/runtime/remove/source/test（需口令） |
| `GET /api/img?url=` | 图片代理（豆瓣 Referer） |
| `GET /api/stream?url=&ua=&referer=&origin=` | 视频流代理（Range + m3u8 重写 + header 透传） |

## 安全

- 口令保护：设置 `ADMIN_TOKEN` 后，所有 `/api/spiders*` 需带 `?token=`。
- QuickJS 沙箱：每次调用独立 Runtime、64MB 内存上限、20s 中断超时、禁 `fs/child_process`。
- Node 子进程：脚本内网络请求仍经 Rust 桥接，沿用 http/https + 内网拦截 + 5MB 响应限制；调用超时 30s 自动重启进程。
- 网络限制：两条链路都拦截内网/回环地址，防 SSRF。
- 建议仅在本机或可信局域网运行，勿直接暴露到公网。

## 已知限制

- Node.js 运行时需在服务端预装（18+）；未安装时加密源不可用，health 会显示 `engines.node=false`。
- 加密源的上游接口若有地域/IP 风控（部分源对云主机出口 IP 返回 403），需在实际网络环境验证，与运行时能力无关。
- `cheerio` 类 HTML 抓取脚本需 Node 运行时，依赖在脚本首次运行时自动安装；QuickJS 下不支持。自动安装只针对白名单包（cheerio/opencc 等），且需服务端可访问 npm registry；离线环境请用 `./install-deps.sh` 或手动放置 `node_modules`。
- 少数脚本依赖 `opencc-js`/`chinese-conv` 做简繁转换，同样按需自动安装；安装失败时相关功能降级，通常不影响搜索/播放。
- OmniBox 专有刮削（TMDB）、弹幕、嗅探能力未实现，stub 为空。
- `parse=1` 的播放地址按直连尝试；需第三方解析接口的源可能无法播放。
- 未导入任何脚本时，搜索页会提示前往脚本管理页。
