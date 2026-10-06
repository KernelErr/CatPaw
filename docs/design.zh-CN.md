# CatPaw：面向 Agent 的 headless-first 浏览器（Rust 从零实现）设计方案

## Context（背景与目标）

- 需求：设计一个开源浏览器，专为 LLM agent（像 Claude Code 这样的 agent）使用；Rust 从零实现；支持 JavaScript；能通过 Cloudflare 的验证；headless 是第一优先级，渲染不是。
- 现状：从空仓库开始的纯绿地项目，工具链为 stable Rust（edition 2024）。
- 为什么不直接用 Chromium headless：每个 tab 上百 MB、进程模型重、"页面是否稳定"只能靠 `networkidle` 这类猜测、agent 视角的 API（语义快照、稳定引用、diff）都要在外层拼凑、虚拟时间/状态分叉做不到。
- 首批落地步骤：(1) 初始化 Cargo workspace 与仓库基础设施；(2) 把本设计落成 `docs/architecture.md` 与 ADR；(3) 实现 M0 垂直切片（网络 + 解析 + 样式 + 语义快照 + CLI），让 agent 立刻能"读"静态/SSR 页面。

## 一页纸结论

| # | 决策 | 结论 |
|---|---|---|
| 1 | "从零"的边界 | 不 fork Chromium/Firefox/WebKit/Servo；DOM、事件循环、绑定层、导航、agent API 自写；允许叶子 crate（html5ever、Stylo、Taffy、Parley、tiny-skia、hyper/rustls、Boa） |
| 2 | JS 引擎 | **默认 Boa（纯 Rust），V8 为一等可选后端**，`JsRuntime` 抽象从 M1 起硬性要求，两后端都进 CI（见下方已确认的决定） |
| 3 | DOM 所有权 | Rust arena（`SlotMap<NodeId, Node>`）+ 每节点一个 JS wrapper + WebKit 式 "opaque root" 存活规则；不把 DOM 放进 Boa GC |
| 4 | 绑定层 | 从 WebIDL（webref 快照 + 覆盖层）用 `cargo xtask bindgen` 生成 Boa 胶水，产物提交仓库 |
| 5 | 线程模型 | N 个引擎线程（普通 OS 线程 + 手写消息循环），每个 browsing-context group 一个 Boa `Context`，永不迁移；网络/解码/光栅在 tokio 与 blocking pool |
| 6 | 布局/渲染 | Stylo 样式 + Taffy/Parley 布局 **按需**计算（被脚本或 agent 观察到才算），tiny-skia CPU 截图与 Canvas 2D，内置固定字体集 |
| 7 | 通过 Cloudflare 的方式 | 做一个真正的浏览器 + 诚实身份：完整执行挑战 JS、Turnstile 勾选框用普通 click、fetch 层内建 **Web Bot Auth**、注册 Signed Agent、申请 Browser Developer Program、过不去就 **human hand-off**；**不做**指纹伪装与打码 |
| 8 | Agent 接口 | CST 语义快照（Playwright aria-snapshot 超集）+ 永不复用的稳定 ref + diff + 精确 `settled` + 一步一往返；JSON-RPC/WS + MCP 内建；CDP 子集放 v2 |
| 9 | 许可与语言 | Apache-2.0 OR MIT；代码与文档英文，README 提供中文；Rust edition 2024 |

**三项已确认的决定**
1. JS 引擎默认值：Boa（选定理由：纯 Rust、无 C++ 工具链、项目身份清晰）vs V8（约快 200 倍、兼容性最好，但项目会退化成"又一个 V8 封装"，Moli/Obscura 已经在做）。
2. Cloudflare 策略：诚实身份 + 合作通道 + hand-off，不含伪装 Chrome 的指纹 profile、不接打码平台。这既是立场也是风险控制（被 Cloudflare 认定为规避工具会毁掉合作路径）。
3. 项目名沿用目录名 CatPaw，许可 Apache-2.0 OR MIT。

## 设计立场

1. 优先级：headless 下让 agent 可靠地浏览真实站点 > JS/DOM 保真（现代 SPA、iframe、postMessage、fetch/XHR/WebSocket、storage、Web Crypto、Canvas 2D）> agent 工效（语义快照、settled 信号、廉价 context）> 按需渲染（正确优先，不求好看）> 性能。
2. v1 非目标：WebGL/WebGPU 渲染（`getContext` 返回 null）、音视频播放、扩展、WebRTC、打印、bfcache、HTTP/3、SharedArrayBuffer。
3. 引擎不替运营者做身份决定：UA、品牌、Bot Auth 密钥都是 context 配置里的"声明"；引擎不内置任何"伪装成别的浏览器"的 profile。

## 生态现状与技术选型（2026-10 调研结果）

| 组件 | 选型 | 现状 | 备注 |
|---|---|---|---|
| JS 引擎（默认） | **Boa** `boa_engine` 0.22.0（2026-08-28） | test262 95.6%，纯 Rust，Unlicense/MIT | 解释器无 JIT；第三方基准 V8 48107 / QuickJS-NG 647 / **Boa 208**。`Context` 是 `!Send`。有 `create_realm`/`enter_realm`、`HostHooks::create_global_object/create_global_this`、`ConstructorBuilder::inherit/custom_prototype`、`JsProxyBuilder`、`WeakGc`、可插拔 `Clock`/`JobExecutor`、`RuntimeLimits`；缺公开的 exotic object 内部方法、`JsProxy::target()`、跨线程中断（指令计数只在 `fuzz` feature 下） |
| JS 引擎（可选） | **V8** `v8` 152.x / `deno_core` | 预编译静态库 | feature `js-v8`，CI 同步测试 |
| HTML 解析 | `html5ever` 0.40.1 | 流式 tokenizer + `TreeSink`，支持 declarative shadow root | — |
| 样式 | **Stylo** `stylo`/`selectors`/`stylo_dom`/`stylo_traits`/`stylo_atoms` 0.22（2026-09-30） | Servo 维护者每月发布，MPL-2.0 | 自有 DOM 实现 `TDocument/TNode/TElement/TShadowRoot` + `selectors::Element`；模板是 Blitz 的 `blitz-dom/src/stylo.rs`（约 1250 行，其 `TShadowRoot` 是 `todo!()`，我们要补上）；`stylo_taffy` 做 `ComputedValues → taffy::Style`。lightningcss 只有 AST 无级联，不用 |
| 布局 | **Taffy** 0.14 + **Parley** 0.11 | block/flex/grid/float；HarfRust 整形、bidi、CJK 断行；fontique 字体发现；skrifa 轮廓 | Taffy 无表格算法，照 Blitz `table.rs`（约 650 行）把表格映射到 grid |
| 光栅 | **tiny-skia** 0.12 | linebender 维护，BSD-3 | 绘制层放在小 trait 后，日后可换 vello_cpu |
| 网络 | hyper 1.11 + h2 0.4 + **rustls** 0.23 + hickory-resolver + cookie_store + http-cache-semantics | rustls 可配密码套件/密钥组顺序、ALPN、ECH；**不能**定制扩展顺序/GREASE；h2 不能定制 SETTINGS 顺序 | 对"诚实且稳定的自有指纹"足够；刻意不用 wreq/boring 这类模仿 Chrome 指纹的栈。h3 0.0.8 仍实验性 |
| Bot Auth | Cloudflare 的 `web-bot-auth` 0.7.0 + `http-signature-directory` | ed25519-dalek | 直接复用 |
| WebIDL | `weedle2` 5.0（解析器） | **没有**现成 WebIDL→Boa 生成器 | 自写 `catpaw-webidl`，是核心基建 |
| 异步 | tokio 多线程 runtime（I/O）+ blocking pool（解码/光栅） | — | JS/DOM 固定在引擎线程 |

