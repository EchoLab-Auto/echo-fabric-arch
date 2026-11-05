# echo-fabric-arch

**业务无关的 Rust 模块框架。** 一个模块契约，两种载体——进程内或独立子进程。

> 宿主不需要认识模块背后的业务，模块不需要认识宿主是谁。

由 EchoAgentCore 的模块机制抽出，业务词汇已抽干净——任何 Rust 应用都能直接用。

## 它解决什么

模块化的第一步通常是 `trait Module` + `HashMap<String, Box<dyn Module>>`。
半天能跑通，然后撞上五个问题：

| 问题 | 这里的答案 | crate |
|---|---|---|
| 卸载不干净 | **注册即副作用**：每次注册返回 `Disposer`，卸载时逆序撤销 | `echo-context` |
| 顺序靠猜 | **声明式依赖**：`Inject { required, optional }`，缺硬依赖即拒绝挂载 | `echo-module` |
| 一个崩溃全盘崩 | **进程外轨**：子进程 + supervisor（退避重启、熔断、`rlimit`） | `echo-module-host` |
| 测试只能真跑 | 装载/卸载是纯 API，可 `assert`；示例宿主本身就是证伪测试 | `examples/` |
| 语言被锁死 | **协议即 ABI**：4 字节长度前缀 + JSON，纯 Python 模块可直接跑 | `echo-module-api` |

## 五个概念

整个框架只有五个概念（外加一个反方向的 `HostService`），其余都是它们的组合：

| 概念 | 一句话 | crate |
|---|---|---|
| `Ctx` | 服务定位器：按**名字 + 类型**找服务，类型不符等于没找到 | `echo-context` |
| `Disposer` | 撤销句柄：`drop` 即撤销那次注册 | `echo-context` |
| `EventBus` | 事件总线：`Observe` / `Waterfall` / `Parallel` / `Serial` 四种分发 | `echo-context` |
| `Module` + `Registry` | 模块 = manifest + 一次挂载；注册表管 `register → mount → unmount` | `echo-module` |
| `ModuleScope` | 模块树：一层 = 一个 `Ctx` + 一个注册表，父链继承、子遮蔽父 | `echo-module` |
| `HostService` | 模块反向调宿主：`HostCall` 到宿主注册的服务，带不可伪造的 `caller` | `echo-module-host` |

读法：用 `Disposer` 保证可撤销，用 `Ctx` 表达依赖，用 `Scope` 表达归属，用
`EventBus` 表达通知，用运输层决定这些发生在进程内还是进程外。

## 两种形态

| | 进程内（`InprocTransport`） | 进程外（`StdioTransport`） |
|---|---|---|
| 模块是 | 编译进宿主的 Rust 对象 | 独立二进制，stdin/stdout |
| 调用开销 | 零序列化 | 每帧一次 JSON |
| 崩溃影响 | 带崩宿主 | 隔离；supervisor 退避重启 |
| 语言 | 只能 Rust | 任意语言 |
| 适合 | 同团队、同步发布、热路径 | 第三方、高权限、可能失控 |

## 跑起来

```bash
cargo test --workspace        # 236 项，全绿
cargo build -p minimal-module && cargo run -p minimal-host
# ✅ 全部断言通过——这套框架可以在非 EchoAgent 的宿主里独立使用
```

`minimal-host`（约 160 行）与 `minimal-module`（约 70 行）只依赖框架，不引入任何
业务 crate。那个 ✅ 不是装饰：示例里有 `assert!`，任何一步不符即非零退出。

## 写一个模块

```toml
echo-module-sdk = { git = "https://github.com/EchoLab-Auto/echo-fabric-arch", tag = "v0.15.0" }
```

```rust
use echo_module_sdk::{async_trait, serve_stdio, Contribution, InvokeOutcome,
                      InvokeRequest, ModuleHandler};

struct Greeter;

#[async_trait]
impl ModuleHandler for Greeter {
    fn contributions(&self) -> Vec<Contribution> { vec![] }
    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        let name = req.payload.get("name").and_then(|v| v.as_str()).unwrap_or("world");
        InvokeOutcome::ok(format!("hello, {name}"))
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = serve_stdio(Greeter).await {
        eprintln!("module exited: {e}");     // 日志一律走 stderr
        std::process::exit(1);
    }
}
```

