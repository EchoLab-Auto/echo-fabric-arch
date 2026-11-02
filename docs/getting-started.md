# 快速上手（第三方）

这套框架让你在自己的 Rust 应用里加一套插件系统：插件可以是**同进程的类型直连**
（零序列化），也可以是**独立子进程**（崩溃隔离、跨语言）。本页是从零到跑通的
最短路径。

> 只想看代码？[`examples/`](../examples) 里有一个 130 行的宿主 + 70 行的插件，
> 跑 `cargo build -p minimal-plugin && cargo run -p minimal-host` 就能看到全流程。

## 0. 概念只有三个

| 概念 | 是什么 | crate |
|---|---|---|
| **Ctx** | 服务定位器。宿主往里注册「这个进程里有哪些能力」，插件按名字取用 | `echo-context` |
| **Plugin** | 插件 = manifest + 一次挂载。挂载是**副作用**，返回值是撤销句柄 `Disposer` | `echo-plugin` |
| **Transport** | 插件实例怎么跑：同进程（inproc）还是子进程（stdio 帧协议） | `echo-plugin-host` |

没有「工具」「会话」「人格」这类概念——框架不认识它们。宿主自己定义词汇。

## 1. 写一个插件（10 分钟）

```toml
[dependencies]
echo-plugin-sdk = { git = "https://github.com/EchoLab-Auto/plugin-framework", tag = "v0.11.2" }
tokio = { version = "1", features = ["full"] }
```

```rust
use echo_plugin_sdk::echo_plugin_api::ServiceContribution;
use echo_plugin_sdk::{
    async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, PluginHandler,
};

struct Greeter;

#[async_trait]
impl PluginHandler for Greeter {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Service(ServiceContribution {
            key: "greeter".into(),
            kind: "greeter".into(),
            version: 1,
        })]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        let name = req.payload.get("name").and_then(|v| v.as_str()).unwrap_or("world");
        InvokeOutcome::ok(format!("hello, {name}"))
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = serve_stdio(Greeter).await {
        eprintln!("plugin exited: {e}");
        std::process::exit(1);
    }
}
```

三条硬规矩：

1. **帧只走 stdout，日志必须走 stderr**。往 stdout 打一个字节就破坏协议。
2. `contributions()` 是**全量替换**语义：每次 `Register` 上报的是完整集合。
3. 别自己写握手/帧。SDK 已经处理了分段到达的读、单写者串行写出、`Cancel`/
   `Drain`/`Dispose` 生命周期——自己写一遍只会引入微妙的帧交错 bug。

## 2. 宿主侧拉起来

```toml
[dependencies]
echo-context = { git = "…", tag = "v0.11.2" }
echo-plugin-host = { git = "…", tag = "v0.11.2" }
```

```rust
use echo_plugin_host::{stdio::StdioTransport, supervisor::PluginSupervisor,
                       transport::PluginSpec};
use serde_json::json;

let supervisor = PluginSupervisor::new();
let handle = supervisor
    .start(
        PluginSpec { plugin_id: "demo.greeter".into(), config: json!({}) },
        Box::new(StdioTransport::new("./target/debug/minimal-plugin")),
    )
    .await?;                                  // ← 握手完成才返回

let out = handle
    .invoke("greeter", Default::default(), json!({ "name": "you" }))
    .await;
println!("{out:?}");
handle.drain(std::time::Duration::from_secs(5)).await?;
```

`start()` 内部做的事：spawn 子进程 → `Hello` → 校验协议名与版本 → 收 `Register`
（贡献）→ 等 `Ready`。任何一步失败都返回 `Err`，**不会**给你一个半可用的句柄。

## 3. 同进程插件（更轻，无序列化）

不需要子进程时，实现 `Plugin` trait 交给 `PluginRegistry`：

```rust
use echo_plugin::{BuiltinPlugin, MountContext, PluginManifest, PluginRegistry};

let registry = PluginRegistry::default();
registry.register(std::sync::Arc::new(BuiltinPlugin::new(
    PluginManifest::builtin("demo.x", "X", "1.0.0", "x", "x", "演示"),
    |ctx: &MountContext| {
        // ctx.ctx 是 Ctx：注册能力、拿服务。返回 Disposer 即撤销句柄。
        Ok(vec![])
    },
)))?;
registry.mount_all(&MountContext::new().with_ctx(services.clone()));
```

`mount` 返回 `Vec<Disposer>`——卸载时逐个执行。这就是「注册即副作用」的全部约定，
不需要 `unmount` 里手写反向操作。

## 4. 崩溃隔离

子进程轨天然隔离：插件 panic / OOM / segfault 只死自己。supervisor 观察到
`TransportError::Closed` 后按策略重启（次数上限 + 指数退避），`PluginHandle::state()`
可观察 `Failed` / `Restarting`；连续失败不再重启并保留错误原因。

还可以给子进程设资源限额（Unix）：

```rust
use echo_plugin_host::stdio::{ResourceLimits, StdioTransport};
let transport = StdioTransport::new("./plugin")
    .with_limits(ResourceLimits { cpu_secs: Some(30), ..Default::default() });
```

## 5. 跨语言插件

协议就是 ABI：**4 字节小端 `u32` 长度前缀 + JSON**，走 stdin/stdout。任何语言写个
循环就能当插件，不需要 Rust、不需要 SDK（`PROTOCOL_NAME` = `echo-plugin`，
`PROTOCOL_VERSION` = 1）。

这不是说法而是能跑的事实：[`examples/minimal-plugin-py/plugin.py`](../examples/minimal-plugin-py/plugin.py)
是一个纯 Python 插件，约 150 行、零 Rust 依赖，文件头把线上协议完整写了一遍
（帧格式 / 握手顺序 / 消息形状 / 生命周期），可直接当别的语言的参考实现。
`cargo test -p echo-plugin-host --test cross_language` 会真的 spawn 它并验证
握手、贡献解析、调用往返、版本不兼容拒绝。

这是这套框架相对 Node 生态插件框架的一个实质优势：**不锁定语言**——宿主是 Rust，
插件可以是 Python / Go / Node / shell，因为边界只有管道上的字节。

## 版本与兼容

- 本仓是**单一版本**（workspace 共享）。用 `tag = "v0.11.2"` 钉住，`Cargo.lock`
  会进一步钉死 rev。
- `PROTOCOL_VERSION` 与 crate 版本**独立**：改线上帧格式才 bump 协议版本（握手会
  fail-closed 拒绝旧端）；只改 Rust API 表面只 bump crate 版本。
- 新增字段一律 `#[serde(default)]`，两端可互换解码。

## 已知边界

- `Contribution` 目前仍带 `Tool` / `Skill` / `PluginInstance` 三个**应用领域**
  分类（历史遗留）。机制上它们是通用的（宿主怎么解释随你），但协议表面承认了这些
  词汇——去业务化排在 M5，需要 2 个以上真实外部使用者才动手。
- 进程级单例表**不在**框架里（那是宿主机制）。要「这个进程只有一份」，用 `Ctx`
  显式注入；它天然支持多份互不干扰，同一进程能起两套宿主。