## 同类项目与差异化

**最直接的对标是 Lightpanda**（Zig + V8，2026-10-02 发布 1.0，AGPL-3.0）：自有 DOM、html5ever 解析、libcurl 网络、V8 堆快照加速启动；CDP/BiDi server、内置 agent loop 与 MCP；WPT 通过约 174 万子测试（约为 Chrome 的 80%）；自报 21 MB / 4.6% CPU（Chrome 402 MB / 158%）。**硬伤**：没有渲染管线，"元素位置按 DOM 深度和兄弟顺序模拟"，截图是占位符；iframe 默认关闭且键盘输入到不了 iframe 内元素；仅 Linux glibc。维护者在 1.0 的 HN 讨论里明确说"绕过反爬不是我们的目标"。

**Rust 阵营**（二手调研，未逐一核实）：Moli（V8 + Stylo + Taffy/Parley + vello_cpu，CDP/WebDriver/BiDi，10.6k stars，中位峰值 92 MiB）、Obscura（deno_core + 自有 DOM/CSS + Taffy + tiny-skia，CDP，约 30 MB RSS）、OxiBrowser（Boa + html5ever + Blitz/Stylo，自有 DOM，CDP，约 4.75 万行，与本方案技术栈最接近）、BrowserOxide、rakers、h5i；Blitz 自身有 Boa 的 JS 草稿 PR（#491）未合并。它们都在"Rust 复刻带 CDP 的 Chromium 替身"这条赛道上。

**Agent 看页面的行业共识**：Playwright MCP / Vercel agent-browser / Perplexity Comet / Claude in Chrome 用"带 ref 的无障碍树文本"（Playwright：`- button "One" [ref=e2]`，iframe 内 `f1e2`，另有 `[cursor=pointer]`、`[active]`），browser-use 用带下标的可交互元素列表 + 截图，OpenAI/Google 的 CUA 走纯截图 + 坐标。Playwright 的 aria snapshot 是注入页面的 JS 算出来的；其 `networkidle` 定义是"500ms 内无在途请求（排除 favicon 与 EventSource）"。Chrome 149 的 WebMCP（站点通过 `navigator.modelContext` 暴露工具）值得 v2 支持。

**CatPaw 的差异化**：(1) agent 原生接口（CST + 稳定 ref + diff + 精确 settled + 一步一往返），CDP 只是 v2 兼容层；(2) 真实（按需）布局与截图，iframe 是一等公民（Turnstile 就在 iframe 里）；(3) 诚实身份 + Web Bot Auth 内建于 fetch 层 + hand-off；(4) 确定性（虚拟时间、网络回放、checkpoint）；(5) 纯 Rust 默认引擎，Apache-2.0 OR MIT。

## 架构总览：crate 划分与依赖方向

```
catpaw (CLI) → catpaw-server (JSON-RPC/WS + MCP + hand-off) → catpaw-agent (CST 快照/动作/settled)
  → catpaw-engine (线程池、后端选择、嵌入 API)
    → catpaw-bindings-boa (生成的胶水、proxies、WindowProxy/Location)
      → catpaw-web (事件循环、navigables、会话历史、文档生命周期、timers、workers、storage、
                    XHR/fetch/WebSocket/crypto/streams；只通过 dyn trait 触达 JS)
      → catpaw-js-boa (realms、JobExecutor、Clock、module loader)
        → catpaw-dom (arena、树操作、HTML 元素状态、html5ever sink、事件派发、表单、MutationObserver)
        → catpaw-style (Stylo traits、UA 样式表、restyle 驱动)
        → catpaw-text (fontique/parley 上下文、内置字体、轮廓)
        → catpaw-layout (Taffy/Parley box tree、hit-test、滚动、几何)
        → catpaw-paint (display list、tiny-skia、截图)   → catpaw-canvas (Canvas 2D)
        → catpaw-fetch (Fetch 规范：CORS、重定向、referrer、cache) → catpaw-net (hyper/rustls/h2、cookies+PSL、代理、HAR、Web Bot Auth 签名)
        → catpaw-js (引擎无关的 JsRuntime traits：回调、promise、异常、中断、structured clone)
catpaw-webidl (IDL 模型 + emitter，被 xtask 使用)    catpaw-protocol (protocol.json，schemars 生成)
xtask (bindgen、IDL 同步、WPT runner、html5lib-tests)
```

`catpaw-web` 只经由 `catpaw-js` 的 trait 访问 JS，所以 `web ↔ bindings` 无环；`catpaw-style` 是唯一碰 Stylo 的 crate（隔离其每月的 breaking 发布）。

## 引擎内核设计

### (a) DOM 所有权与 JS GC：arena + wrapper 缓存 + "opaque root"

