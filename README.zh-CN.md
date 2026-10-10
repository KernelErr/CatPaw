# CatPaw

**专为 AI agent 设计的 headless-first 浏览器，用 Rust 从零实现。**

CatPaw 是一个把 LLM agent 当作第一用户的浏览器。它像浏览器一样执行页面的 JavaScript
和 DOM，只在有人索取几何信息时才计算布局，并给 agent 它真正需要的东西：带稳定元素引用的
紧凑页面快照、精确的"页面已稳定"信号，以及每次操作后页面变了什么，而不是把整页再给一遍。
凡是要代用户发出去的内容，它都先问用户；网站需要真人操作时，它把标签页交给用户。

**状态：v0.1，预览版。** agent 工具现在已经可以通过 MCP 使用（[路线图](#路线图)中的
M0 到 M3）；1.0 之前，工具和返回格式仍可能变化。还不能像浏览器那样工作的地方，列在
[已知欠缺](docs/architecture.md#known-gaps)（英文）里。

[English](README.md) · [功能清单](docs/features.md)（英文） · [架构](docs/architecture.md) ·
[决策记录](docs/adr/) · [更新日志](CHANGELOG.md) · [完整设计（中文）](docs/design.zh-CN.md) ·
[catpaw.sh](https://catpaw.sh)

## 安装

> v0.1.0 正在准备中，发布之前请先从源码编译（见下文）。

macOS（Apple 芯片）和 Linux（x86_64、aarch64）：

```sh
curl -fsSL https://catpaw.sh/install.sh | sh
```

Windows（PowerShell）：

```powershell
irm https://catpaw.sh/install.ps1 | iex
```

安装脚本会从 [Release](https://github.com/KernelErr/CatPaw/releases) 下载适合你系统的
压缩包，用 Release 里的 `SHA256SUMS` 校验，然后列出要写入的内容和位置（只有一个文件
`catpaw`，放在 `~/.catpaw/bin`；Windows 上是 `%LOCALAPPDATA%\Programs\CatPaw` 下的
`catpaw.exe`，并把这个目录加入你的 PATH），等你确认后才写入。`sh -s -- --dir <目录>`
（PowerShell 用 `-Dir`）可以装到别的目录，`--yes` 跳过确认，`--uninstall` 卸载，
`--help` 列出全部选项。Intel 芯片的 Mac 和其他系统请从源码编译，需要 Rust 1.89 或更新：

```sh
git clone https://github.com/KernelErr/CatPaw && cd CatPaw
cargo build --release -p catpaw        # 产物在 target/release/catpaw
```

crates.io 上的 crate（`catpaw` 0.0.1 和五个库 crate）是早期预览，那时 CatPaw 还不能执行脚本。

## 供 agent 使用

把 `catpaw mcp --stdio` 注册到 agent 所在的宿主：

```sh
catpaw setup claude-code                # 打印 `claude mcp add` 命令
catpaw setup codex --write              # 写入 ~/.codex/config.toml
catpaw setup cursor -- --policy strict  # -- 之后的参数传给 `catpaw mcp`
```

agent 会拿到十五个工具：`navigate`、`snapshot`、`click`、`type`、`fill`、`press`、
`select`、`act`（hover、check、uncheck、focus、clear、scroll、upload、drag）、`wait`、
`read`（markdown、text、links、forms、tables、find、html、download）、`screenshot`、
`evaluate`、`tabs`、`logs` 和 `handoff`。元素用 ref 指代，元素还在页面上时 ref 一直有效。
每个操作在页面稳定后返回，说明发生了什么、页面变了什么：

```text
ok click e15 button "Add to cart" (after e14 button "View details for Sauce Labs Backpack")
# s3 changed=1 added=1 removed=1
~ e10 button "Cart, empty" → "Cart, 1 items"
+ e36 button "Remove" (in e12, after e14)
- e15 button "Add to cart"
```

- **代用户发出的内容要用户批准。** 默认策略下，表单提交和文件上传会停下来等用户批准：
  `needs_confirmation c1: click e8 button "Login" would submit → POST
  https://…/authenticate (fields: username=tomsmith, password=***)`。宿主支持 MCP
  elicitation 时（Claude Code 支持）在它自己的弹窗里询问；否则 CatPaw 在用户的浏览器里
  打开一个批准页面。批准后，被挂起的提交只发送一次，不会重新点击。`--policy strict` 还会
  在脚本向其他站点发送数据和 `evaluate` 之前询问；`--trust <host>` 让某个主机不必询问；
  `--allowed-domain` 把标签页限制在指定域名内。
- **交给用户（hand-off）。** 网站需要真人操作时（登录、面向人类的验证），`handoff` 在
  用户自己的浏览器里打开这个标签页。用户像在网页上一样点击、打字；提交给这个网站的内容
  直接发出，页面要发出的其他内容先等用户允许。agent 拿回页面时，用户输入的内容一律遮蔽。
- **每次调用都有记录。** `--flight-log <dir>` 记录每次调用（`--flight-screens` 为每个
  动作加一张截图；密码只记长度），`--profile <dir>` 在会话之间保留 cookie、存储、
  checkpoint 和记录。
- **回放。** `--record-har run.har.zst` 录下会话的流量，`--replay-har` 不联网把它放
  回来；再加上 `--random-seed` 和 `--time-origin`，回放结果逐字节一致。

出错时会说明下一步可以怎么做；页面没有稳定时，会说明它还在等什么。协议见
[ADR 0006](docs/adr/0006-agent-protocol.md)，快照格式见
[ADR 0005](docs/adr/0005-cst-snapshot-format.md)。

## 命令行用法

```sh
catpaw fetch https://example.com --snapshot
catpaw fetch https://news.ycombinator.com --markdown
catpaw fetch https://news.ycombinator.com --js --console   # 先执行页面脚本
catpaw fetch https://example.com --js --eval "document.title"
catpaw fetch https://example.com --js --screenshot page.png
# 通过表单登录，并把会话留给下一次运行
catpaw fetch https://site.example/login --js --cookie-jar ./jar.json \
    --action "fill #username bob" --action "fill #password secret" --action "press Enter" --text
```

要在自己的脚本里调用 CatPaw（爬虫、定时任务、CI 里的检查），见
[在脚本中使用 CatPaw](docs/scripting.md)（英文）。

## 与众不同之处

- **为 agent 设计的接口。** 快照是紧凑的文本树，是 Playwright aria snapshot 的超集，
  ref 永不复用；每个操作都等页面稳定，并直接返回 diff，agent 走一步就是一次往返。
- **精确的"已稳定"。** CatPaw 掌握事件循环，知道每个在途请求、每个定时器（以及设置它的
  那行代码）、每个动画帧和 microtask；超时时会说出页面还在做什么。
- **按需布局。** 样式（Stylo）和布局（Taffy、Parley）只在脚本或 agent 需要几何信息或
  截图时才运行。
- **诚实的身份。** CatPaw 表明自己是谁（`CatPaw/0.1.0 (+https://catpaw.sh/bot)`），
  不提供指纹伪装，不做验证码破解。需要经过验证的身份的部署方，可以用自己的密钥按
  [Web Bot Auth](https://datatracker.ietf.org/wg/webbotauth/about/) 给请求签名；网站
  需要真人时，由用户来操作（[ADR 0003](docs/adr/0003-identity-bot-auth-and-challenges.md)）。
- **纯 Rust。** JavaScript 引擎是 Boa；V8 是计划中的可选后端
  （[ADR 0001](docs/adr/0001-js-engine-boa-default.md)）。

## 对比

在二十个自动化练习站点的任务上，agent 读到的 CatPaw 结果共 68 KB（约 19 500 个 token），
Playwright MCP 走同样的步骤是 243 KB（约 69 500 个 token）；遇到长文章时，CatPaw 把页面
折叠到 4000 个 token，Playwright MCP 的快照则有 160 000 个。任务、测量方法和全部数字见
[docs/comparison.md](docs/comparison.md)（英文）。

## 路线图

| 里程碑 | 范围 | 完成标准 |
|---|---|---|
| M0 抓取与阅读（已完成） | HTTP/1.1+2、cookie、Web Bot Auth 签名、HTML 解析、Stylo 样式、快照、markdown/文本/表单视图、命令行 | `catpaw fetch … --snapshot` 能用于真实页面；WPT tree-construction 套件在 CI 中运行 |
| M1 脚本运行（已完成） | Boa realm、由 Web IDL 生成的绑定、带虚拟时间的事件循环、解析与脚本交错、fetch/XHR、脚本预算、进程内 WPT runner | WPT `dom/`、`html/dom/`、`fetch/api/`、`xhr/` 按记录的预期通过；React 与 Vue 应用完成 hydration |
| M2 交互（已完成） | 布局、命中测试、输入、表单、导航与历史、iframe 与弹窗、存储、observer、截图、Canvas 2D、Web Crypto、WebSocket、Worker | 能登录真实网站；Turnstile 组件以测试 site key 完成验证 |
| M3 agent API（已完成） | MCP 上的快照、diff 与预算、稳定判定、操作后果、阅读视图、标签页、HAR 录制回放、确认、飞行记录、checkpoint 与 profile、hand-off、`catpaw setup` | 练习站点上的任务集通过 MCP 完成并能逐字节回放；确认与 hand-off 在 Claude Code 中可用 |
| M4 保真与验证 | 识别人机验证、覆盖每种 Cloudflare 挑战模式的测试区、给部署方的 Web Bot Auth 文档、Intl 与 CSSOM | 有实测通过率 |
| M5 规模与兼容 | 多租户限额、OpenTelemetry、Docker、CDP 子集、V8 后端追平 | 单机 1000 个上下文；puppeteer-core 冒烟测试 |

## 参与开发与安全

规则和开发者命令见 [CONTRIBUTING.md](CONTRIBUTING.md)，报告安全问题见
[SECURITY.md](SECURITY.md)。

## 许可

Apache-2.0 OR MIT，任选其一。依赖各自带有许可证（Stylo 为 MPL-2.0）；Release 压缩包里的
`THIRD-PARTY-LICENSES.html` 列出了全部依赖的许可证。
