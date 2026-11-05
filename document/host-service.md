---
id: host-service
title: 宿主服务与回呼
group: 概念
link: ["semantics | 模块语义", "lifecycle | 生命周期", "structure | 组织一个应用"]
---

# 宿主服务与回呼

模块不能只被调用，它还要**反过来用宿主的能力**：读配置、查数据库、写日志、
拿认证令牌。这个反方向的通道就是 `HostCall`，宿主侧的实现叫**宿主服务**。

它是进程外模块**唯一**能触达宿主的方式——没有共享内存、没有动态库符号、没有
Rust 类型。也正因如此，它是宿主唯一的**授权点**：所有"这个模块能做什么"的
判断都落在这里。

## 一条调用链

```prodoc-flow
graph LR
  P["模块<br/>host.call(service, method, payload)"] -->|HostCall 帧| S["supervisor<br/>填 caller"]
  S --> R["HostServiceRegistry<br/>按名字查表"]
  R --> A["AsyncHostService<br/>直接 await"]
  R --> B["HostService（同步）<br/>挪到阻塞池"]
  A --> Out["结果 → HostCallResult 帧"]
  B --> Out
  Out --> P
```

链上有两件事值得记住：

1. **`caller` 是 supervisor 填的**，不是模块自报的（见下文「身份」）；
2. **回执按来源连接路由**：热替换瞬间，旧实例的调用不会错投到新实例。

## 两种实现

宿主能力通常是 async 的（数据库、HTTP、其他服务），所以推荐异步版；同步版保留
给遗留代码和明确的阻塞 IO。

```rust
use echo_module_host::async_trait;
use echo_module_host::host_service::{AsyncHostService, CallContext};

struct Storage { pool: PgPool }

#[async_trait]
impl AsyncHostService for Storage {
    async fn call(
        &self,
        cx: CallContext,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, (String, String)> {
        match method {
            "get" => {
                let key = payload["key"].as_str().unwrap_or_default();
                let row = self.pool.get(key).await.map_err(|e| ("unavailable".into(), e.to_string()))?;
                Ok(serde_json::json!({ "value": row }))
            }
            other => Err(("method_not_found".into(), format!("未知方法 {other}"))),
        }
    }
}
```

```rust
// 同步版：签名里没有 caller；调用时框架把它挪到 spawn_blocking 线程池。
impl echo_module_host::host_service::HostService for Legacy {
    fn call(&self, method: &str, payload: serde_json::Value)
        -> Result<serde_json::Value, (String, String)) { /* ... */ }
}
```

| | `AsyncHostService`（推荐） | `HostService`（同步） |
|---|---|---|
| 拿到 `caller` | 有 | 没有 |
| 执行方式 | 直接在调用任务里 await | 挪到 `spawn_blocking` 线程池 |
| 适合 | 宿主能力是 async 的（绝大多数） | 遗留实现；明确的阻塞 IO |
| 代价 | — | 一次线程切换；拿不到调用者身份 |

同步实现被**挪出** async worker 是有意的：一个做阻塞 IO 的同步服务若内联执行，
会占住 tokio 的 worker 线程，连累同一 worker 上所有模块的调用。

## 注册与调用时序

```rust
let supervisor = ModuleSupervisor::new();
// 必须在 start() 之前：模块在 Ready 之后随时可能回呼。
supervisor.host_services().register_async("my.storage", Arc::new(Storage { pool }));

let handle = supervisor.start(spec, Box::new(StdioTransport::new(program))).await?;
```

顺序不能反。`start()` 返回时模块已经 `Ready`，它完全可以在第一个 `invoke` 里
立刻回呼——晚注册的服务在那一瞬间就是"不存在"。

## 身份：`caller` 不可伪造

```rust
#[derive(Debug, Clone)]
pub struct CallContext {
    pub caller: String,   // 发起本次回呼的模块 id
}
```

它由 **supervisor 从连接归属推导**：

- supervisor 一定知道自己在为哪个 `module_id` 跑 serve 循环；
- **线上协议里没有这个字段**，所以模块写不了它。

于是宿主服务可以把它当成授权依据：

```rust
if cx.caller != "trusted.panel" {
    return Err(("forbidden".into(), format!("{} 无权重启服务", cx.caller)));
}
```

这在多模块宿主里是必需品，不是锦上添花：没有它，"谁能调哪个方法"就只能靠
"我们只有自己写的模块"这种部署上的巧合。

## 错误契约

`Err((code, message))` 里的 `code` 是**机器可读**的，模块与宿主都靠它分支。
框架自己会产出两个：

| code | 含义 |
|---|---|
| `service_not_found` | 注册表里没有这个名字（拼错 / 该功能没启用） |
| `method_not_found` | 服务在，但方法不认识（**由各实现自行返回**，注册表不代查） |

约定的其它常见码：`forbidden`（来源不允许）、`invalid`（参数错）、
`unavailable`（依赖方暂时不可用）、`rate_limited`（超出限额，请退避重试）。

`message` 是给人的，**不要放凭据**：它会一路进日志与面板。

## 三个坑

**在 `on_ready` 里直接回呼会自锁。** `on_ready` 在消息循环开始**前**被 await；
在里面直接 `await` 一次回呼，要等读循环跑起来才可能有结果——而它还没跑。
正确做法是只把 `HostClient` 存起来，之后在 `invoke` 里用：

```rust
async fn on_ready(&self, host: HostClient) {
    *self.host.lock().unwrap() = Some(host);   // 只保存，不调用
}
```

**宿主服务的名字是宿主词汇，模块只认字符串。** 框架不校验、不解释，也不提供
"服务目录"。两边对不上时错误是运行期 `service_not_found`——这是「名字 + 存在性」
边界的代价，补偿是错误消息里带着调用方给的名字（一眼能看出拼错）。

**别把重活放在同步实现里就以为万事大吉。** 它确实不占 async worker 了，但阻塞池
也是有限的；一个跑几分钟的同步服务会拖慢所有同步服务。

## 与模块侧 API 的关系

| 谁 | 用什么 | 位置 |
|---|---|---|
| 宿主 | `AsyncHostService` / `HostService` + `register_*` | `echo-module-host` |
| 模块 | `HostClient::call(service, method, payload)` | `echo-module-sdk` |
| 线上 | `ModuleToHost::HostCall` / `HostToModule::HostCallResult` | `echo-module-api` |

模块侧不需要知道宿主怎么实现，宿主也不需要知道模块是什么语言写的——中间只有
一条 JSON 帧。

## 最小可跑的例子

`examples/minimal-host` 注册了一个 `demo.identity` 服务，`examples/minimal-module`
在 `invoke` 里回呼它，并把宿主返回的 `caller` 一起带回给宿主：

```
[4] Invoke(greeter) → Ok { result: {
      "greeting": "hello, echo-fabric-arch",
      "host": "minimal-host",
      "host_caller": "demo.greeter"     ← supervisor 填的调用者 id
    } }
```

那个 `host_caller` 就是「身份不可伪造」的可执行证据：它出现在模块的返回值里，
却是宿主自己写进去的。