- 否决"节点放进 `boa_gc::Gc`"：每次 Rust 遍历都付 `borrow + downcast + GcBox` 间接成本；boa_gc 是线程局部、非分代、非增量的 mark-sweep，每次回收会标记整棵 DOM（真实站点 10 万+ 节点）而不是几千个 wrapper；Stylo 要求 `TElement: Copy` 就得包裸指针；且把 DOM 焊死在 boa_gc 上，V8 后端要另写一套内存模型。
- 采用：每个引擎线程一个 `SlotMap<NodeId, Node>`（不按文档分，`adoptNode` 时 id 稳定）；`NodeId` = index + generation，陈旧持有者安全失败；Stylo/Taffy/html5ever 看到的是 `(&Dom, NodeId)` 这种 `Copy` 句柄。
- 存活规则（WebKit/Blink 模型）：
  1. 每棵节点树（文档树、每棵分离子树、template 内容；shadow tree 归宿主）最多一个 `Gc<TreeToken>`，`TreeToken { wrappers: GcRefCell<HashMap<NodeId, JsObject>> }`；wrapper 的 native data `NodeWrapper { id, token: Gc<TreeToken>, listeners, ce_state, … }` 强持有 token，token 强持有本树所有 wrapper，Rust 侧只弱持有 token（`Dom.tokens: HashMap<root NodeId, WeakGc<TreeToken>>`，同时就是节点→wrapper 的查找表）。结果：每节点一个 wrapper（realm = 节点所属文档的 realm，同源 frame 间 `el === el`）；expando 跨 detach/re-attach 存活（wrapper 从不重建）；分离子树只要任一 wrapper 可达就整棵存活（JS → wrapper → token → 兄弟 wrapper）；`WeakRef` 行为自动正确。
  2. 所有引用 JS 值的状态（listeners、编译后的 `onX`、custom element 实例）放在 GC 追踪的 wrapper data 里，绝不放 arena（Rust 持有的 `JsObject` 是 root，放 arena 就是 IE6 式泄漏）。
  3. 无 wrapper 的分离子树移除时直接释放；有 wrapper 的在 `TreeToken::finalize` 里释放；insert/remove 本来就要遍历子树做 connectedness 与 CE reactions，同一次遍历顺便在 token 间搬 wrapper。
  4. 之后要向节点派发事件或把节点交还脚本的 Rust 持有者（pending image/fetch 任务、Range、MutationRecord、focus）持有 `NodeHandle`（按需创建的 rooted wrapper clone），其余只持 `NodeId`。
  5. 纪律：持有 arena `RefMut` 时绝不分配 JS 对象（GC 可能在分配时运行且 finalizer 会碰 arena）；通过只让 `DomCtx` 的 `&mut` 方法能分配来强制。
  6. V8 后端：同一 arena，wrapper 槽换成 `v8::TracedReference`，token 换成 cppgc `GarbageCollected`，语义不变。

### (b) WebIDL 驱动的绑定生成

- IDL 来源：vendored `@webref/idl` 精选快照（已校验、继承已解析）+ `idl/overlay/*.idl` 加 CatPaw 扩展属性（`[Reflect]`、`[ReflectDefault=]`、`[CatPawUnimplemented]`）。导入前核实 webref 许可。
- 生成方式：`cargo xtask bindgen`，产物提交仓库，CI 做 diff 检查。不用 build.rs（不透明、clean build 重跑、不可索引），不用 proc-macro（mixin/partial/overload 集/原型链需要全语料知识，且每次编辑都重编）。流水线：parse → 合并 partial/mixin → 解析类型 → 计算 effective overload set → 输出 `emit_boa.rs`（以后 `emit_v8.rs`）+ 每接口一个后端无关的 `trait XImpl`（列出胶水要调用的函数，漏实现 = 编译错误）。
- 接入方式：Node 系接口是 `catpaw-dom` 里作用于 arena 句柄的自由函数（`fn value(dom: &Dom, el: NodeId) -> DomString`），继承 = 基类函数接受同一个 `NodeId`，按 `node.kind` 分派；非 Node 层级（Event → UIEvent → MouseEvent）每个具体接口一个 native struct 内嵌基类，生成的访问 trait（`AsEvent`）做 blanket impl，加生成的 `InterfaceId` 品牌检查。
- 原型链：**不用** `Class`/`ClassBuilder`（不支持继承）。每 realm 用 `ObjectInitializer` 生成 prototype 对象（父原型、accessors、方法、`@@toStringTag`、`constructor`），用 `ConstructorBuilder::new(ctx, f).inherit(parent_proto).custom_prototype(parent_interface_object)` 生成 interface object，于是 `Object.getPrototypeOf(HTMLInputElement) === HTMLElement`、`instanceof` 原生正确；原型表放 `Realm::host_defined_mut()`；wrapper 用 `from_proto_and_data(proto_from(new_target), data)`，这同时给出 custom element 升级。
- 扩展属性：`[Reflect]` 按反射规则生成内容属性 getter/setter；`[CEReactions]` → `dom.with_ce_reactions(..)`；`[LegacyUnforgeable]` → wrap 时定义不可配置 own accessor；`[Replaceable]`、`[PutForwards]`、`[Exposed]`/`[SecureContext]`（realm 初始化时的每 global 接口表）、`[Global]`（`HostHooks::create_global_object` 返回 Window，链 Window.prototype → WindowProperties → EventTarget.prototype；`create_global_this` 返回 WindowProxy）。
- Exotic object：Boa 0.22 没有公开的内部方法，所以 HTMLCollection、live NodeList、NamedNodeMap、DOMStringMap、Storage、form/select/window 具名属性、WindowProxy、Location 都用 `JsProxyBuilder` + native trap；`JsProxy` 没有 `target()`，先用 realm 私有 symbol 让 `get` trap 交出 target，并向上游提一行 PR。静态 NodeList 直接物化索引属性。
- 转换：dictionary → struct（按字典序成员转换）；union → enum（distinguishing 算法）；enum 参数 TypeError / 属性赋值忽略；sequence 走迭代器协议；`iterable<>`/`maplike` 生成迭代器；callback interface 保存 `JsObject` 调用时查 `handleEvent`；返回 Promise 的操作用 `JsPromise::new_pending` 并把 throw 转 reject。

### (c) 事件循环与线程

- N 个引擎线程（默认 `min(cores, 4)`），普通 OS 线程 + 手写消息循环（引擎线程上不跑 async）。每个顶层页面 + 其 iframe + 其 popup（一个 browsing-context group）= 一个 Boa `Context`（自己的 microtask 队列、interner）+ 每 Window 一个 realm；group 创建时分到最空闲线程，永不迁移。跨域 iframe 必须同线程（规范要求 WindowProxy/Location 跨域访问是同步的），隔离由 proxy trap 检查当前 realm 的 origin 来保证。线程外：tokio 多线程 runtime（net/DNS/TLS/解压）、blocking pool（图片解码、光栅）、agent API server、Web Workers（独立线程 + 独立 `Context`）、存储持久化。
- 消息：每引擎线程一个有界 inbox（`mpsc`，1024）`Msg { group_id, seq, payload }`。响应 body **拉取式**（解析器 / `Response.body` 读者调 `BodyHandle::pull(n)`），慢页面无法淹没线程，`ReadableStream` 背压精确。`postMessage` 同线程：在源 realm structured-serialize 成引擎无关的 `SerializedValue`（自写，Blob/File/ImageData/MessagePort 等平台对象要参与），向目标 window 排任务；跨线程（workers、BroadcastChannel）同一 `SerializedValue` 是 `Send`，ArrayBuffer 转移即 detach，MessagePort 是端口注册表里的线程安全端点。
- 循环：任务源（DOM 操作、用户交互、网络、导航、timers、postMessage、websocket、parser）固定优先级、源内 FIFO；每个任务后、以及每次回调进 JS 且 JS 栈空时跑 microtask checkpoint = 自定义 `JobExecutor` 排空 `PromiseJob`/`NativeJob`（我们的 `queueMicrotask`、MutationObserver 通知、CE reactions）；`NativeAsyncJob`（模块加载 future）放 `FuturesUnordered`，waker 往 inbox 投递时轮询；timers 自管（按 `(deadline, seq)` 的堆，4ms 嵌套钳制），不用 Boa 的 `TimeoutJob`。渲染机会：仅当存在 rAF/动画/Intersection/ResizeObserver 时才有 16.67ms 的 frame 任务（resize/scroll steps、media query、Stylo 动画、rAF、observers，只在此时强制布局）；`requestIdleCallback` 在 inbox 为空时跑（50ms deadline）。
- 时钟：共享的 `Clock` 驱动 `Date.now`、`performance.now`、timers。虚拟时间模式：仅当无任务/microtask/inbox 消息/在途 fetch/解码/worker 消息时才推进，跳到最早的 timer/frame deadline，受每次调用的预算限制（Puppeteer `setVirtualTimePolicy` 语义）；真实时间模式用 `recv_timeout` 到下一个 deadline。
- CPU 预算：Boa 不能跨线程中断，`instructions_remaining` 只在 `fuzz` feature 下可用。M1 先开 `fuzz` 并按任务重置指令预算（超限 = 不可捕获的 runtime-limit 错误结束该任务）+ `RuntimeLimits` 的递归/栈上限兜底 + 每个 native 调用检查 group deadline，同时向上游提 atomic interrupt flag（issue #3238 有需求）。防饥饿：单任务时间片 1s、每 agent 命令的页面累计预算、`isolation: Thread` 让某 group 独占线程。

