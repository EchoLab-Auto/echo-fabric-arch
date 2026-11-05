---
id: transport
title: 运输层
group: 概念
---

# 运输层

同一个插件契约，两种载体。换载体不改插件代码——这是"协议即 ABI"的实际含义。

## 进程内（`InprocTransport`）

插件是实现 `Plugin` 的 Rust 对象，编译进宿主二进制。注册是函数调用，参数与
返回值**不序列化**。

- 优点：零序列化、可以直接传 trait object、调试时栈是连续的；
- 缺点：插件 panic 会带崩宿主；插件与宿主共用 Rust 版本和 crate 版本。

## 进程外（`StdioTransport`）

插件是独立二进制，通过 stdin/stdout 通信。

- 优点：崩溃隔离、语言无关、可以单独升级、能加资源限额；
- 缺点：每次调用要序列化；插件拿不到宿主的 Rust 类型。

## 帧格式

```
[4 字节小端长度][JSON 载荷]
```

长度前缀而不是换行分帧——JSON 里可以合法出现换行，按行切会碎。

单帧上限 16 MiB。**日志必须写 stderr**：stdout 是协议通道，混进任何字节都会
破坏帧（这是新插件最常见的第一个 bug）。

## 握手顺序

```prodoc-flow
graph LR
  H1[宿主: Hello<br/>version + config] --> P1[插件: Welcome<br/>id + version + capabilities]
  P1 --> P2[插件: Register<br/>贡献清单]
  P2 --> Ready[就绪：可 Invoke]
  Ready --> Inv[Invoke / Cancel / HostCall]
  Ready --> Drain[Drain → Dispose]
```

两步校验都是 **fail-closed**：

- 协议名或版本不兼容 → 直接拒绝，不做"先连上再试试"；
- `Register` 里声明的贡献之外的东西，宿主调用时返回结构化错误，不静默放行。

`HostCall`（插件回呼宿主服务）是这条通道上的反方向消息，单独成篇：
[宿主服务与回呼](./host-service.md)。

## 进程外轨的治理项

| 项 | 缺省 | 说明 |
|---|---|---|
| 崩溃重启 | on-failure | backoff 200ms 起、×2、上限 2s、5 次后熔断 |
| 资源限额 | **不限制** | `rlimit`：CPU / 地址空间 / 单文件 / fd / 进程数。0 会被拒绝（`setrlimit` 里 0 = 完全禁止） |
| 速率限额 | Emit 2000/s、HostCall 500/s、Log 200/s | 只限"投递即忘"的帧；`InvokeResult` **绝不丢** |
| 环境 | `env_clear` + 仅 `PATH` + 显式 env | 不继承宿主的全部环境变量 |
| 排空 | 卸载先 `Drain`，超时再杀 | 在途调用以 `plugin_crashed` 收尾 |

## 什么时候该用哪个

| 场景 | 建议 |
|---|---|
| 插件由同一团队维护、与宿主同步发布 | 进程内 |
| 第三方插件 / 语言不同 / 可能失控 | 进程外 |
| 每个 token 都要走的热路径 | 进程内（跨进程序列化会吃掉收益） |
| 需要在运行期单独升级插件 | 进程外（`upgrade` 原子切流量） |

还有两条可选轨（**当前未实现**，`EntryKind` 里留了位置）：`dylib`（实验性，
Rust 没有稳定 ABI）与 `wasm`（沙箱轨）。装载器能解析这两类条目，但框架没有
对应的运输层实现——宿主拿到这样的条目只能明确拒绝，不能假装它能起来。
