# CatPaw

**专为 AI agent 设计的 headless-first 浏览器，用 Rust 从零实现。**

CatPaw 不是 Chromium 的封装，也不是"渲染引擎 + 自动化接口"。它是一个把 LLM agent
当作第一用户的浏览器：忠实执行 JavaScript 与 DOM，只在有人观察时才计算布局，并直接提供
agent 真正需要的东西——带稳定引用的紧凑语义快照、精确的"页面已稳定"信号、用 diff 代替
重复输出、确定性的时间、廉价的隔离上下文。

> 状态：**pre-alpha**。里程碑 M0（"抓取与阅读"）与 M1（"脚本运行"，基于 Boa 的 JavaScript）已完成，M2（"交互"）进行中。
>
> M2 当前进展：布局。块级、flex、grid 盒由 Taffy 排布，行内内容由 Parley 在内置字体集上整形断行，且只在有人索取几何信息时才计算；CSSOM View（`getBoundingClientRect`、`getClientRects`、`offset*`/`client*`/`scroll*`、`scrollTo`、`scrollIntoView`、`elementFromPoint`）以及 `IntersectionObserver`、`ResizeObserver` 都基于它作答。截图（`catpaw fetch --js --screenshot out.png [--full-page]`）用 tiny-skia 绘制背景、边框与文字。输入：受信的指针与键盘事件序列，含焦点、打字、激活行为（链接、按钮、label、`details`）、各种编码的表单提交及其引发的导航，由 `--action "click <selector>"`、`fill`、`type`、`press`、`check`、`select` 驱动。框架：每个 `iframe` 都是同一线程上独立的页面（自己的文档、脚本与事件循环），尺寸取自其元素；框架之间只能通过 `postMessage`、`parent`/`top`/`contentWindow` 与 `load` 事件相见，如同跨源框架；`--action "frame <selector>"` 把后续动作与 `--eval` 指向该框架（`frame top`、`frame parent` 返回）。Worker：dedicated worker（`new Worker`，支持同源、`blob:` 与 `data:` 脚本；双向 `postMessage`、`importScripts`、`close`、`terminate`，错误转发给所有者）作为独立 realm 在页面线程上与页面及框架轮流运行。有了框架与 Worker，Cloudflare Turnstile 组件能加载挑战框架并以测试 site key 完成验证，页面回调收到 token。通道与套接字：页面内的 `MessageChannel`、`MessagePort`、`BroadcastChannel`；`WebSocket` 复用 HTTP 的传输层（代理、TLS、cookie 与私网策略一并生效），文本与二进制双向收发，带关闭码与原因；只剩一个打开的 socket 在等的页面，静默一秒后即视为已稳定。网络：响应体在线上和解码后都有上限（`--max-response-mb`），回环与私网地址默认拒绝（`--allow-private-network` 放行），支持 HTTP `CONNECT` 与 SOCKS5 代理（`--proxy`），cookie 文件跨运行保留（`--cookie-jar`）。
>
> 通过 `catpaw fetch --js` 使用：经典脚本（内联、外链、`defer`、`async`、
> 脚本动态插入、`document.write`）与模块脚本（静态与动态 import、import map）在 Boa 上与解析器交错执行；核心 DOM（全部 HTML/SVG 元素接口、属性对象、树遍历、XPath、`Range` 与 `Selection`、`DOMParser`、`document.implementation`、文档集合与具名访问如 `document.forms`、`document.myForm`）、
> Shadow DOM 与 slot、Custom Elements、事件、`MutationObserver`/`IntersectionObserver`/`PerformanceObserver`、定时器、`URL`、
> history、Navigation Timing、内联样式与计算样式（样式表会被抓取并由 Stylo 层叠）及 CSSOM（`CSSStyleSheet`、`adoptedStyleSheets`、`CSS.supports`）、
> `fetch`/`XMLHttpRequest`（CORS、预检、重定向与 referrer policy 均按 Fetch 标准由页面处理）、Streams、`data:` URL、`sendBeacon`、storage、编码、`crypto` 随机数与摘要、
> 无布局形态的字体加载 API，以及 console 等 API 的绑定由 Web IDL 生成；事件循环支持虚拟时间；墙钟脚本预算（`--script-budget`，默认 10 秒）会终止失控的脚本。
> React、Vue、Svelte、Lit、htmx、Alpine 站点均可运行；Boa 引擎以附带修复的形式 vendor 在 `vendor/` 下（见其中说明）。
> web-platform-tests 在 CI 中按记录的预期运行，由进程内的 WPT 服务器替身提供服务（含 fetch/XHR 测试依赖的 Python 处理器的等价实现）：`dom` 4246 个子测试通过 2978，`html/dom` 1066 通过 582，`fetch/api` 2237 通过 1908，`xhr` 1200 通过 859，`css/cssom-view` 1198 通过 478（其余大多需要 iframe、布局，或替身尚未模拟的服务器行为）。
> 尚未支持：截图中的图片、渐变与圆角，表格的网格布局、图片的固有尺寸、canvas、媒体、Worker、WebAssembly，以及 HTML 元素中超出属性反射的成员。
>
> 不依赖 JavaScript 即可用的部分：基于 rustls 的 HTTP/1.1 与 HTTP/2、重定向、cookie、
> gzip/brotli/zstd 解压、编码嗅探、Web Bot Auth 请求签名（已通过 Cloudflare 测试端点验证）、
> HTML 解析进 arena DOM（WPT tree-construction 1968 例通过 1858 例，其余为已记录的上游差距）、
> 由 Stylo 从 UA/外链/内联样式表解析出的 `display`/`visibility`、带稳定 ref 的 CST 快照，
> 以及 markdown/文本/链接/表单视图。