### (d) Browsing context 与导航

- 解析：html5ever `Tokenizer<TreeBuilder<NodeId, Sink>>`（`TreeSink` 方法是 `&self`，sink 持 `RefCell<Dom>`）；`encoding_rs` 嗅探；分块喂给有时间预算的 parser 任务（20ms 后重新排队），让 timers/rAF 在解析期间也能跑。`feed()` 返回 `TokenizerResult::Script(id)` 时暂停：inline 脚本立即执行；parser-blocking 外部脚本挂起喂入直到取回；`defer` 在解析完成后、DOMContentLoaded 前运行；`async` 加载完在任务间运行；模块默认 defer。`document.write` 在脚本执行期间向插入点的独立 `BufferQueue` 重入喂入（照 Servo 的 `ServoParser`）。`readyState`、`DOMContentLoaded`、`load`（受图片、iframe、阻塞样式表、脚本门控）按规范实现。
- 会话历史：每 traversable 一组条目（URL、document 或 null、序列化 state、滚动恢复）；`pushState/replaceState` 同步；`back/forward/go` 排遍历任务；fragment 导航触发 `hashchange`/`popstate`；无 bfcache。Navigation API 推到 M3。新导航中止进行中的导航（丢弃 net future）、跑 `beforeunload` 策略、触发 `pagehide`/`unload`，然后 abort 文档：取消 timers、abort fetches/`AbortController`、停解析器、关 socket、终止 workers。
- iframe：初始同步 `about:blank`、`src`/`srcdoc`、sandbox 标志、CSP `frame-ancestors`/X-Frame-Options、跨域 `contentDocument` 为 null、WindowProxy/Location 只暴露跨域可访问子集。Agent 策略：最多 32 个 frame、深度 8、`blockThirdPartyFrames` 选项。popup：`window.open` 需要 transient activation（agent 点击授予），在同 group 内建新页面（`opener`/`postMessage` 可用）、上报 `popup` 事件、每页最多 5 个，否则返回 null（等价于弹窗拦截）。Dialog 永不阻塞引擎线程：默认 `alert: accept`、`confirm/prompt: dismiss`、`beforeunload: proceed`，全部记录并可被 agent 注册的同步规则覆盖。下载：`Content-Disposition: attachment`、不支持的 MIME、`a[download]` 流入每 context 有上限的存储并发事件。表单：entry list 构造、`submit`/`formdata`/`invalid` 事件、约束校验、urlencoded/multipart/text-plain、GET/POST/dialog、隐式提交、`requestSubmit`、`form*` 属性覆盖。

### (e) 样式、布局、绘制：按需

- Stylo：为 `CatNode<'a> = (&'a Dom, NodeId)` 实现 `TDocument/TNode/TElement/TShadowRoot/NodeInfo` 与 `selectors::Element`；每元素在 arena 里持 `AtomicRefCell<Option<ElementData>>`、dirty/snapshot 标志、`Cell<ElementSelectorFlags>`；每文档一个 `Stylist` + `Device`、`SharedRwLock`；`Stylesheet::from_str` 装 UA/作者样式表，`StylesheetLoader` 处理 `@import`；`RecalcStyle: DomTraversal` 由 `style::driver::traverse_dom` 驱动（先顺序，后 rayon）。失效 v1：属性/class/state 变化 → 元素上 `RestyleHint::restyle_subtree()` + 祖先 `set_dirty_descendants`；插入重算插入子树及其后兄弟。v2：`ServoElementSnapshot`/`SnapshotMap` 精确失效。样式更新是惰性的：`getComputedStyle`、带 `:hover` 类状态的选择器匹配、布局都先 `dom.flush_style()`。
- 布局：为 arena 实现 `taffy::LayoutPartialTree`（+ Block/Flexbox/Grid 容器 trait、`CacheTree`），`stylo_taffy::TaffyStyloStyle` 转样式；block、flex、grid、float/clear（Taffy `float_layout`）、absolute/fixed（fixed = 相对初始包含块的 absolute）、relative；`sticky` 在 v1 当 relative；表格按 Blitz `table.rs` 映射到 grid（`ColumnCursor` 处理跨行列、border-spacing 当 gap）；行内格式化用 Parley `TreeBuilder`（文本 run + `InlineBox` 表示 inline-block/替换元素，`break_all_lines(Some(available))`、`align`），在 `compute_leaf_layout` 里测量。overflow 生成滚动容器与每元素偏移；`scrollTo/scrollIntoView/scrollTop` 即时生效（`behavior: smooth` 立即完成并触发 `scrollend`），`scroll` 事件在 scroll steps 里触发。
- 惰性：`Document` 维护 `style_dirty`/`layout_dirty`；只有 `getBoundingClientRect`、`offset*`/`client*`、依赖布局的 `getComputedStyle` 值、`elementFromPoint`、`scrollIntoView`、observers、截图会调 `update_layout()`。无 observer、无截图消费者时 frame 完全跳过样式与布局（**no-layout 快路径**，headless 常态）。
- 命中测试：按绘制顺序逆向遍历 box tree（stacking context、定位盒、overflow 裁剪、`pointer-events`、`visibility`）。agent 点击 = 滚入视口、取第一个非空 client rect 的中心、hit-test、校验命中的是目标或其后代（Playwright actionability）、派发 pointer/mouse/focus/click 序列。
- 绘制：box tree → display list → tiny-skia（背景、边框、图片经 `image`、文字经 Parley glyph run → skrifa 轮廓 → path、opacity、transform、clip；无 filter/shadow）。光栅在 blocking pool，截图永不阻塞 JS。Canvas 2D：tiny-skia `Pixmap` 上的完整状态机（path、渐变、pattern、`BlendMode` 做合成、`Mask` 做 clip、`drawImage`、`getImageData`、`toDataURL` 经 `png`），文字用 Parley。字体：内置确定性字体集（DejaVu Sans/Serif/Mono + Noto Sans 回退）装进 fontique collection，系统字体默认禁用，跨机器度量与 canvas 哈希稳定。WebGL：`getContext('webgl'|'webgl2')` 返回 null。

