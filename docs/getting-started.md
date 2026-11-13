# 快速上手（第三方）

这套框架让你在自己的 Rust 应用里加一套模块系统：模块可以是**同进程的类型直连**
（零序列化），也可以是**独立子进程**（崩溃隔离、跨语言）。本页是从零到跑通的
最短路径。

> 只想看代码？[`examples/`](../examples) 里有一个 160 行的宿主 + 70 行的模块，
> 跑 `cargo build -p minimal-module && cargo run -p minimal-host` 就能看到全流程。

## 0. 先记住三个概念

| 概念 | 是什么 | crate |
|---|---|---|
| **Ctx** | 服务定位器。宿主往里注册「这个进程里有哪些能力」，模块按名字取用 | `echo-context` |
| **Module** | 模块 = manifest + 一次挂载。挂载是**副作用**，返回值是撤销句柄 `Disposer` | `echo-module` |
| **Transport** | 模块实例怎么跑：同进程（inproc）还是子进程（stdio 帧协议） | `echo-module-host` |

另外两个是**可选**的：`EventBus`（事件通知）与 `ModuleScope`（模块树，让子模块随
父模块级联卸载）。用不到就不碰。五个概念的全景见
[核心模型](../document/model.md)。

没有「工具」「会话」「人格」这类概念——框架不认识它们。宿主自己定义词汇。

## 1. 写一个模块（10 分钟）

```toml
[dependencies]
echo-module-sdk = { git = "https://github.com/EchoLab-Auto/echo-fabric-arch", tag = "v0.17.0" }
tokio = { version = "1", features = ["full"] }
```

```rust
use echo_module_sdk::echo_module_api::ServiceContribution;
use echo_module_sdk::{
    async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, ModuleHandler,
};

struct Greeter;

#[async_trait]
impl ModuleHandler for Greeter {
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
        eprintln!("module exited: {e}");
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
echo-context = { git = "…", tag = "v0.17.0" }
echo-module-host = { git = "…", tag = "v0.17.0" }
```

```rust
use echo_module_host::{stdio::StdioTransport, supervisor::ModuleSupervisor,
                       transport::ModuleSpec};
use serde_json::json;

let supervisor = ModuleSupervisor::new();
let handle = supervisor
    .start(
        ModuleSpec { module_id: "demo.greeter".into(), config: json!({}) },
        Box::new(StdioTransport::new("./target/debug/minimal-module")),
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

## 3. 同进程模块（更轻，无序列化）

不需要子进程时，实现 `Module` trait 交给 `ModuleRegistry`：

```rust
use echo_module::{BuiltinModule, MountContext, ModuleManifest, ModuleRegistry};

let registry = ModuleRegistry::default();
registry.register(std::sync::Arc::new(BuiltinModule::new(
    ModuleManifest::builtin("demo.x", "X", "1.0.0", "x", "x", "演示"),
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

子进程轨天然隔离：模块 panic / OOM / segfault 只死自己。supervisor 观察到
`TransportError::Closed` 后按策略重启（次数上限 + 指数退避），`ModuleHandle::state()`
可观察 `Failed` / `Restarting`；连续失败不再重启并保留错误原因。

还可以给子进程设资源限额（Unix）：

```rust
use echo_module_host::stdio::{ResourceLimits, StdioTransport};
let transport = StdioTransport::new("./module")
    .with_limits(ResourceLimits { cpu_secs: Some(30), ..Default::default() });
```

## 5. 跨语言模块

协议就是 ABI：**4 字节小端 `u32` 长度前缀 + JSON**，走 stdin/stdout。任何语言写个
循环就能当模块，不需要 Rust、不需要 SDK（`PROTOCOL_NAME` = `echo-module`，
`PROTOCOL_VERSION` = 1）。

这不是说法而是能跑的事实：[`examples/minimal-module-py/module.py`](../examples/minimal-module-py/module.py)
是一个纯 Python 模块，约 170 行、零 Rust 依赖，文件头把线上协议完整写了一遍
（帧格式 / 握手顺序 / 消息形状 / 生命周期），可直接当别的语言的参考实现。
`cargo test -p echo-module-host --test cross_language` 会真的 spawn 它并验证
握手、贡献解析、调用往返、版本不兼容拒绝。

这是这套框架相对 Node 生态模块框架的一个实质优势：**不锁定语言**——宿主是 Rust，
模块可以是 Python / Go / Node / shell，因为边界只有管道上的字节。

## 版本与兼容

- 本仓是**单一版本**（workspace 共享）。用 `tag = "v0.17.0"` 钉住，`Cargo.lock`
  会进一步钉死 rev。
- `PROTOCOL_VERSION` 与 crate 版本**独立**：改线上帧格式才 bump 协议版本（握手会
  fail-closed 拒绝旧端）；只改 Rust API 表面只 bump crate 版本。
- 新增字段一律 `#[serde(default)]`，两端可互换解码。

## 已知边界

- `Contribution` 的四个分类（服务 / 事件 / 命令 / 文本块）都与业务无关。领域
  想法走 `Contribution::Custom { kind, payload }`：`kind` 是**你自己的词汇**，
  框架只搬运、不解释。2026-11 已删除此前的三个领域分类与三个聊天平台类型。
- 进程级单例表**不在**框架里（那是宿主机制）。要「这个进程只有一份」，用 `Ctx`
  显式注入；它天然支持多份互不干扰，同一进程能起两套宿主。

## 接着读

- 代码怎么分层、什么该放哪 → [组织一个应用](../document/structure.md)
- 五个概念与其组合 → [核心模型](../document/model.md)
- 为什么这么设计、什么时候不该用 → [为什么需要它](../document/why.md)