[English](README.md) · [架构](docs/architecture.md) · [决策记录](docs/adr/) ·
[完整设计（中文）](docs/design.zh-CN.md)

## 与众不同之处

- **agent 原生接口。** `snapshot` 返回 CST（CatPaw Snapshot Text）树——Playwright aria
  snapshot 的超集——ref 永不复用；每个动作都等待页面稳定并可直接返回 diff，一步 = 一次
  往返。内建 JSON-RPC over WebSocket 与 MCP server；CDP 子集之后提供以兼容 Puppeteer。
- **精确的 settled。** CatPaw 拥有事件循环，所以知道每一个在途请求、每一个 timer（及其
  来源行）、每一个动画帧和 microtask。超时会点名元凶，而不是静默失败。
- **按需布局。** 样式（Stylo）和布局（Taffy + Parley）只在脚本或 agent 索取几何信息或
  截图时才运行。不需要这些的 headless 页面几乎零开销。
- **诚实的身份。** CatPaw 表明自己的身份，在 fetch 层实现
  [Web Bot Auth](https://datatracker.ietf.org/wg/webbotauth/about/)（RFC 9421 HTTP
  Message Signatures）；当站点要求 agent 无法合法提供的东西时，交给人来完成。不提供指纹
  伪装 profile，不接打码服务——见 [ADR 0003](docs/adr/0003-identity-bot-auth-and-challenges.md)。
- **默认纯 Rust。** 默认 JavaScript 引擎是 Boa；V8 作为计划中的可选后端
  （[ADR 0001](docs/adr/0001-js-engine-boa-default.md)）。

## 快速开始

```sh
cargo install catpaw            # 已发布到 crates.io；源码目录下可用 `cargo run -p catpaw --` 代替
catpaw fetch https://example.com --snapshot
catpaw fetch https://news.ycombinator.com --markdown
catpaw fetch https://httpbin.org/forms/post --forms
# 源码目录下（尚未包含在已发布的 0.0.1 中）：先执行页面脚本再读取
cargo run -p catpaw -- fetch https://news.ycombinator.com --js --console
cargo run -p catpaw -- fetch https://example.com --js --eval "document.title"
# 通过表单登录，并把会话留给下一次运行
cargo run -p catpaw -- fetch https://site.example/login --js --cookie-jar ./jar.json \
    --action "fill #username bob" --action "fill #password secret" --action "press Enter" --text
catpaw keygen --out ./agent-key.json
```

库 crate 同样已发布：`catpaw-net`、`catpaw-fetch`、`catpaw-dom`、`catpaw-style`、`catpaw-agent`。

## 路线图

| 里程碑 | 范围 | 完成标准 |
|---|---|---|
| M0 抓取与阅读（已完成） | HTTP/1.1+2、cookies、Web Bot Auth 签名、HTML 解析进 arena DOM、Stylo UA + 作者样式表、CST 快照 v0、markdown/文本/表单视图、CLI | `catpaw fetch … --snapshot` 在真实页面可用；WPT tree-construction 套件在 CI 中按记录的期望运行 |
| M1 脚本运行（已完成） | Boa realm、生成的绑定、带虚拟时间的事件循环、解析器/脚本交错、fetch/XHR、脚本预算、进程内 WPT runner（取代原计划的 WebDriver 子集） | WPT `dom/`、`html/dom/`、`fetch/api/`、`xhr/` 子集按记录的预期通过；React 与 Vue 应用完成服务端标记的 hydration（测试套件中计时） |
| M2 交互 | 布局、命中测试、输入事件、表单、导航与历史、iframe 与弹窗、存储、observers、截图、Canvas 2D、Web Crypto、WebSocket、Workers | 在真实站点完成登录；Turnstile 勾选框点击生效 |
| M3 agent API | JSON-RPC/WS、MCP、快照 diff、settled、动作后果、checkpoint、HAR 录制回放、SDK | agent 通过 MCP 完成 WebArena 任务 |
| M4 保真与挑战 | 挑战检测、human hand-off、覆盖各种 Cloudflare 挑战模式的测试 zone、Signed Agent 注册 | 有通过率数据；hand-off 端到端可用 |
| M5 规模与兼容 | 多租户限额、OpenTelemetry、Docker、CDP 子集、V8 后端对等 | 单机 1000 个 context；puppeteer-core 冒烟测试 |

## 许可

Apache-2.0 OR MIT 双许可，任选其一。依赖各有其许可（Stylo 为 MPL-2.0）。