## 网络栈、身份与 Cloudflare 策略（基于 2026-10 官方资料）

### Cloudflare 现状（事实）

- **Web Bot Auth 已进 IETF 标准轨道**：工作组 `webbotauth`，`draft-ietf-webbotauth-httpsig-protocol-00`（2026-09-01）。协议 = RFC 9421 的 profile：`Signature-Input` / `Signature` / `Signature-Agent` 三个头；`Signature-Agent` 是 Dictionary 结构化头（HTTPS URI + `type=directory|jwks_uri|cimd`）且必须被签名覆盖；覆盖组件至少 `@authority`（推荐加 `@method`、`@path`）；`created`/`expires` 必填（Cloudflare 建议分钟级）；`keyid` = JWK SHA-256 thumbprint；`tag=web-bot-auth`；Ed25519；密钥目录在 `/.well-known/http-message-signatures-directory`（JWKS，`kty: OKP, crv: Ed25519`，目录响应自身也应签名）。测试端点 `https://crawltest.com/cdn-cgi/web-bot-auth`（200 已验证 / 401 未知密钥 / 400 格式错）。
- **Signed Agents 可自助注册**：dashboard 的 Bot Submission Form（BotBase，2026-08 起自助 + 自动校验），类型选 Signed Agent，验证方式 Request Signature，提供密钥目录 URL 与 UA 模式；2026-07 起区分 Direct / Intermediary（一个运营者服务多个终端用户 = intermediary）。站点侧字段 `cf.bot_management.signed_agent`，类别 "Agent"（user-directed agents）；**2026-09-15 起新 zone 默认在含广告页面屏蔽 Training 与 Agent 流量**，这是站点所有者的选择，CatPaw 遵守。
- **挑战类型**：Managed / Non-Interactive(JS) / Interactive / Block；Turnstile 三种模式（Managed/Non-Interactive/Invisible），Cloudflare 承诺"永不再出视觉谜题"，交互 = 点一个勾选框。挑战响应带 `cf-mitigated: challenge` 且总是 text/html（会打断 fetch/XHR，SPA 需 Turnstile pre-clearance）；`cf_clearance` 默认 30 分钟、`SameSite=None; Secure; Partitioned`、绑定访问者与设备；Precursor（2026-07）在整个会话期间持续行为评估。
- **官方对新引擎的态度**：支持列表写明"自动化浏览器不支持通过生产环境挑战"、"自定义或深度修改的引擎支持有限"；2025-03 Pale Moon/Falkon/SeaMonkey 曾被困在挑战循环，Servo 的 Turnstile issue（#34320）至今未关。但 Cloudflare 2025-08 推出了 **Browser Developer Program**（面向新兴/嵌入式/小众浏览器的双向沟通渠道与测试集成），并资助了 Ladybird。

### CatPaw 的做法

1. **fetch 层内建 Web Bot Auth**：每 context 可配 Ed25519 私钥 + 目录 URL；签名覆盖 `@authority @method @path signature-agent`，`expires` 默认 60s，含 `nonce`；作用于导航、子资源和重定向后的请求（`signFor` 可按域名限定）；CLI 自带 `catpaw keygen` 与目录文件/签名目录响应生成器。复用 `web-bot-auth` crate，CI 用 crawltest.com 验签。
2. **项目层面走官方通道**：稳定 UA（`CatPaw/<ver> (+https://<project>/bot)`）与密钥目录；以 Signed Agent（Intermediary）提交 BotBase；申请 Browser Developer Program。这两件是运营动作，写进 README 的"部署者须知"。
3. **引擎只做真实浏览器该做的事**：完整执行挑战脚本（需要 DOM storage、cookie partitioning、`challenges.cloudflare.com` 跨域 iframe、postMessage、Web Crypto、Canvas 2D、`performance.now` 一致行为）；把 `cf-mitigated: challenge` 的 fetch/XHR 响应、Turnstile iframe、挑战页 DOM 特征统一上报为 `challenge` 事件；Turnstile 勾选框经普通 click 路径；UA 在会话内绝不变化。
4. **诚实的、自洽的指纹**：rustls 默认配置 + 固定套件顺序，不解析/不模仿 JA3/JA4；h2 默认 SETTINGS；`navigator.userAgent`/`userAgentData` 与 HTTP UA 一致；`navigator.webdriver` 按规范返回 `true`。
5. **过不去就交给人**：`challenge.handoff`（见 Agent 接口层）。
6. **验证用自己的 zone**：自有 Cloudflare zone 上分别开 Managed Challenge、JS Challenge、Turnstile 三条规则作为集成测试靶场；CI 记录通过率，不把"必过"当断言。

**明确不承诺**：对任何第三方站点"必能通过"。结果由 Cloudflare 评分与站点所有者策略决定。

### 其余网络栈

自写 Fetch 规范实现（CORS、重定向、Referrer Policy、CSP 钩子、credentials、`keepalive`、AbortSignal、流式 body）架在 hyper 1.x + h2 + rustls 0.23（ECH 可开）上；DNS 用 hickory-resolver（可配 DoH）；每 context 独立 cookie jar（RFC 6265bis：SameSite、`__Host-`/`__Secure-` 前缀、**CHIPS Partitioned**，`cf_clearance` 依赖它）、HTTP cache（http-cache-semantics）、代理（HTTP CONNECT / SOCKS5 按 context）；WebSocket 用 hyper upgrade 自写帧层或 tokio-tungstenite；所有出站请求经 urlPolicy（私网/metadata IP 默认拒绝）与 PII 脱敏日志；HAR 录制点在 fetch 层。

### 自动化协议兼容现状

WebDriver BiDi 仍是 W3C Working Draft（2026-09-30 版）；Playwright 的 `connectOverCDP` 只支持 Chromium 系，BiDi 在 Playwright 里仍是实验特性；Firefox 已移除 CDP。因此 v1 只做自有协议 + MCP；v2 做 CDP 子集（Lightpanda 实现了 19 个域可对照）；BiDi 作为 v2.5 的前向路径。**但 WebDriver classic 的最小子集要提前到 M1**：WPT runner 靠 WebDriver 驱动浏览器，没有它跑不了 WPT。

## Agent 接口层设计

### 页面表示：CST 语义快照（主视图）