两条硬规矩：**帧只走 stdout，日志必须走 stderr**（往 stdout 打一个字节就破坏协议）；
别自己写握手与分帧（SDK 已处理分段到达、单写者串行写出、`Cancel` / `Drain`）。

## 装进宿主

```rust
use echo_module_host::{stdio::StdioTransport, supervisor::ModuleSupervisor,
                       transport::ModuleSpec};

let handle = ModuleSupervisor::new()
    .start(ModuleSpec { module_id: "demo.greeter".into(), config: json!({}) },
           Box::new(StdioTransport::new("./target/debug/greeter")))
    .await?;                                     // 握手完成才返回，失败即 Err

handle.invoke("greeter", Default::default(), json!({ "name": "you" })).await;
handle.drain(Duration::from_secs(5)).await?;     // 优雅退出
```

`start()` 内部：spawn → `Hello` → 校验协议名与版本 → 收 `Register` → 等 `Ready`。
任何一步失败都返回 `Err`，不会给你一个半可用的句柄。`invoke` 与 `drain` 的完整
写法见[快速上手](./docs/getting-started.md)。

## 怎么组织一个应用

| 你写的 | 放哪 | 判据 |
|---|---|---|
| 业务逻辑 | 独立 crate，**零框架依赖** | 它要能被 CLI 直接调用吗 |
| 跨模块/跨进程的 DTO 与 trait | 契约 crate，只依赖 serde | 双方都要编译它吗 |
| 装配、注册、生命周期 | 宿主 `main.rs`（唯一组合根） | 只有它知道全局拓扑 |
| 配置、持久化、HTTP、日志 | 你选的库 | 框架不管这些 |

最容易踩的两条线：**能力层不 `use` 框架**（否则没框架的用法就死了）；
**别给未来的需求预留 trait**（框架自己有过一次零调用点的预留，后来全删）。

## 文档

- [`document/`](./document/index.md) — 12 篇设计文档（ProDoc 文档图），从「为什么」
  到「边界」；代码怎么摆看[组织一个应用](./document/structure.md)，模块怎么回调宿主
  看[宿主服务与回呼](./document/host-service.md)
- [`docs/getting-started.md`](./docs/getting-started.md) — 单篇速查：写模块、装模块、
  进程内、崩溃隔离、跨语言
- [`examples/`](./examples) — 三个可跑示例（Rust 宿主 / Rust 模块 / Python 模块）

crate 清单：`echo-context`（内核）、`echo-module`（生命周期）、`echo-module-api`
（冻结协议）、`echo-module-host`（宿主角）、`echo-module-sdk`（模块侧）、
`echo-module-loader`（分层配置）；另有 `framework-gates`，只含门禁测试。

## 版本与兼容

- 单一版本号（workspace 共享）。0.x 期间允许 breaking，逐条记入
  [CHANGELOG.md](./CHANGELOG.md)。
- `PROTOCOL_VERSION` 与 crate 版本**独立**：改线上帧格式才 bump 协议版本（握手
  fail-closed 拒绝旧端）；只改 Rust API 表面只 bump crate 版本。
- 新增字段一律 `#[serde(default)]`，两端可互换解码。消费方用 `tag = "v0.x.y"` 钉住。

## 不做什么

不定义业务（没有「工具」「会话」「人格」这类概念，`kind` 是开放字符串）、不管
持久化、不做权限模型（进程外只做资源隔离，进程内假定可信）、不提供容器（组合根
是你的 `main()`）。

什么时候**不该**用它：模块集合编译期固定、永不变动（一个 `enum` 分派就够）；只想
要依赖注入（单拿 `echo-context`）；要求模块与宿主共享复杂 Rust 类型（做不到）。

**协议零领域词汇**：贡献分类只剩与业务无关的形状（服务 / 事件 / 命令 /
文本块），领域宿主用 `Contribution::Custom { kind, payload }` 自带分类——框架
只搬运，不认识其中含义。2026-11 审计删掉了三个零使用的领域变体与三个聊天平台
类型，并把 `echo-module-api` / `echo-module-sdk` 一起纳入领域词门禁（此前它们被
排除，正是那份豁免让业务类型长了进来）。详见
[边界与纪律](./document/boundaries.md)。

## License

MIT（见 [LICENSE](./LICENSE)）。
