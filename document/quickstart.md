---
id: quickstart
title: 上手
group: 实践
---

# 上手

两条路：**写一个模块**（只依赖 `echo-module-sdk`），或者**把模块装进宿主**
（`echo-module-host`）。两种形态（进程内 / 进程外）都演示一遍。

仓库里有两个可跑的示例，本文给的是它们的最小版本：
`examples/minimal-module`（约 70 行）与 `examples/minimal-host`（约 130 行）。
后者同时是**可执行的证伪测试**——断言失败即非零退出。

## 一、写一个进程外模块

只加一行依赖：

```toml
[dependencies]
echo-module-sdk = { git = "https://github.com/EchoLab-Auto/echo-fabric-arch", tag = "v0.16.0" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

实现 `ModuleHandler`，交给 `serve_stdio`：

```rust
use echo_module_sdk::echo_module_api::ServiceContribution;
use echo_module_sdk::{async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, ModuleHandler};
use serde_json::{json, Value};

const SERVICE_KEY: &str = "greeter";

struct Greeter;

#[async_trait]
impl ModuleHandler for Greeter {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Service(ServiceContribution {
            key: SERVICE_KEY.to_string(),
            kind: "greeter".to_string(),   // 宿主自定义词汇，框架只透传
            version: 1,
        })]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        let name = req.payload.get("name").and_then(Value::as_str).unwrap_or("world");
        InvokeOutcome::ok(json!({ "greeting": format!("hello, {name}") }).to_string())
    }
}

#[tokio::main]
async fn main() {
    // 日志必须走 stderr —— stdout 是协议通道。
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    if let Err(e) = serve_stdio(Greeter).await {
        eprintln!("exited: {e}");
        std::process::exit(1);
    }
}
```

`serve_stdio` 负责帧读写、握手、并发调用、单写者、取消与生命周期。你写的是
`invoke` 一个方法。

## 二、把模块装进宿主（进程外）

```rust
use echo_module_host::stdio::StdioTransport;
use echo_module_host::supervisor::ModuleSupervisor;
use echo_module_host::transport::ModuleSpec;

let supervisor = ModuleSupervisor::new();
let handle = supervisor.start(
    ModuleSpec {
        module_id: "demo.greeter".to_string(),
        config: json!({ "note": "原样透传到模块的 Hello.config" }),
    },
    Box::new(StdioTransport::new("/path/to/greeter")),
).await?;

assert_eq!(handle.state(), ModuleState::Ready);
println!("贡献：{:?}", handle.contributions());
```

调用与收尾：

```rust
let out = handle.invoke("greeter", InvokeContext::default(), json!({ "name": "world" })).await;
handle.drain(Duration::from_secs(5)).await?;   // 优雅退出
```

`ModuleSupervisor` 默认就带：崩溃 backoff 重启（5 次熔断）、速率限额、
环境隔离。要加资源上限用 `StdioTransport::new(prog).with_limits(limits)`。

## 三、进程内模块（更轻，无序列化）

模块是实现 `Module` 的 Rust 对象，编译进宿主：

```rust
use echo_module::{BuiltinModule, MountContext, ModuleManifest, ModuleMountResult, ModuleRegistry};

let registry = ModuleRegistry::new();
registry.register(Arc::new(BuiltinModule::new(
    ModuleManifest::builtin(
        "demo.counter", "计数器", "1.0.0",
        "counter",              // kind：开放字符串，框架不解释
        "counter",
        "挂载时把计数器注册进 Ctx",
    ),
    |mctx: &MountContext| -> ModuleMountResult {
        let Some(ctx) = &mctx.ctx else { return Ok(vec![]) };
        let disposer = ctx.provide(&COUNTER, Arc::new(AtomicUsize::new(0)))
            .map_err(|e| echo_module::ModuleMountError::Mount(e.to_string()))?;
        Ok(vec![disposer])       // 卸载时自动撤销
    },
)))?;

registry.mount_all(&MountContext::new().with_ctx(ctx.clone()));
registry.unmount("demo.counter")?;   // Ctx 里的键随之消失
```

这段的要点是**挂载闭包返回 `Vec<Disposer>`**：它做的每件事都必须能撤销。

## 三·五、让模块反过来调宿主

大多数真实模块要读宿主的东西（配置、数据库、令牌）。宿主实现
`AsyncHostService` 并注册，模块用 `HostClient` 调：

```rust
// 宿主侧（必须在 supervisor.start() 之前注册）
use echo_module_host::async_trait;
use echo_module_host::host_service::{AsyncHostService, CallContext};

struct Storage;

#[async_trait]
impl AsyncHostService for Storage {
    async fn call(&self, cx: CallContext, method: &str, payload: Value)
        -> Result<Value, (String, String)>
    {
        // cx.caller = 发起本次回呼的模块 id（supervisor 填，模块无法伪造）
        Ok(json!({ "caller": cx.caller }))
    }
}

supervisor.host_services().register_async("my.storage", Arc::new(Storage));
```

```rust
// 模块侧：on_ready 存句柄，之后随便调（别在 on_ready 里直接 await，会自锁）
async fn on_ready(&self, host: HostClient) { *self.host.lock().unwrap() = Some(host); }
// …在 invoke 里…
let out = self.host.lock().unwrap().as_ref().unwrap()
    .call("my.storage", "load", json!({ "key": "k" })).await;
```

细节（两种实现、错误码、坑）见[宿主服务与回呼](./host-service.md)。

## 四、声明依赖与订阅事件（可选）

```rust
BuiltinModule::new(manifest, mount_fn)
    .with_inject(Inject::new(["loop"]).with_optional(["sanitizer"]))
    .with_subscriptions(["ResourceCreated"])          // 想收哪些事件
    .with_event_handler(|event| {                     // 收到时做什么
        println!("{}: {}", event.name, event.payload);
        Ok(())
    })
```

- 依赖缺失时**不会挂载**，`mount_fn` 一次都不执行；
- 想在依赖晚到时才挂，用 `registry.mount_when_ready(id, ctx, timeout)`；
- 依赖会变动（服务被注销/重注册）就周期性调 `registry.reconcile(&ctx)`。

## 五、跑起来

```bash
cargo build -p minimal-module && cargo run -p minimal-host
# ✅ 全部断言通过——这套框架可以在非 EchoAgent 的宿主里独立使用
```

跨语言也验过：

```bash
cargo test -p echo-module-host --test cross_language
```

它会 spawn 一个纯 Python 模块（`examples/minimal-module-py/module.py`，手写帧、
零 Rust 依赖），跑完握手 → 贡献 → 调用 → 排空全流程。

## 下一步

- 想做进程树 / 子模块 → [模块语义](./semantics.md) 的「树」一节
- 想配限额、限速、热替换 → [运输层](./transport.md)
- 想搞清"为什么这里用字符串键不用类型" → [边界与纪律](./boundaries.md)
