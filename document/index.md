---
id: index
title: echo-fabric-arch
link: ["why | 为什么需要它", "quickstart | 上手", "boundaries | 边界与纪律"]
---

# echo-fabric-arch

一套**业务无关的 Rust 插件机制**：插件怎么声明"我能提供什么"、宿主怎么装载
与撤销、进程内的插件与独立进程的插件用同一套语义。

它对外只有一个承诺：**宿主不需要认识插件背后的业务，插件不需要认识宿主是谁。**
其余内容（有哪些服务、什么是"工具"、什么算一次"调用"）全部由使用者定义。

## 三句话理解它

**第一句：它是一套"注册即可撤销"的机制。** 插件挂载时做的每件事都返回一个
撤销句柄（`Disposer`），卸载时逐个撤销。这条约束推着整套设计往前走——不做成
可撤销的副作用，就没法安全地启停、热替换、崩溃后重来。

**第二句：它同时管"进程内"和"进程外"。** 同一个插件契约，既可以编译进宿主
（`inproc`，零序列化），也可以做成独立二进制经 stdio 通信（`subprocess`，
崩溃隔离）。上层的 `Plugin` / `PluginRegistry` / 依赖声明 / 事件通道对两种形态
是同一套 API——换运输层不改插件代码。

**第三句：它不认识任何应用词汇。** 没有 `tool` / `session` / `agent` / `user`
这些概念；`PluginManifest.kind` 是**开放字符串**，服务键是**名字 + 存在性**。
这条边界由仓库内的门禁（`crates/framework-gates`）守着，不是自觉遵守。

## 它能做什么

| 能力 | 落点 |
|---|---|
| 服务注册与定位（含父子作用域、就绪等待、变更订阅） | `echo-context` |
| 插件清单、挂载/卸载、启停门控、依赖声明与重挂 | `echo-plugin` |
| 分层配置组合（profile → bundle → patch），带来源追踪 | `echo-plugin-loader` |
| 进程外插件：握手、并发调用、取消、排空、崩溃重启、资源限额 | `echo-plugin-host` |
| 插件侧运行时：帧读写、serve 循环、宿主回呼 | `echo-plugin-sdk` |
| 冻结的线上协议类型与版本协商 | `echo-plugin-api` |

## 它不做什么

- **不定义业务**：不提供"工具"抽象，也不知道什么叫一次"会话"。
- **不管持久化**：插件状态存哪、怎么迁移，是插件与宿主的事。
- **不做权限模型**：进程外能靠 `env_clear` + `rlimit` 做隔离，进程内则假定可信。
- **不承诺跨 dylib 的 Rust ABI**：动态库轨是实验性的，默认用进程内或子进程。

## 从这里开始

- 想知道它为什么长这样 → [为什么需要它](./why.md)
- 想立刻跑起来 → [上手](./quickstart.md)
- 想确认能不能用在你的项目 → [边界与纪律](./boundaries.md)

```prodoc-flow
graph LR
  Host[宿主应用|/quickstart.md] --> Inproc[进程内插件]
  Host --> Sub[进程外插件]
  Inproc --> Contract[同一份插件契约|/model.md]
  Sub --> Contract
  Contract --> Gates[零领域依赖门禁|/boundaries.md]
```
