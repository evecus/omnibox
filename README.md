# OmniBox — 豆瓣推荐 + QuickJS 动态影视源

豆瓣做首页推荐与分类筛选，影视源全部由用户导入的 **OmniBox 格式脚本**提供，统一由 **Rust 内嵌 QuickJS** 执行：单二进制、零外部依赖（无需 Node.js / npm），每次调用独立隔离。

> 本项目不内置任何影视爬虫脚本。脚本由用户自行上传、粘贴源码或 URL 拉取，保存在本机 `data/spiders/`。请遵守当地法律法规，仅用于学习研究。

## 脚本兼容层

QuickJS 沙箱内提供以下环境，绝大多数采集脚本无需改动即可运行：

| 模块 | 说明 |
|---|---|
| `omnibox_sdk` / `spider_runner` | OmniBox SDK（request 桥接回 Rust，安全边界见下） |
| `axios` | get/post/put/head/create |
| `http` / `https` | Node 风格 `get`/`request`（回调 + `req.write/end/on` + `res.on('data'/'end')`），底层走 Rust `reqwest` |
| `cheerio` | **真 cheerio npm 包**（构建时 esbuild 打包编进二进制），HTML 解析语义与 Node 下完全一致 |
| `crypto-js` | **真 crypto-js npm 包**（同上编进二进制） |
| `crypto` | Rust 原生实现：`createHash` md5/sha1/sha256/sha512、`createCipheriv`/`createDecipheriv` AES-128/192/256 CBC/ECB、`publicEncrypt`/`privateDecrypt` RSA（PKCS1/OAEP/无填充）、`createSign` RSA-SHA1/SHA256/SHA512、`randomBytes`/`randomFillSync` |
| `Buffer` | utf8/latin1/base64/hex，含字节级操作与 `Buffer.concat` |
| `URL` / `URLSearchParams` / `url` / `querystring` / `util` | 常用方法 |
| `fs` | 沙箱键值持久化（路径只作键，内容存数据目录 `.fs/`，单文件 ≤1MB，跨调用保留，如登录 token） |
| `setTimeout` / `clearTimeout` | 由 Rust 侧定时泵驱动 |

`cheerio` / `crypto-js` 仅在脚本源码出现对应 `require` 时才求值（按需加载）。不支持 `vm`、`child_process`、`opencc`（个别脚本需要时可用 Node 单独跑）。

## 架构

```
浏览器  →  axum(Rust)
            ├─ douban.rs   豆瓣首页/分类
            ├─ proxy.rs    图片/视频流代理
            ├─ registry.rs 脚本注册/存储
            ├─ vod.rs      调度 + 结果归一化
            ├─ spider.rs   QuickJS + OmniBox SDK 兼容层
            └─ jscrypto.rs 加密原语（AES/RSA/哈希/随机）
libs/  构建时依赖：esbuild 打包 cheerio/crypto-js → 编进二进制
```

脚本网络请求统一经 Rust `reqwest` 发出：仅 http/https、拦内网地址、响应体限 5MB；非 UTF-8 响应附带 base64（供二进制场景）。每次脚本调用创建独立 QuickJS Runtime（64MB 内存上限、JS 执行 20s 中断、整体 30s 上限）。

## 快速开始

```bash
cargo build --release
./target/release/omnibox -p 5050 -d /path
```

命令行参数（`-h` 查看帮助，命令行优先于同名环境变量）：

| 参数 | 说明 | 默认 |
|---|---|---|
| `-p, --port <端口>` | 监听端口 | `3080` |
| `-d, --data-dir <目录>` | 数据目录：脚本、播放历史都放这里 | `./data` |

例：`./target/release/omnibox -p 5050 -d /opt/omnibox-data`，浏览器打开 `http://127.0.0.1:5050`。不传参数则用默认端口 3080、默认目录 `./data`。

1. 打开 `/spiders.html`，三种方式之一导入：
   - **上传文件**：选择本地 `.js`；
   - **粘贴源码**：把 `.js` 内容整段粘贴（手机端最稳）；
   - **URL 拉取**：填公网 raw 地址（如 GitHub raw）。
2. 点「测试」验证。
3. 回到首页 `/`（豆瓣推荐），或 `/search.html?q=片名` 搜索；手机可「添加到主屏幕」安装为 PWA。
4. 播放过的影片自动记录在 `/history.html`（各页面 header 的「历史」入口可进），点击可继续播放，支持删除单条或清空；历史存于数据目录 `spiders/history.json`。

### 重新打包内嵌库

仅修改内嵌库版本时需要（需 Node.js 18+）：

```bash
cd libs && npm install && npm run build && cargo build --release
```

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

## 环境变量

| 变量 | 说明 |
|---|---|
| `PORT` | 监听端口（兼容兜底，推荐用 `-p`） |
| `ADMIN_TOKEN` | 脚本管理接口口令（建议局域网使用时设置） |
| `DATA_DIR` | 数据目录（兼容兜底，推荐用 `-d`）；脚本/历史存于其下 `spiders/` |
| `SITE_API` | 传给脚本 `process.env.SITE_API`（通用采集站模板用） |
| `DANMU_API` | 传给脚本 `process.env.DANMU_API`（弹幕匹配用） |

## HTTP API

| 接口 | 说明 |
|---|---|
| `GET /api/health` | 健康检查（`engines.quickjs`） |
| `GET /api/douban/home` | 豆瓣首页（分类/筛选/Banner） |
| `GET /api/douban/category?cat=&page=&filters=` | 豆瓣分类 |
| `GET /api/vod/search?src=all\|<id>&wd=&page=` | 聚合搜索（all=所有启用脚本） |
| `GET /api/vod/detail?src=<id>&id=` | 详情（剧集已归一化） |
| `GET /api/vod/play?src=<id>&q=` | 播放解析 |
| `GET /api/spiders` | 脚本列表（需口令） |
| `POST /api/spiders/upload` | multipart 上传脚本（需口令） |
| `POST /api/spiders/paste` | JSON `{name, source}` 粘贴导入（需口令） |
| `POST /api/spiders/url?url=` | URL 拉取脚本（需口令） |
| `GET /api/spiders/:id/:action` | toggle/remove/source/test（需口令） |
| `GET /api/img?url=` | 图片代理（豆瓣 Referer） |
| `GET /api/stream?url=&ua=&referer=&origin=` | 视频流代理（Range + m3u8 重写 + header 透传） |

## 安全

- 口令保护：设置 `ADMIN_TOKEN` 后，所有 `/api/spiders*` 需带 `?token=`。
- QuickJS 沙箱：每次调用独立 Runtime、64MB 内存上限、20s 中断超时；`fs` 只是数据目录下的键值存储，无法访问真实文件系统；无 `child_process`/`vm`。
- 网络限制：脚本网络请求统一经 Rust 桥接，拦截内网/回环地址，防 SSRF。
- 建议仅在本机或可信局域网运行，勿直接暴露到公网。

## 已知限制

- 个别脚本依赖 `opencc` 简繁转换、`child_process`（Cloudflare 过盾类）等 QuickJS 无法提供的能力时不可运行（如 PPnix、FKTV），请改用支持这些能力的环境。
- 加密源的上游接口若有地域/IP 风控（部分源对云主机出口 IP 返回 403），需在实际网络环境验证，与运行时能力无关。
- OmniBox 专有刮削（TMDB）、弹幕、嗅探能力未实现，stub 为空。
- `parse=1` 的播放地址按直连尝试；需第三方解析接口的源可能无法播放。
- 未导入任何脚本时，搜索页会提示前往脚本管理页。
