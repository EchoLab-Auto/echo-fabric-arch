---
id: crates
title: 六件套
group: 概念
---

# 六件套

六个 crate 各管一段，依赖方向单向向下。判断"这段代码该放哪"的标准只有一条：
**它认不认识应用领域词汇。**

| crate | 一句话 | 依赖谁 |
|---|---|---|
| `echo-context` | 内核：服务定位 + 事件总线 + 可撤销注册 + 作用域 | 无（零 `echo-*` 依赖） |
| `echo-plugin-api` | 冻结协议：消息类型、贡献类型、版本协商 | 无 |
| `echo-plugin-loader` | 分层配置合成 + provenance + 依赖排序 | 无 |
| `echo-plugin` | manifest + 生命周期 + 注册表 + 插件树 | `echo-context` |
| `echo-plugin-host` | 宿主侧运输层 + supervisor（握手指令/取消/排空/重启/限额） | `echo-plugin-api` |
| `echo-plugin-sdk` | 插件侧运行时（帧读写 / serve 循环 / 宿主回呼） | `echo-plugin-api` |
| `framework-gates` | 仓库自守门禁（只含测试） | 全部 |

## 依赖方向（单向，门禁强制的）

```prodoc-flow
graph TB
  Host["宿主应用（你的 main）"] --> Reg[echo-plugin 注册表]
  Host --> HostC[echo-plugin-host 监督器]
  Reg --> Ctx[echo-context]
  HostC --> Api[echo-plugin-api]
  Sdk[echo-plugin-sdk] --> Api
  Loader[echo-plugin-loader] --> Nothing[无 echo-* 依赖]
  Plugin["你的插件二进制"] --> Sdk
  Gates[framework-gates] -->|门禁| Reg
  Gates -->|门禁| HostC
```

关键约束：**`echo-plugin` 不依赖 `echo-defs` / 任何业务 crate**，`echo-context`
与 `echo-plugin-api` 零 `echo-*` 依赖。这不是为了好看——它保证了"第三方项目
只引入框架"这件事成立，而门禁测试（`framework-gates`）会让越界无法合并。

## 三个边界的具体位置

**`echo-plugin` 不认识"工具"**。它只认识 manifest、生命周期、服务键。所以
"把插件的 `Contribution::Tool` 变成宿主能调用的东西"这件事，不在框架里，在
宿主的适配层（EchoAgent 里是 `echo-plugin-adapter`）。

**`echo-context` 不认识"插件"**。它只是服务表 + 事件总线 + 撤销句柄；插件是
`echo-plugin` 的事。所以一个只想解决依赖注入的项目可以**只拿 `echo-context`**。

**`echo-plugin-host` 不认识 Rust 类型**。它只认协议帧。因此它可以把任意语言写的
插件拉起来（`examples/minimal-plugin-py` 就是一个纯 Python 插件），代价是跨进程
调用必须过 JSON。

## 该用哪个 crate 做起点

| 你的需求 | 起点 |
|---|---|
| 只要依赖注入 / 事件总线 | `echo-context` |
| 进程内插件 + 生命周期 | `echo-plugin` |
| 想要崩溃隔离 / 外部进程插件 | `echo-plugin-host` + `echo-plugin-sdk` |
| 插件配置要分层可覆盖 | `echo-plugin-loader` |
| 只想写插件、不动宿主 | `echo-plugin-sdk` |
