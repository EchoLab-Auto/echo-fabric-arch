---
id: structure
title: 组织一个应用
group: 实践
link: ["crates | 六件套", "boundaries | 边界与纪律"]
---

# 组织一个应用

框架不规定目录结构。但它的设计蕴含一套组织方式——照着摆，能省掉三类返工：
能力层被框架绑死、契约层长出业务、组合根散落多处。

## 四层，依赖只能向下

```prodoc-flow
graph TB
  Main["组合根 main()（唯一知道拓扑的地方）"] --> Plug[插件层]
  Main --> Cap[能力层]
  Plug --> Contract[契约层：DTO + trait + 错误码]
  Cap --> Contract
```

| 层 | 认识什么 | 不认识什么 |
|---|---|---|
| **组合根**（`main.rs`） | 框架、全部插件的类型、全局拓扑 | 具体业务实现 |
| **插件层**（`impl Plugin` / `impl PluginHandler`） | 框架 + 契约 | 别的插件、别的进程 |
| **契约层**（DTO + trait + 错误码） | `serde`（仅此而已） | 框架、业务实现 |
| **能力层**（真正的业务逻辑） | 契约 | **框架** |

`examples/minimal-host` 就是这一层的示范：它 `use` 了框架，但没有一行业务规则；
真正的业务在它注册的插件里。

## 五条纪律

### 1. 组合根只有一个

框架不提供容器（见[为什么需要它](./why.md)），所以**装配代码只出现在 `main()`
里**，平铺直叙：建 `Ctx` → 建注册表 → `register` 插件 → `mount_all` → 起进程外
插件 → 取服务 → 等信号 → 卸载。读一遍就知道这个程序由哪些部分组成。

代价很明确：加一个插件要改一处 `main.rs`。这是用「可预测」换掉了「自动发现」。

### 2. 能力层不认识框架

这条最容易破，代价最大。能力层常常要同时被「带插件的宿主」和「纯 CLI」调用
（`esc hub` 那种），一旦它 `use` 了框架，不带框架的用法就死了——被迫拖进整个
插件运行时。

可验证的推论：**撤掉框架时，只改组合根与 `mount` 闭包，能力层一行不动。**

### 3. 契约住中间层，错误码也住那儿

跨进程或跨插件的通信，最脆的地方是错误码：宿主用 `format!("{:?}", kind)` 生成、
插件用硬编码字符串解析，就是**两处独立的假定**——改一处忘了另一处，所有错误会
静默降级成 500。

正确做法是把码表放进契约 crate，两边都从它派生：

```rust
ErrorKind::as_code()      // 产出方
ErrorKind::from_code(s)   // 消费方，未知码 → Other（向前兼容）
ErrorKind::http_status()  // 需要 HTTP 时
```

加新错误类别只有一处要改，而且会被迫回答「它对外该是 400 还是 409」。

### 4. 按「失败影响」选运输层，不按重要性

判断只有一句：**它崩了，在途的活儿会不会跟着断？**

| 判据 | 选进程内 | 选进程外 |
|---|---|---|
| 崩了会断掉在途请求 | 是（不要额外的进程边界） | — |
| 需要高权限（docker socket、特权端口） | — | 是（单独用户 + 沙箱） |
| 需要运行期单独升级 | — | 是 |
| 每个请求都走它 | 是（序列化会吃掉收益） | — |

反直觉的是「重要的东西」和「该放进程内」常常相反：**最危险的东西（需要 root 的）
恰恰应该放外面**。

### 5. 配置分层，但整行替换不字段合并

用 `echo-plugin-loader` 时，四层合成（profile → bundles → patches → inline）的
语义是**按 id 定位、整行替换**。字段级 merge 会让「删掉一个配置项」无法表达——
你只能设成空值，而下次默认值变化时它不会跟着变。完整规则见[配置组合](./config.md)。

## 这段代码该放哪

| 你想写的 | 放哪 | 判据 |
|---|---|---|
| 业务逻辑 | 能力 crate，零框架依赖 | 它要能被 CLI 直接调用吗 |
| 跨插件 / 跨进程的 DTO 与 trait | 契约 crate | 双方都要编译它吗 |
| 装配、注册、生命周期 | 组合根 | 只有它知道全局拓扑吗 |
| 一棵子能力树 | `PluginScope` + `mctx.nest()` | 要随父插件级联卸载吗 |
| 配置解析、持久化、HTTP、日志 | 你自己选库 | 框架明确不管这些 |

## 两个反例

**给未来的需求预留 trait。** v0.2 的 `MountContext` 里有 `tools` / `skills` 两个
字段各配一个 sink trait，「给未来的工具插件预留接缝」。审计发现全仓 `mount` 闭包
都是 `|_ctx| Ok(vec![])`、`register_tool` 零调用点、sink 从未实例化——v0.3 全删。
详见[边界与纪律](./boundaries.md)：**给未来的预留，几乎总是错的**。

**让插件直接 `use` 宿主的类型。** 一旦这么做，「插件可被独立替换」就没了：换掉
一个插件要重新编译所有引用它的插件，语言无关也一并消失。要共享复杂类型，正确
做法是在宿主侧写适配层——适配层可以认识业务，框架不行。

## 最小骨架

```text
my-app/
  Cargo.toml            # [workspace]
  crates/
    my-contract/        # DTO + trait + 错误码（只依赖 serde）
    my-core/            # 业务实现（不知道框架存在）
    my-plugin-gui/      # 进程外插件：impl PluginHandler + serve_stdio
    my-app/             # 组合根：main() 装配
```

```rust
// my-app/src/main.rs —— 全部的装配代码都在这里
let ctx = Arc::new(Ctx::default());
let registry = PluginRegistry::default();
registry.register(Arc::new(BuiltinPlugin::new(manifest, mount_fn)))?;
registry.mount_all(&MountContext::new().with_ctx(Arc::clone(&ctx)));

let core: Arc<dyn MyControl> = ctx.require(&MY_CONTROL)?;   // 只用契约 trait
// …等信号…
registry.unmount("my.core")?;
```
