# OmniBox — 豆瓣推荐 + 动态影视源

豆瓣做首页推荐与分类筛选，影视源全部由用户导入的 **OmniBox 格式脚本**提供。脚本统一由 Rust 内嵌的 **QuickJS** 运行：单二进制、零外部依赖（**不需要安装 Node.js**）、每次调用独立隔离。常见 Node 能力（`crypto` / `crypto-js` / `cheerio` / `axios` / `http(s)` / `URL` / `vm` 等）均已用 Rust + JS 兼容层实现。

> 本项目不内置任何影视爬虫脚本。脚本由用户自行上传、粘贴源码或 URL 拉取，保存在本机 `data/spiders/`。请遵守当地法律法规，仅用于学习研究。

## 脚本兼容性

在 `/spiders.html` 导入脚本（上传 / 粘贴 / URL 拉取）即可使用，无需选择引擎。

| 脚本类型 | 支持情况 | 说明 |
|---|---|---|
| 纯 JSON/API 采集（MacCMS 站等） | ✅ | |
| `crypto` AES/RSA/HMAC/签名（如瓜子） | ✅ | Rust 原生实现 |
| `crypto-js` | ✅ | 内置 shim，已与真实 crypto-js 对拍 |
| `cheerio` HTML 解析 | ✅ 只读 API | Rust(scraper) + JS 兼容层，含 `:contains/:eq/:first/:last/:gt/:lt/:even/:odd`；`attr(k,v)/append/html(str)` 等修改类 API 未实现 |
| `WebAssembly`（如布布影视、3Q影视） | ❌ | QuickJS 无 WebAssembly |
| `child_process` / `WebSocket`（如枝枝影视、PPnix，需本机 Chromium/curl） | ❌ | 沙箱内不可用 |

> 升级说明：旧版本的 `registry.json` 中的 `runtime` 字段会被忽略；原先设为 Node 的脚本现在也由 QuickJS 运行（上表 ❌ 类脚本将不可用）。

## 架构

```
浏览器  →  axum(Rust)
            ├─ douban.rs   豆瓣首页/分类（保留）
            ├─ proxy.rs    图片/视频流代理（保留）
            ├─ registry.rs 脚本注册/存储
            ├─ vod.rs      调用脚本 + 结果归一化
            └─ spider.rs   QuickJS + OmniBox SDK / Node 兼容层
```

脚本内的网络请求统一走 `OmniBox.request` 安全边界（仅 http/https、拦内网（含重定向每一跳）、响应体限 5MB）。

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
| `-auth, --auth <密码>` | 启用访问鉴权，未登录无法访问任何页面/接口 | 不启用 |

例：`./target/release/omnibox -p 5050 -d /opt/omnibox-data -auth mypassword`，浏览器打开 `http://127.0.0.1:5050`。不传参数则用默认端口 3080、默认目录 `./data`、不启用鉴权。

### 访问鉴权与防爆破

加 `-auth <密码>` 后所有页面和 API 都需要登录（`/login.html` 登录页与登录接口、静态图标资源除外）：

- 会话：登录成功种 `omni_session` Cookie（HttpOnly + SameSite=Lax），内存存储、7 天有效，重启后需重新登录。
- 防爆破：同一 IP 10 分钟内连续错 5 次即锁定 30 秒，之后每多错一次锁定时间翻倍，封顶 15 分钟；登录成功即清零。
- 未登录访问页面会 302 跳转 `/login.html`（登录后自动回跳），访问 `/api/*` 返回 401 JSON。
- 也可用环境变量 `AUTH` 设置密码（命令行 `-auth` 优先）。

1. 打开 `/spiders.html`，先选引擎，再三种方式之一导入：
   - **上传文件**：选择本地 `.js`；
   - **粘贴源码**：把 `.js` 内容整段粘贴（手机端最稳）；
   - **URL 拉取**：填公网 raw 地址（如 GitHub raw）。
2. 点「测试」验证脚本是否可用；用到 WebAssembly / child_process / WebSocket 的源在 QuickJS 下不可用。
3. 无需安装任何依赖，crypto-js / cheerio / axios 等已内置。
4. 回到首页 `/`（豆瓣推荐），或 `/search.html?q=片名` 搜索；手机可「添加到主屏幕」安装为 PWA。
5. 播放过的影片自动记录在 `/history.html`（各页面 header 的「历史」入口可进），点击可继续播放，支持删除单条或清空；历史存于数据目录 `spiders/history.json`。

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