- 格式：**CST（CatPaw Snapshot Text）**，Playwright aria-snapshot 语法的严格超集（`- role "name" [attr=value]`，缩进表示嵌套，`[ref=eN]`），另提供 `format:"json"`。ref 全局 `eN`（不分 frame，iframe 节点标 `[frame=fN origin=…]`），现有 Playwright-MCP 提示词可直接迁移。
- 新增：首行 header（快照 id、tab、doc、url、title、viewport、scroll、filter、节点数、settled、challenge、dialog）、`[offscreen]`、`[occluded-by=eN]`（opt-in）、`[shadow]`（closed shadow root 也展开，agent 层是特权层）、`[value=…]`（密码框恒为 `***`）、`[collapsed=N nodes] [cursor=…]`。

```
# s14 tab=t1 doc=d3 url=https://shop.example/cart title="Cart (2)" vp=1280x720 scroll=0,0 filter=interesting nodes=38/412 settled=yes challenge=none dialog=none
- banner [ref=e1]
  - searchbox "Search products" [ref=e3] [value=""]
  - button "Search" [ref=e4]
- main [ref=e6]
  - heading "Your cart" [ref=e7] [level=1]
  - table "Items" [ref=e8] [rows=2]
    - row [ref=e9]
      - cell: "Wool socks, grey"
      - spinbutton "Quantity" [ref=e11] [value=2] [min=1]
      - button "Remove" [ref=e12]
  - iframe "Payment" [ref=e20] [frame=f2 origin=pay.example]
    - textbox "Card number" [ref=e21] [value=""] [required]
    - button "Pay $44.00" [ref=e22] [disabled]
- contentinfo [ref=e30] [collapsed=12 nodes] [cursor=s14/e30]
```

- Token 预算：`filter: all|interesting|interactive`（默认 `interesting`：可交互 + 标题 + landmark + live region + 有 alt 的图 + ≥12 字文本，纯包装 `generic` 折叠、相邻文本合并）、`root`、`depth`、`maxTokens`；超预算子树折叠为 cursor 续取。
- **ref 分配与失效**：每 tab 一张 `RefTable`（NodeKey ⇄ u32），节点第一次被输出时分配，单调递增、**永不复用**；DOM 的 `node_disconnected` 钩子标 `Stale{Removed}`，跨文档导航标 `Stale{Navigated}`；用到失效 ref 返回 `StaleRef{ref, reason, suggestion?}`（按 role+name+父路径尽力匹配，agent 需显式采用）。
- **Diff**：服务端保留每 tab 最近 8 份快照（只存 ref+attrs），`snapshot({diffFrom})` 输出 `~`/`+`/`-` 三类行；跨文档导航返回全量并标 `doc=d4 (navigated from d3)`。

```
# s15 diff-from=s14 tab=t1 doc=d3 url=(same) settled=yes changed=3 added=1 removed=1 unchanged=33
~ e11 spinbutton "Quantity" [value=2 → 3]
~ e22 button "Pay $44.00" → "Pay $58.00" [disabled → -]
+ e41 status "Cart updated" (in e6, after e8)
- e13 row "Beanie" (+3 descendants)
```

- 其他视图（`page.read`）：`markdown`（readability 正文，链接渲染为 `[text](ref:e12)`）、`text`、`forms`（表单 → 字段 schema）、`tables`（表头推断 → 行对象）、`html`（某 ref 的 outerHTML 切片，剥离 script/style）、`find`（文本/正则 → 上下文 + 最近元素 ref）。

### 动作

- 定位：`{ref}` | `{css, pierce}` | `{text, role?, exact?}` | `{point}`；文本定位有歧义返回 `AmbiguousTarget{candidates≤5}` 而不是猜。
- 可操作性检查（引擎内）：已连接、可见、稳定（两帧 box 不变）、enabled、命中测试到目标或其后代（否则 `Occluded{by}`）；`force:true` 跳过。
- **受信事件序列**与 Chromium 一致（`isTrusted=true`）：click = pointermove/mousemove(+over/enter) → pointerdown → mousedown → 焦点转移 → pointerup → mouseup → click → 激活行为；type = 逐 grapheme 的 keydown → keypress → beforeinput → 改值 → input → keyup，`fill` 为快路径；press 支持组合键，Enter 触发隐式提交；另有 hover/scroll/scrollIntoView/select/check/upload/drag/focus/clear/evaluate（`$ref("e12")` 在 JS 里解析为元素）。
- 每个动作隐式等待 `waitUntil`（默认 `settled`）并可内联 `snapshot: diff|full|none`（MCP 下默认 diff），**一步 = 一次往返**。结果报告后果：导航、dialogs、downloads、popups、DOM 变动摘要、网络计数、新增 console 错误、settled 与否及 pending 报告、是否需要确认。

### 等待与 settled 的精确定义

由调度器直接求值（不轮询），以下全部成立即 `settled`：
1. 任何 frame 无 pending 导航 / 历史遍历 / 表单提交；
2. 无"与文档相关"的 fetch 在途：排除 keepalive/sendBeacon、可配置的统计域名、轮询 timer 发起的请求、长连接流（EventSource/WebSocket/头已到 body 持续 >2s）、prefetch；图片/字体仅当策略加载且影响布局时计入，超过 `assetTimeoutMs`(2s) 放弃；
3. JS：无运行中的 task，microtask 队列空，剩余延迟 ≤ `timerThresholdMs`(250) 的 timer 已触发；同一调用点 re-arm ≥5 次的 timer 判为 `polling-loop` 忽略；
4. rAF：无回调，或连续 ≥3 帧无 DOM/样式变动；
5. DOM：`quietMs`(真实 100ms / 虚拟 2 帧) 内无非装饰性变动；
6. 影响布局的字体/未定尺寸图片已加载或超时；
7. 无打开的 dialog（dialog 报告为 `blocked`）。

`waitUntil: commit|domcontentloaded|load|networkidle|settled`；`wait.for: selector|ref|text|url|predicate|event|state`。超时诊断必须点名元凶（pending fetches 的 url/age/initiator、timers 的 delay/rearmed/site、rAF 状态、DOM 热点、navigation、dialog）并给 `advice`。

### Context / Tab / Session

- Context 配置：identity（UA + Bot Auth 的 signatureAgent/keyId/privateKey/signFor）、storage、proxy、locale、timezone、viewport、geolocation、permissions、资源策略（images/fonts/media 可 block，不影响快照质量）、time（`real` | `virtual`）、dialogs 策略、downloads 沙箱、limits（maxTabs、memoryMb、cpuMsPerAction、jsHeapMbPerTab、maxInflight）、urlPolicy、confirmBefore、recording。
- Tab 租约：`tab.lease({holder, ttlMs})` → token；写操作需 token，读操作不需要；hand-off 期间租约归人。popup → 同 context 新 tab + 事件；资源超限只杀当前 tab。

### 挑战检测与 human hand-off

