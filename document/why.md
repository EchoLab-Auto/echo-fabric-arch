---
id: why
title: 为什么需要它
group: 概念
link: ["model | 核心模型"]
---

# 为什么需要它

## 从"能加载"到"能管住"

绝大多数 Rust 项目的插件化需求，第一步都会写成 `trait Plugin` + 一个
`HashMap<String, Box<dyn Plugin>>`。这能在半天内跑通，然后立刻遇到五个问题：

1. **卸载不干净**。插件在 `load()` 里注册了路由、开了线程、订阅了事件；`unload()`
   要么不存在，要么指望插件作者记得把自己做过的事情反向做一遍。
2. **顺序靠猜**。插件 A 要的服务由插件 B 提供。谁先谁后？写成隐式约定——
   顺序错了插件照样"加载成功"，直到运行期才 panic。
3. **一个崩溃全盘崩**。第三方插件 panic、内存泄漏、死循环，宿主跟着一起死。
4. **测试只能靠真跑**。没有"装载/卸载/再装载"的确定性语义，就只能拿真实宿主
   做集成测试，改一行等一分钟。
5. **语言被锁死**。`Box<dyn Plugin>` 是 Rust 的 `trait object`，本质上是
   进程内的函数指针表 —— 想接一个 Python / Go 写的插件，无路可走。

## 这套框架的五个回答

| 上面的问题 | 这里的答案 | 在哪 |
|---|---|---|
| 卸载不干净 | **注册即副作用**：每次注册返回 `Disposer`，卸载时逆序撤销 | `echo-context::Disposer` |
| 顺序靠猜 | **声明式依赖**：`Inject { required, optional }`，缺失即拒绝挂载；之后依赖变了自动追平 | `echo-plugin::Inject` / `reconcile` |
| 一个崩溃全盘崩 | **进程外轨**：子进程 + supervisor，崩溃按 backoff 重启、限额、限速 | `echo-plugin-host` |
| 测试只能真跑 | 装载/卸载/重挂都是纯 API，`assert` 可断言；示例宿主本身是**证伪测试** | `examples/minimal-host` |
| 语言被锁死 | **协议即 ABI**：4 字节长度前缀 + JSON，纯 Python 插件可直接跑 | `examples/minimal-plugin-py` |

## 与 Cordis（dsh）的关系

`echo-context` 的设计是明确对齐 [Cordis](https://github.com/cordiverse/cordis)
的：服务定位器 `Ctx`、可撤销副作用 `ctx.effect()`、事件总线 + waterfall 中间件、
`ctx.plugin()` 的插件树与 `ctx.isolate()` 的作用域隔离，在这里分别是
`Ctx` + `Disposer` + `EventBus` + `PluginScope` + `Ctx::child()`。

**但它不是"把所有代码装进容器里运行"的内核**，而是一套**可以被调用**的机制库：

- Cordis 的容器本身是应用，核心服务自己也是插件；这里框架**不提供容器**，
  组合根是宿主应用的 `main()`。
- v0.3.0 起框架**主动移除了进程级单例表**（原 `echo-context::kernel`）——
  因为"框架导出全局态"会直接破坏"同一进程能起两套宿主"这个复用前提。
- 代价是弱于 Cordis 的部分：没有 `inject` 的类型化注入、没有隐式的父子插件
  级联声明（要靠 `MountContext::nest()` 显式建作用域）。

## 与 MCP 的关系

MCP（Model Context Protocol）解决的是"让模型发现并调用外部工具"，本质上是
**一种工具插件的导入通道**。两者不冲突：MCP 服务器可以被包装成这里的一个
`Contribution::Custom`（`kind` 由宿主自己定义），于是任何 MCP 实现即插件。

反过来不成立——这套框架管的是**完整生命周期**（装载/依赖/启停/热替换/崩溃
重启/资源限额），MCP 协议本身不表达这些。

## 什么时候不该用它

- **插件集合编译期就固定、永不变动**：一个 `enum` 分派就够了，别引入注册表。
- **只想要"依赖注入"**：`Ctx` 可以单拿（`echo-context` 零依赖），不用整套。
- **要求插件与宿主共享复杂 Rust 类型**：这里是名字 + JSON 的边界，做不到
  （也不打算做，见[边界与纪律](./boundaries.md)）。
