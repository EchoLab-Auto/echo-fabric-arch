# examples

| 目录 | 是什么 | 依赖 |
|---|---|---|
| [`minimal-host`](./minimal-host) | 宿主：注册服务、挂载进程内插件、拉起进程外插件、调用、卸载、排空 | `echo-context` + `echo-plugin` + `echo-plugin-host` |
| [`minimal-plugin`](./minimal-plugin) | 插件（Rust）：实现 `PluginHandler`，声明一个服务，响应调用 | **只有** `echo-plugin-sdk` |
| [`minimal-plugin-py`](./minimal-plugin-py) | 插件（**Python**）：手写帧，不用任何 Rust 代码 | **无**（只要 `python3`） |

前两个合起来是**证伪测试**：如果框架还有业务残留，它们就跑不起来。
第三个是**协议边界测试**：证明「协议即 ABI」不是说法，而是能跑的事实。

三者都**不依赖** `echo-defs` / `echo-agent` / `echo-protocol`——这套框架不认识
「工具」「会话」「人格」这些概念。宿主自己决定给插件发什么调用、怎么解释返回值。

## 跑起来

```bash
# Rust 插件 + 宿主
cargo build -p minimal-plugin
cargo run -p minimal-host
```

预期输出：

```
[1] Ctx 就绪：keys = ["host-version"]
    require(host-version) = 0.1.0
[2] 进程内插件已挂载（错误 []），Ctx keys = ["host-version", "counter"]
[3] 进程外插件就绪：state=Ready，贡献 ["service:greeter"]
[4] Invoke(greeter) → Ok { text: "{\"greeting\":\"hello, echo-fabric-arch\"}", ... }
[5] 已卸载进程内插件：Ctx keys = ["host-version"]，撤销次数 = 1，卸载前计数 = 41
[5] 进程外插件已排空退出（state=Stopped）

✅ 全部断言通过——这套框架可以在非 EchoAgent 的宿主里独立使用
```

最后一个 ✅ 不是装饰：示例里有 `assert!`，任何一步不符合预期都会以非零码退出，
CI 直接红。

Python 插件由 `cargo test -p echo-plugin-host --test cross_language` 驱动
（3 项测试：握手 + 贡献解析 + 调用往返 / 版本不兼容必须拒绝 / 帧能被 Rust 类型
逐条解析）。

## 各自在证明什么

**`minimal-host`**（约 130 行）

- **服务定位器**：`Ctx` + 强类型 `ServiceKey`，注册返回 `Disposer`（撤销句柄）
- **进程内插件**：`Plugin` trait + 挂载闭包；`mount` 是一次可撤销的副作用
- **进程外插件**：`PluginSupervisor` + `StdioTransport`——握手、贡献注册、监督、
  崩溃重启全在这里，宿主代码只有一次 `start()`
- **调用**：`invoke(contribution, ctx, payload)` → 结果原样带回
- **收尾**：`unmount` 撤销注册（示例断言服务键确实消失）+ `drain` 优雅退出

**`minimal-plugin`**（约 70 行）

只做一件事：`serve_stdio(Greeter)`。所有协议工作由 SDK 承担——帧读写（4 字节
小端长度前缀 + JSON）、握手（`Hello` → `Welcome` + `Register` + `Ready`）、
并发调用（每调用一任务，结果经单写者串行写出，帧不会交错）、`Cancel` / `Drain` /
`Dispose` 生命周期、宿主回呼。

插件作者**不需要**知道有这些。这就是 SDK 的全部意义。

**`minimal-plugin-py`**（约 150 行，含协议注释）

完全不碰 Rust：读 4 字节长度 → 读 JSON → 应答。文件头把线上协议完整写了一遍
（帧格式、握手顺序、消息形状、生命周期），可以直接当"用任何语言写插件"的参考。

硬规矩只有一条：**帧走 stdout，日志走 stderr**。

## 一个真实踩过的坑（留作警示）

`Contribution::Service` 曾经**根本过不了 stdio**：

```
{"kind":"service","key":"greeter","kind":"greeter","version":1}
                                   ^^^^ 与枚举 tag 重复
```

`Contribution` 是内部标签枚举、tag 叫 `kind`，而 `ServiceContribution` 的载荷
字段也叫 `kind` → 序列化出重复键 → 宿主 serde_json 报 `duplicate field`，整帧
解析失败。

**为什么长期没人发现**：inproc 轨类型直连、零序列化，这个冲突不出现；而当时唯一
声明 `Service` 贡献的插件（`echo-plugin-loop`）跑在 inproc 上。正是这些示例
（第一批同时用「Service 贡献 + stdio 轨」的组合）把它撞了出来。

现在 wire 字段叫 `service_kind`，并且有三道防线：

1. `echo-plugin-api` 的 `every_contribution_variant_roundtrips_through_json`
   ——所有变体都过一遍 JSON
2. `echo-plugin-api` 的 `service_contribution_wire_field_avoids_tag_collision`
   ——断言 JSON 里 `"kind"` 只出现一次
3. `echo-plugin-host/tests/cross_language.rs`——Python 插件真发这个帧；把
   `service_kind` 改回 `kind` 会让其中两条测试立刻 FAILED（已实测）