- 检测：HTTP 层（403/503 + `cf-mitigated: challenge` 等厂商头）、DOM 特征（`#challenge-form`、`script[src*=challenge-platform]`、`iframe[src*=challenges.cloudflare.com]`、hCaptcha/reCAPTCHA/Arkose/DataDome/PerimeterX/Kasada 标记）、标题/文本启发式、刷新循环。
- 事件 `challenge{kind, vendor, canAutoProceed, frameRef, checkboxRef?, url}`，kind ∈ `cf-managed`(auto) / `cf-turnstile`(maybe) / `captcha`(false) / `interstitial` / `blocked`；快照 header 显示 `challenge=cf-managed(auto, 3s)`。
- Agent 三条路：`challenge.wait`；对 `checkboxRef` 普通 click；`challenge.handoff({ttlSec, reason})` → `{sessionUrl, expiresAt}`，人完成或挑战清除后控制权返回。
- 远程查看协议：`wss://host/handoff/{token}` + 同路径 HTTPS 自包含 viewer；token 128 位随机、一次性、绑定 context+tab、有 TTL、可吊销；服务端→客户端二进制帧 `Full{jpeg}` / `Delta{rects}`（tiny-skia damage tracking，≤10fps）/ `Cursor`；客户端→服务端 pointer/key/wheel/text/done，走同一条受信输入管线；按键不写日志，帧默认不落盘。

### Checkpoint / 回放 / 可观测性

- v1 checkpoint = cookies（含 httpOnly）+ localStorage + sessionStorage + IndexedDB（v1.1）+ 各 tab 历史/滚动 + URL + viewport + 虚拟时钟；restore = 重建 context、注入存储、逐 tab 重新导航并等 settled。v2：context arena 的 copy-on-write fork 实现 `tab.fork()`。
- 网络录制/回放：HAR 1.2 + `_catpaw` 扩展（虚拟时间、initiator、WebSocket 帧）；回放 + 虚拟时间 = 逐字节确定的 eval。
- 日志：每 tab 的 console/network/errors 环形缓冲；每 RPC 一个 OpenTelemetry span（子 span：resolve_target / actionability / dispatch / wait_settled）；GIF/WebM 在动作与 settle 点取关键帧（feature flag）。

### 传输层、兼容层、安全

- Rust 库 API 是本体：`Browser::new(cfg)` → `new_context(ContextConfig)` → `new_tab()` → `goto / snapshot / act / wait_for / events()`。
- **schema-first**：`crates/catpaw-protocol/protocol.json`（schemars 生成）派生 JSON-RPC 文档、MCP 工具 schema、TS/Python SDK。
- JSON-RPC 2.0 over WebSocket（`/rpc`，Bearer token）与 stdio；内置 MCP server（`catpaw mcp --stdio|--http`）。
- 安全：非 loopback 必须 token；urlPolicy 在 fetch 层对所有请求生效；**confirm-before 钩子**（动作类型 + 文本正则 + url glob + method）→ `needs_confirmation{confirmationId, preview}`，host 调 `act.confirm` 后才派发，所有表单提交与 POST 导航经同一闸口；日志/HAR/span 经 PII 脱敏器。
- MCP 工具：`catpaw_navigate / snapshot / click / type / press / select / check / hover / scroll / drag / upload / wait / read / screenshot / evaluate / tabs / dialog / challenge / logs / checkpoint`。
- JSON-RPC 方法族：`browser.* context.* tab.* nav.* page.* act.* wait.* dialog.handle download.* challenge.* handoff.end checkpoint.* recording.* logs.query events.*`；事件：tab.opened/closed、nav.*、dialog、download、challenge、challenge.cleared、handoff.*、console、network.*、error、confirmation.required、resource.limit。

## 里程碑与退出标准

| 里程碑 | 内容 | 退出标准 |
|---|---|---|
| **M0 fetch & read**（无 JS） | workspace + CI + 文档/ADR；`catpaw-net`（HTTP/1.1+2、TLS、gzip/br/zstd、cookies、重定向、**Web Bot Auth 签名**）；`catpaw-fetch` 最小文档抓取；html5ever → arena DOM；UA 样式表 + Stylo restyle（用于 `display:none` 剪枝）；CST 快照 v0（HTML-AAM 子集 + ARIA 的 role/name）；`read` 的 text/markdown/links/forms；CLI `catpaw fetch <url> --snapshot|--markdown|--html`、`catpaw keygen` | `catpaw fetch https://example.com --snapshot` 输出 CST；HN 首页 markdown 可读；html5lib tree-construction 全过；带密钥的请求在 crawltest.com 得到 200 |
| **M1 scripts run** | Boa realms + Window（host hooks）；`xtask bindgen` 覆盖 DOM/Events/HTML 核心元素/XHR/fetch/URL/timers/console/Storage/MutationObserver；事件循环 + 虚拟时间；parser-script 交错（inline/external/defer/async/module、`document.write`）；`structuredClone`；CPU 预算；**最小 WebDriver classic**（WPT 需要）；`JsRuntime` 抽象定稿 | WPT `dom/`、`html/dom/`、`fetch/api/`、`xhr/` 子集按 expectations 通过；一个 Next.js 与一个 Vue 应用完成 hydration 并可交互（基准计时入 CI） |
| **M2 interact** | 布局（Taffy/Parley/表格/浮动）、几何 API、hit-test、指针/键盘事件与焦点模型、表单与提交、导航 + 会话历史 + pushState、iframe（同域/跨域、WindowProxy、postMessage、popup）、持久 localStorage、Intersection/ResizeObserver、截图、Canvas 2D、Web Crypto（RustCrypto）、WebSocket、Web Workers | 能在真实站点完成登录流程；Turnstile 演示页（自有站点嵌入）勾选框点击后 token 回调触发；截图可辨认 |
| **M3 agent API** | JSON-RPC/WS + stdio + MCP；CST diff、settled、动作后果、read 视图、logs、checkpoint v1、HAR 录制回放、TS/Python SDK | 一个 agent（例如 Claude Code）通过 `catpaw mcp --stdio` 完成 WebArena 自托管子集的任务；eval 在 HAR 回放 + 虚拟时间下可重复 |
| **M4 fidelity & challenges** | 挑战检测、hand-off 远程查看、自有 Cloudflare 测试 zone、BotBase 提交 + Browser Developer Program 申请、Intl、CSSOM 补全、WPT 覆盖推进 | 自有 zone 的 Managed/JS Challenge 通过率有数据；hand-off 端到端可用；WPT 通过数公开仪表板 |
| **M5 scale & compat** | 多租户限额、OpenTelemetry、Docker 镜像、CDP 子集（puppeteer-core 冒烟）、V8 后端在 CI 与 Boa 同等覆盖、BiDi 探索 | 单机 1000 context 压测内存/CPU 曲线；puppeteer-core 冒烟通过 |

## 风险清单（Top 10）

