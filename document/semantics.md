---
id: semantics
title: 模块语义
group: 概念
link: ["lifecycle | 生命周期", "transport | 运输层", "config | 配置组合"]
---

# 模块语义

模块与宿主之间有四类约定。它们都是**声明式**的：模块说"我需要什么 / 我会对
什么有反应"，宿主决定怎么满足、什么时候调用。

## 一、依赖：`Inject`

```rust
BuiltinModule::new(manifest, mount_fn)
    .with_inject(Inject::new(["loop"]).with_optional(["sanitizer"]))
```

- `required`：挂载前必须在 `Ctx` 里，**缺一个就拒绝挂载，mount 闭包不执行**；
- `optional`：只作声明（面板展示 / 宿主按需读），缺失不影响挂载。

三种消费方式，语义不同：

| 方法 | 时机 | 缺失时 |
|---|---|---|
| `mount` | 立即 | 报 `MissingService{key, available}` 并失败 |
| `mount_when_ready` | 先等（有超时）再挂 | 超时后失败，记进 `pending` |
| `reconcile` | 依赖变动后 | 该挂的挂上、该卸的卸下 |

**`reconcile` 只看事实不看意图**：它只处理"因为缺依赖没挂上、现在依赖齐了"的
模块，以及"已经挂上、现在依赖没了"的模块。人主动禁用的、从没挂过也没 pending
记录的，一律不动——否则一次服务注册就会把用户的手动决定冲掉。

## 二、配置：`MountContext.config`

```rust
move |mctx| {
    let addr = mctx.config_str("address").unwrap_or("127.0.0.1:3132");
    // ...
}
```

配置由宿主按 **id** 分发，缺省是 `Value::Null` 而不是 `{}`——模块要能区分
"没配"与"配了空表"。框架不解析内容、不做 schema 校验，那是宿主与模块之间的契约。

## 三、事件：双向

| 方向 | API | 语义 |
|---|---|---|
| 模块 → 宿主 | `mctx.emit(name, payload)` | 即发即走、无返回值；没有出口时静默丢弃 |
| 宿主 → 模块 | `subscribed_events()` + `on_event()` + `deliver_event()` | 按名分发，`"*"` 全收 |

两条重要边界：

- **只投给已挂载的模块**。没挂载就没有副作用，也就不该收到事件。
- **`on_event` 返回 `Err` 只计数不卸载**。事件是观察通道，不是控制通道——一个
  模块处理不了某条通知，不该因此被踢掉。

来源 id 由**宿主**填（挂载时注入），模块不必知道自己的 id。

## 四、树：`ModuleScope`

模块可以在自己的 `mount` 里建子作用域、把子模块挂进去：

```rust
move |mctx| {
    let scope = mctx.nest().expect("有树把手");
    scope.mount(child_module)?;
    Ok(vec![])
}
```

- 子作用域的 `Ctx` 是父的 `child()`（父链继承、子遮蔽父、子注册对父不可见）；
- 卸载父模块时，它名下的子作用域**递归卸载**，子先于父；
- 没有树把手的裸注册表：`nest()` 返回 `None`，挂载照常成功——**树是可选能力**。

## 反方向：模块调用宿主

上面四条都是宿主 → 模块。反方向（模块 → 宿主）走 `HostCall` + 宿主服务，
单独成篇：[宿主服务与回呼](./host-service.md)。

## 为什么这四类都是"名字 + 存在性"

依赖是键名，配置是 id，事件是名字，作用域是父链。四处都没有跨模块的 Rust 类型：

- 要类型 → 模块之间就互相 `use`，不再可独立替换；
- 要类型 → 框架就得认识宿主的 trait，零领域依赖的边界立刻破。

代价很清楚：**拼错的名字不会编译报错**。补偿手段是运行期的可读错误
（`MissingService` 带 `available` 列表、`pending_inject()` 能查、面板能看到
"它要什么 / 现在缺什么"）。
