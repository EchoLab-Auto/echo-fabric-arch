# echo-fabric-arch

**业务无关的 Rust 插件框架。** 一个插件契约，两种载体——进程内或独立子进程。

> 宿主不需要认识插件背后的业务，插件不需要认识宿主是谁。

由 EchoAgentCore 的插件机制抽出，业务词汇已抽干净——任何 Rust 应用都能直接用。

## 它解决什么

插件化的第一步通常是 `trait Plugin` + `HashMap<String, Box<dyn Plugin>>`。
半天能跑通，然后撞上五个问题：

| 问题 | 这里的答案 | crate |
|---|---|---|
| 卸载不干净 | **注册即副作用**：每次注册返回 `Disposer`，卸载时逆序撤销 | `echo-context` |
| 顺序靠猜 | **声明式依赖**：`Inject { required, optional }`，缺硬依赖即拒绝挂载 | `echo-plugin` |
| 一个崩溃全盘崩 | **进程外轨**：子进程 + supervisor（退避重启、熔断、`rlimit`） | `echo-plugin-host` |
| 测试只能真跑 | 装载/卸载是纯 API，可 `assert`；示例宿主本身就是证伪测试 | `examples/` |
| 语言被锁死 | **协议即 ABI**：4 字节长度前缀 + JSON，纯 Python 插件可直接跑 | `echo-plugin-api` |

## 五个概念

整个框架只有五个概念，其余都是它们的组合：

| 概念 | 一句话 | crate |
|---|---|---|
| `Ctx` | 服务定位器：按**名字 + 类型**找服务，类型不符等于没找到 | `echo-context` |
| `Disposer` | 撤销句柄：`drop` 即撤销那次注册 | `echo-context` |
| `EventBus` | 事件总线：`Observe` / `Waterfall` / `Parallel` / `Serial` 四种分发 | `echo-context` |
| `Plugin` + `Registry` | 插件 = manifest + 一次挂载；注册表管 `register → mount → unmount` | `echo-plugin` |
| `PluginScope` | 插件树：一层 = 一个 `Ctx` + 一个注册表，父链继承、子遮蔽父 | `echo-plugin` |

读法：用 `Disposer` 保证可撤销，用 `Ctx` 表达依赖，用 `Scope` 表达归属，用
`EventBus` 表达通知，用运输层决定这些发生在进程内还是进程外。

## 两种形态

| | 进程内（`InprocTransport`） | 进程外（`StdioTransport`） |
|---|---|---|
| 插件是 | 编译进宿主的 Rust 对象 | 独立二进制，stdin/stdout |
| 调用开销 | 零序列化 | 每帧一次 JSON |
| 崩溃影响 | 带崩宿主 | 隔离；supervisor 退避重启 |
| 语言 | 只能 Rust | 任意语言 |
| 适合 | 同团队、同步发布、热路径 | 第三方、高权限、可能失控 |

## 跑起来

```bash
cargo test --workspace        # 222 项，全绿
cargo build -p minimal-plugin && cargo run -p minimal-host
# ✅ 全部断言通过——这套框架可以在非 EchoAgent 的宿主里独立使用
```

`minimal-host`（约 160 行）与 `minimal-plugin`（约 70 行）只依赖框架，不引入任何
业务 crate。那个 ✅ 不是装饰：示例里有 `assert!`，任何一步不符即非零退出。

## 写一个插件

```toml
echo-plugin-sdk = { git = "https://github.com/EchoLab-Auto/echo-fabric-arch", tag = "v0.11.2" }
```

```rust
use echo_plugin_sdk::{async_trait, serve_stdio, Contribution, InvokeOutcome,
                      InvokeRequest, PluginHandler};

struct Greeter;

#[async_trait]
impl PluginHandler for Greeter {
    fn contributions(&self) -> Vec<Contribution> { vec![] }
    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        let name = req.payload.get("name").and_then(|v| v.as_str()).unwrap_or("world");
        InvokeOutcome::ok(format!("hello, {name}"))
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = serve_stdio(Greeter).await {
        eprintln!("plugin exited: {e}");     // 日志一律走 stderr
        std::process::exit(1);
    }
}
```

两条硬规矩：**帧只走 stdout，日志必须走 stderr**（往 stdout 打一个字节就破坏协议）；
别自己写握手与分帧（SDK 已处理分段到达、单写者串行写出、`Cancel` / `Drain`）。

## 装进宿主

```rust
use echo_plugin_host::{stdio::StdioTransport, supervisor::PluginSupervisor,
                       transport::PluginSpec};

let handle = PluginSupervisor::new()
    .start(PluginSpec { plugin_id: "demo.greeter".into(), config: json!({}) },
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
| 跨插件/跨进程的 DTO 与 trait | 契约 crate，只依赖 serde | 双方都要编译它吗 |
| 装配、注册、生命周期 | 宿主 `main.rs`（唯一组合根） | 只有它知道全局拓扑 |
| 配置、持久化、HTTP、日志 | 你选的库 | 框架不管这些 |

最容易踩的两条线：**能力层不 `use` 框架**（否则没框架的用法就死了）；
**别给未来的需求预留 trait**（框架自己有过一次零调用点的预留，后来全删）。

## 文档

- [`document/`](./document/index.md) — 11 篇设计文档（ProDoc 文档图），从「为什么」
  到「边界」；代码怎么摆看[组织一个应用](./document/structure.md)
- [`docs/getting-started.md`](./docs/getting-started.md) — 单篇速查：写插件、装插件、
  进程内、崩溃隔离、跨语言
- [`examples/`](./examples) — 三个可跑示例（Rust 宿主 / Rust 插件 / Python 插件）

crate 清单：`echo-context`（内核）、`echo-plugin`（生命周期）、`echo-plugin-api`
（冻结协议）、`echo-plugin-host`（宿主角）、`echo-plugin-sdk`（插件侧）、
`echo-plugin-loader`（分层配置）；另有 `framework-gates`，只含门禁测试。

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

什么时候**不该**用它：插件集合编译期固定、永不变动（一个 `enum` 分派就够）；只想
要依赖注入（单拿 `echo-context`）；要求插件与宿主共享复杂 Rust 类型（做不到）。

**已知缺口**：协议表面仍带 `Tool` / `Skill` / `PluginInstance` 三个领域分类（历史
遗留），去业务化需 2 个以上真实外部使用者才动手。

## License

MIT（见 [LICENSE](./LICENSE)）。