1. **Boa 吞吐**（无 JIT，热循环比 V8 慢 1–2 个数量级）→ `JsRuntime` 边界与双后端 codegen 从第一天起；M1 起 hydration 基准入 CI；原子字符串缓存、批量转换。
2. **Boa 缺口**（无跨线程中断、无 `JsProxy::target`、`Error.stack` 格式与 V8 不同）→ `[patch.crates-io]` 小 fork + 持续上游 PR；提供 V8 风格 `stack` 格式。
3. **Stylo 每月 breaking 发布与编译时间** → 锁定精确版本、季度升级 PR、lld/sccache、`catpaw-style` 是唯一接触 Stylo 的 crate。
4. **行内布局保真**（bidi、vertical-align、line-height、white-space、浮动交互）→ Parley 做整形/断行；以 CSS2 WPT 子集把关；接受近似。
5. **跨边界 GC 环与泄漏** → token 模型；CI 泄漏测试（`force_collect` + arena 普查）；每 group 堆/节点上限。
6. **WebIDL 覆盖量**（约 250 个接口）→ 生成胶水 + `[CatPawUnimplemented]` 计数桩，按爬取遥测排优先级。
7. **挑战脚本与指纹探测**（`navigator`、属性顺序、`toString` 输出、canvas 哈希）→ 诚实 UA、规范精确的属性顺序与格式、挑战页检测 + hand-off；不做 Chrome 伪装。**新引擎可能被评分为可疑并困在挑战循环**，这是官方文档明示的限制，靠 Signed Agent 与 Browser Developer Program 缓解。
8. **parser/script 交错 bug** → 照 Servo `ServoParser`，WPT `html/syntax`。
9. **网络顺序导致的非确定性** → 虚拟时间、带种子的平局打破、测试用 HAR 回放。
10. **进程内跨域隔离 bug** → 所有跨域访问汇聚到两套 proxy trap，fuzz + WPT `html/browsers/origin`；日后提供 process-per-group 模式。

## 验证方式

- **单元/集成**：`cargo test --workspace`；`cargo xtask html5lib`（tree-construction 与 tokenizer 测试）；`cargo xtask wpt --include <dirs>` 跑 WPT 子集，expectations 放 `tests/wpt/meta/*.ini`（Servo/Ladybird 做法），CI 分片；test262 由 Boa 上游覆盖，不自跑。
- **M0 端到端**：`cargo run -p catpaw -- fetch https://example.com --snapshot` 输出带 ref 的 CST；`--markdown` 对 HN 首页可读；`catpaw keygen` + `catpaw fetch https://crawltest.com/cdn-cgi/web-bot-auth --bot-auth-key <file> --signature-agent <url>` 返回 200（需把目录部署到该 URL，否则预期 401，CI 中作为 opt-in 网络测试）。
- **M1–M2**：Next.js/Vue 示例应用的 hydration 计时与交互脚本；真实站点登录流程脚本（凭证走本地 env，不入库）。
- **M3**：将 `catpaw mcp --stdio` 配成 MCP server，由一个 agent 完成 WebArena 自托管子集任务；HAR 回放 + 虚拟时间下两次运行逐字节一致。
- **M4**：自有 Cloudflare zone 的三条挑战规则各跑 N 次记录通过率；hand-off 由人工在浏览器里完成一次 Turnstile 勾选。
- **性能基线**：Lightpanda 的 BENCHMARKS.md 场景（本地电商页加载 100 次）对比内存/CPU；目标单页 < 50 MB、空闲 CPU 接近 0。
- **泄漏**：每个集成测试结束时 `force_collect` + arena 节点普查为 0。

## M0 落地步骤

1. 仓库骨架：`git init`；`Cargo.toml` workspace（resolver 3、edition 2024、`[workspace.dependencies]` 锁版本）；`rust-toolchain.toml`（stable）；`.github/workflows/ci.yml`（fmt/clippy/test，linux + windows + macos）；`LICENSE-APACHE`、`LICENSE-MIT`；`README.md` + `README.zh-CN.md`；`CONTRIBUTING.md`；`.gitignore`。
2. 文档：`docs/architecture.md`（本设计）；`docs/adr/0001-js-engine-boa-default.md`、`0002-dom-arena-and-wrapper-liveness.md`、`0003-identity-bot-auth-and-challenges.md`、`0004-webidl-codegen.md`、`0005-cst-snapshot-format.md`。
3. crate 骨架（全部先建空壳 + `lib.rs` 文档注释，保证 `cargo build --workspace` 通过）：`crates/catpaw-net`、`catpaw-fetch`、`catpaw-dom`、`catpaw-style`、`catpaw-text`、`catpaw-layout`、`catpaw-paint`、`catpaw-canvas`、`catpaw-js`、`catpaw-js-boa`、`catpaw-webidl`、`catpaw-bindings-boa`、`catpaw-web`、`catpaw-engine`、`catpaw-agent`、`catpaw-protocol`、`catpaw-server`、`catpaw`（bin）、`xtask`。
4. M0 实现顺序：
   - `catpaw-net`：hyper + rustls 客户端、重定向、解压、cookie jar（PSL）、`BotAuthSigner`（`web-bot-auth` crate）、`catpaw keygen`；
   - `catpaw-dom`：`arena.rs`（SlotMap、Node kinds、树操作）、`html/sink.rs`（html5ever `TreeSink`）、`serialize.rs`；
   - `catpaw-style`：`stylo_impl.rs`（traits）、`ua.css`、restyle 驱动，输出每元素 `display`/`visibility`；
   - `catpaw-agent`：`snapshot.rs`（CST v0：role/name 推断、`interesting` 过滤、RefTable）、`read.rs`（text/markdown/links/forms）；
   - `catpaw`：`fetch` 子命令；
   - `xtask`：`html5lib` 测试驱动、`wpt` 骨架（M1 启用）。
5. M0 验收即上文"验证方式"的 M0 条目。

## 关键文件（实现时承载核心决策）

- `crates/catpaw-dom/src/arena.rs`：SlotMap 节点存储、树变更遍历、token 钩子
- `crates/catpaw-bindings-boa/src/wrapper.rs`：TreeToken、wrapper 缓存、proxies、WindowProxy/Location trap
- `crates/catpaw-webidl/src/emit_boa.rs`：IDL → Boa 胶水 emitter、overload 解析、原型链搭建
- `crates/catpaw-web/src/event_loop.rs`：任务源、microtask checkpoint、timers、虚拟时钟、预算
- `crates/catpaw-style/src/stylo_impl.rs`：TNode/TElement/selectors::Element 实现、restyle 驱动
- `crates/catpaw-agent/src/snapshot.rs`：CST emitter、RefTable、过滤、cursor、diff
- `crates/catpaw-agent/src/actions.rs`：定位、actionability、受信事件序列、动作后果
- `crates/catpaw-engine/src/settle.rs`：调度器集成的 settled 谓词、后台 fetch/轮询分类器、pending 报告
- `crates/catpaw-net/src/bot_auth.rs`：RFC 9421 签名、密钥目录生成
- `crates/catpaw-server/src/handoff.rs`：挑战检测、一次性 token 远程查看协议、租约
- `crates/catpaw-protocol/protocol.json`：JSON-RPC / MCP / SDK 的唯一事实来源