**QuickJS 兼容层**提供（实现见 `static/node-compat.js`、`static/cheerio-shim.js`、`static/crypto-js-shim.js`、`src/jscrypto.rs`、`src/jsdom.rs`）：
- `omnibox_sdk`、`spider_runner`（支持 `runner.run({...})` 导出写法）、`process`/`console`、真实定时器 `setTimeout/setInterval`、`fetch`、`TextEncoder/Decoder`、`atob/btoa`。
- `crypto`：`createHash`（md5/sha1/sha256/sha512）、`createHmac`、`createCipheriv/createDecipheriv`（AES-128/192/256，CBC/ECB）、`publicEncrypt/privateDecrypt`（RSA PKCS1/OAEP/无填充）、`createSign/createVerify`（RSA-SHA1/256/512）、`randomBytes/randomUUID/randomInt/randomFillSync`，均为 Rust 原生实现。
- `crypto-js`、`cheerio`（首次 `require` 时惰性加载）。
- `axios`（get/post/put/patch/delete/head/create、拦截器、baseURL、对象/表单/二进制请求体、`responseType: 'arraybuffer'`、`maxRedirects`、默认 2xx 校验，与真实 axios 行为一致）、`http`/`https`（`request`/`get`，含 write/end/timeout 事件与二进制响应）。
- `Buffer`（字节级）、`URL`/`URLSearchParams`（WHATWG 近似实现，非法 URL 抛 TypeError）、`url`、`querystring`、`util`（promisify 等）、`events`、`vm`（近似实现：在全局作用域求值并回写 sandbox，非真隔离）、`path`/`os`。
- `fs`：沙箱键值持久化（路径只作键，内容存于数据目录 `.fs/`，单文件 ≤1MB，跨调用保留，如登录 token），无法访问真实文件系统。
- `child_process`：调用时给出明确错误（ENOSYS）。
- 不支持：`WebAssembly`、`WebSocket`、`Intl`、`AbortController`、IDN（中文域名）punycode。


## 环境变量

| 变量 | 说明 |
|---|---|
| `PORT` | 监听端口（兼容兜底，推荐用 `-p`） |
| `AUTH` | 访问鉴权密码（兼容兜底，推荐用 `-auth`）；不设置则不启用鉴权 |
| `ADMIN_TOKEN` | 脚本管理接口口令（建议局域网使用时设置） |
| `DATA_DIR` | 数据目录（兼容兜底，推荐用 `-d`）；脚本/历史存于其下 `spiders/` |
| `SITE_API` | 传给脚本 `process.env.SITE_API`（通用采集站模板用） |
| `DANMU_API` | 传给脚本 `process.env.DANMU_API`（弹幕匹配用） |

## HTTP API

| 接口 | 说明 |
|---|---|
| `GET /api/health` | 健康检查（含 `engines.quickjs`） |
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
- QuickJS 沙箱：每次调用独立 Runtime、64MB 内存上限、20s 中断超时、禁 `child_process`；`fs` 只是数据目录下的键值存储，无法访问真实文件系统。
- 网络限制：拦截内网/回环地址，防 SSRF。
- 建议仅在本机或可信局域网运行，勿直接暴露到公网。

## 已知限制

- 加密源的上游接口若有地域/IP 风控（部分源对云主机出口 IP 返回 403），需在实际网络环境验证，与运行时能力无关。
- QuickJS 下的 `cheerio` 兼容层只实现只读 API（`load/find/children/parent(s)/closest/siblings/next/prev/eq/first/last/filter/not/has/is/map/each/text/html/attr/data/val/clone/remove` 等），写入类 API（`attr(k,v)`、`append`、`html(str)`、`addClass` 等）会抛出明确错误。这类脚本在 QuickJS 下不可用。
- 依赖 `opencc-js`/`chinese-conv` 做简繁转换的脚本，在 QuickJS 下会降级为脚本自带的兜底映射表，通常不影响搜索/播放。
- OmniBox 专有刮削（TMDB）、弹幕、嗅探能力未实现，stub 为空。
- `parse=1` 的播放地址按直连尝试；需第三方解析接口的源可能无法播放。
- 未导入任何脚本时，搜索页会提示前往脚本管理页。
