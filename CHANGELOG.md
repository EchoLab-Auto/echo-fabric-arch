# Changelog

本仓遵循 [语义化版本](https://semver.org/lang/zh-CN/)。0.x 期间允许 breaking，
但每一处都记在这里。`PROTOCOL_VERSION` 是**线上契约**版本，与 crate 版本独立
（判定规则见 [README](./README.md#版本与兼容)）。

## [0.17.0] — 未发布

### 新增：清单承载依赖与提供（`requires` / `optional_requires` / `provides`）

依赖此前只存在于构造代码里（`BuiltinModule::with_inject(Inject::new([...]))`），
后果有两个，都很实在：

- **读不出来**：想知道"这个模块要什么"必须读代码，再跟到每个键的定义处；
- **只有一半**：`Module` trait 有 `inject`，没有任何"我提供什么"的声明——
  服务靠 mount 闭包里命令式 `ctx.provide(...)` 注册，于是"这个键是谁给的"
  只能人肉搜索 `provide(`。面板的依赖图因此只能硬编码一张表。

现在 `ModuleManifest` 有三个新字段（都 `#[serde(default)]`，旧 manifest 照旧
解析）：`requires` 硬依赖服务键、`optional_requires` 软依赖、`provides` 本模块
提供的键。

配套：

- [`Module::inject`] 的**缺省实现改为从清单取**（`Inject::from_manifest`），
  模块不必再写一遍；`with_inject` 保留但语义降为**覆盖**（第三方宿主与测试用），
  覆盖后与清单不一致由宿主侧校验报出。
- 新增 [`Module::provides`]，缺省取 `manifest().provides`。
- [`ModuleManifest::from_toml`]：从 `module.toml` 文本构造，只把版本号留给宿主
  注入（版本随宿主发版，写进模块文件就得每次改十几个文件且改漏无人发现）。
- `ModuleDescriptor` 加 `provided_services`，面板据此画真实依赖图。

模块侧随之可以把身份、依赖、提供全部写在 crate 根目录的 `module.toml` 里，
用 `include_str!` 编进二进制。

## [0.16.1] — 未发布

### 变更：`[[module]]` 的 `name` 改为可选（缺省取 id）

它是纯展示元数据——`kind = "builtin"` 的行根本不用它（面板标题来自模块自己的
manifest）。必填的唯一后果是"只想写个开关"的配置也得抄一个名字，而宿主机生成
基线文件的场景尤其别扭：**写进去的名字不生效，看着却像能改**。

空名/空白回退到 `id`，任何消费方都不会拿到空串去渲染（面板里一行空白比显示
id 更让人困惑）。

## [0.16.0] — 未发布

### 新增：`modules-base.toml`（与 profile 同目录时自动作为第 0 层）

宿主自带的模块此前是**隐式集合**：`modules.toml` 不提它们，它们照样装载。
隐式的好处是"升级不会因为配置过时而少装一个模块"，坏处是没人能一眼看出
装了什么、按什么顺序——顺序恰恰是运行期调不了的。

`modules-base.toml` 把这份集合变成可读的文本，由 [`load`] 自动应用：

- 排在**所有** bundle / patch 之前——它描述的是"宿主自带什么"这个前提，
  部署方的 profile 与 patch 都是对它的追加与覆盖；
- **存在才应用**：没有这个文件时行为一字不变（老部署零迁移）；
- 行的 `layer_source` 记为 `modules-base.toml`，因此"这行来自哪一层"在
  面板里仍然答得出来；
- 以 `modules-base.toml` 自身作为 profile 时不会重复应用。

覆盖语义沿用既有的"按 id 定位、整行替换"：profile 里同 id 的行会替换基线的
那一行，**位置留在原位**（覆盖不该让挂载顺序漂移），且不继承基线的
`config`（半覆盖会留下陈旧键）。

[`BASE_FILE_NAME`] 导出这个名字，宿主与测试都不必各自抄字面量。

## [0.15.0] — 未发布（breaking）

- 全仓统一采用 **Module / module / 模块** 命名：crate、API、协议、配置、示例与文档均由旧命名体系迁移。

## [0.14.0] — 未发布（breaking）

### 移除：贡献分类里的领域词汇

`Contribution` 曾有五个变体，其中三个是**应用领域分类**：面向模型的工具声明
（含 JSON Schema、UI 分类、"是否产生平台可见回复"）、技能目录（`SKILL.md`
路径）、聊天平台模块实例（平台名、能力位、会话标签、UI 描述符）。

2026-11 审计确认它们**零真实使用者**：EchoAgentCore 已经不依赖本仓（用自己
仓库内的副本），本仓内部只有测试与 conformance 夹具在构造它们。删掉的是：

- `Contribution::{Tool, Skill, ModuleInstance}`
- `ToolContribution` / `SkillContribution` / `InstanceContribution` /
  `InstanceCapabilities`
- SDK 的 `InstanceModule` / `InstanceHandler` / `serve_instance_stdio`
- `capabilities::{TOOLS, SKILLS, MODULE_INSTANCE}`

### 新增：`Contribution::Custom`

领域词汇的**唯一入口**：

```rust
Contribution::Custom {
    custom_kind: String,   // 宿主自己的分类（"report_generator" / 任何字符串）
    name: String,          // Invoke 寻址名（空 = 不参与寻址）
    payload: Value,        // 宿主与模块自行约定，框架不校验
}
```

线上标签是 `custom`，分类字段叫 `custom_kind`——载荷字段不能叫 `kind`，枚举
tag 已经占了那个名字，重复键会让整帧解析失败（`Service` 变体踩过同一个坑）。

这把「框架不该认识业务分类」从口号变成了结构：以后任何宿主想加一类贡献，写
`Custom { kind: "它自己的词", .. }` 就行，**协议不需要改**。

### 顺带：`Contribution::target_name()` / `custom_kind()`

哪些贡献参与 `Invoke` 寻址是**协议的知识**，放在变体定义旁边。此前 SDK 里有个
函数把每个变体挨个 match 一遍——不是因为它需要，而是因为它不得不穷尽枚举。
现在 SDK 只调方法。

### 协议兼容性：没有 bump `PROTOCOL_VERSION`

删变体通常是线上格式变更。这次没 bump，理由可验证：**没有任何真实模块发送过
那三个分类**（唯一的生产者是 echo-server-core 的面板模块，它只发 `Service`）。
新宿主能解的帧集合小了一圈，但实际在跑的模块一个都没受影响。

若将来发现确有外部模块在用，正确做法不是回退，而是让它改发 `Custom`——那才是
它本来就该用的形状。

### 门禁

- 领域词审计不再排除 `echo-module-api` / `echo-module-sdk`（此前排除等于这两个
  crate 里新增任何领域词都不会被发现，而那正是业务类型能长进来的原因）。改为
  `KNOWN_DOMAIN_RESIDUE` 白名单：已知残留逐条登记，新增的报错。**本次清空至零条**。
- 新增防腐烂测试 `known_residue_entries_must_still_be_needed`：白名单条目一旦
  不再触发违规就报错，逼着同步删除——否则白名单只会单向变厚。
- `rust_files` 跳过 `tests.rs`（`#[cfg(test)] mod tests;` 的目标文件）：此前它被
  当生产代码审计，只会逼着测试里换词。

### 文档

- `document/boundaries.md`：把「领域残留的实况」并入 §三「名字 + 存在性，而不是
  类型」——两节讲的是同一条原则在两处（服务表 / 协议）的落地，分开写会让读者
  以为是两件事。审计的实测数据（谁用过、零使用者、EchoAgentCore 已不依赖本仓）
  与白名单机制作为「它怎么被强制执行」保留在同一节里。
- `README` / `crates.md` / `why.md` / `quickstart.md` / `getting-started.md` 中
  关于"协议表面尚未收敛"的表述同步更新。

## [0.13.0] — 2026-11-28

### 移除

- **`MessageTarget` / `ChannelType` / `IncomingMessage`**（63 行）：从聊天平台
  带过来的完整数据结构（出站寻址、私聊/群聊通道、入站消息含 `at_me` / 图片 /
  文件）。2026-11 审计确认**零使用者**——本仓内部只有一条 serde 往返测试在测
  它们，没有任何生产者构造过它们。

### 新增

- `Contribution::target_name()`：把「哪些贡献参与 `Invoke` 寻址」从 SDK 挪到
  变体定义旁边。

### 门禁

- 领域词审计改为白名单制（详见 0.14.0 段），并清掉第一批残留。

## [0.12.0] — 2026-11-28

从第二个真实宿主（echo-server-core）的接入经验里带回来的四处缺口。

### 新增

- **`AsyncHostService`**：宿主服务的异步 trait，签名带调用者身份：

  ```rust
  async fn call(&self, cx: CallContext, method: &str, payload: Value)
      -> Result<Value, HostCallError>;
  ```

  同步的 `HostService` 保留（旧实现零改动），注册表在调用时抹平差异：异步版直接
  await，同步版挪到 `spawn_blocking`（不占 tokio worker；无运行时上下文时内联）。

- **`CallContext::caller`**：本次回呼的来源模块 id，**由 supervisor 从连接归属
  填写**，线上协议里没有这个字段 —— 模块无法伪装成别的模块。宿主服务据此做来源
  授权与审计。同步 trait 没有这个参数（签名不变，向后兼容）。

- **`ModuleHandle::wait_until(contribution, ctx, payload, timeout, predicate)`**：
  「握手完成」不等于「模块内部的服务真的起来了」。轮询 + 超时 + 判据这套就绪探测
  每个宿主都会写一遍，收进框架。

- **`ModuleScope::snapshot()` → `ModuleTreeSnapshot`**（可序列化）：把作用域 +
  注册表投影成只读树。遍历、父子拼接、`ready/pending/failed/disabled` 判定原本
  每个宿主都要重写一遍（重写就会与框架语义漂移）。

- **`InvokeOutcome::data(Value)`**：只填结构化结果的构造器。配套把 `text` /
  `images` 在文档注释里标为**遗留字段**。

- **`HostServiceRegistry::names()`**：已注册服务名（排序），面板/快照用。

- 文档新增 [`document/host-service.md`](./document/host-service.md)：宿主服务与
  `HostCall` 回呼的完整说明（两种实现、身份、错误契约、三个坑、最小示例）。
  `examples/` 补上了回呼演示——此前框架最重要的机制之一**没有示例也没有文档**。

### 变更（breaking）

- `HostServiceRegistry::call` 由同步变异步，并新增 `caller` 参数：
  `async fn call(&self, caller: &str, service: &str, method: &str, payload: Value)`。
  这是内部调用点（supervisor）才用的 API，宿主一般只调 `register_*`。

### 变更

- `mount` 失败现在会**写入 descriptor 的 `error`**（此前只出现在 `mount_all` 的
  返回值里，事后无从查起、面板只能说"没挂载"）。快照因此能回答"它为什么没起来"。
- `Ctx::keys()` 的返回顺序改为**按名字排序**。底层是 `HashMap`（迭代顺序随机），
  而这份清单会进 `MissingService.available`（给人看的错误信息）与面板——随机顺序
  会让同一个错误每次长得不一样。顺带让文档里的示例输出可以固定下来。

### 文档

- `README.md` 重写：去掉版本历史叙述（已由本文件承载），改为「问题 → 概念 →
  两种形态 → 跑起来 → 写模块 → 装进宿主 → 怎么组织应用 → 文档」的短结构。
- 新增 [`document/structure.md`](./document/structure.md)「组织一个应用」：四层结构、
  五条纪律、一段代码该放哪的判据、两个反例、可搬的最小骨架。
- 修 `document/quickstart.md` 里过期的 `tag = "v0.7.0"`。
- 修 `document/crates.md` 流程图里的 `-.门禁.->` 虚线语法——ProDoc 只支持
  `-->` 与 `-->|标签|`，那两行原本会被解析器跳过。
- 对齐 `examples/README.md` 与 `docs/getting-started.md` 的行数与预期输出。

## [0.11.2] — 2026-11-28

### 新增

- `ModuleHandle::upgrade_preserving_contributions`：面向“宿主在启动期物化贡献注册表”的兼容热替换。新实例握手后、流量切换前比对完整贡献集合；工具/schema/实例等贡献有任何变化即关闭 replacement、保留旧实例并返回明确错误，避免只换进程造成宿主注册表与模块能力漂移。

## [0.11.1] — 2026-11-21

### 新增

- `ServiceSlot<T>`：**在 `mount` 时解析、在处理时取用**的服务槽。

  模块的依赖常常是这样的形状：只有 `mount` 期间能查服务表（`MountContext`），
  而真正用到宿主能力的地方是命令处理器 / 事件回调——那些闭包在构造模块时就
  得交出去。手写 `Arc<RwLock<Option<T>>>` + "卸载时清空"在每个模块里都长得
  一样，而且极易漏掉清空（拿旧宿主的句柄继续跑）。

  `slot.bind(mctx, &KEY)?` 一步完成：解析（缺失即 `MissingService`）+ 记录
  + 返回"卸载清空"的 `Disposer`。

## [0.11.0] — 2026-11-21

**模块零捕获**：`MountContext` 现在能自己解析宿主提供的能力，于是
`mount` 闭包不必再捕获 `Arc<dyn SomeHost>` 这类运行期句柄，模块构造退化成
`module(version)`。

### 新增

- `MountContext::service<T>(&ServiceKey<T>) -> Option<T>`：从服务表解析；
- `MountContext::require<T>(&ServiceKey<T>) -> Result<T, ModuleMountError>`：
  缺失即挂载失败，错误带 `available` 全部键名；
- `MountContext::context() -> Option<&Arc<Ctx>>`：需要 `provide` 或条件解析时
  取服务表本身。

### 为什么这属于框架

「宿主把句柄塞进闭包」与「模块按键解析」是**同一件事的两种写法**，但后者
让**目录**（谁存在）与**接线**（各自需要什么）解耦：组合根可以写
`push(catalog, tools::module(version))`，把「哪个 trait 对象给哪个模块」交给
服务表。这是 dsh `inject` 的**取用侧**——`inject()` 只声明，不取用。

### 不变

`MountContext` 的既有字段与构造方式一字未动（新方法都是附加的）。
裸注册表（无 `Ctx`）语义不变：`service` 回 `None`、`require` 回
`MissingService{available: []}`，不 panic。

## [0.10.0] — 2026-11-20

**运行态上收**：把「宿主为了用这套框架而必须自己维护的状态」收进注册表。
动机很具体——把内置能力逐一搬成模块之后，下游宿主（`echo-agent`）里那个
`ModuleHost` 还剩 1,559 行，其中一大半是在**重新实现框架本该有的运行态**：

| 宿主自己维护的 | 为什么本来是框架的 |
|---|---|
| 缓存的 `MountContext` | `set_enabled` / `restart` / `reconcile` 每次都要它，宿主只能自己存一份 |
| 事件订阅**兴趣集** | `emit` 是热路径（流式正文每 token 一条），没人订阅时要能一次原子读就返回 |
| 投递**重入保护** | 模块的 `on_event` 里再 `emit` 会自递归 |
| 投递**计数** | 排查「事件为什么没到」的第一手证据 |

四者都与领域无关，任何宿主都会需要。

### 新增

- `ModuleRegistry::set_mount_ctx(ctx)` / `mount_ctx()`：缓存挂载上下文；
- `ModuleRegistry::enable(id, enabled)` / `restart_cached(id)` /
  `reconcile_cached()`：**不需要再传 `MountContext`**（用缓存的那份；
  未设置 = 默认上下文，裸注册表语义不变）；
- `ModuleRegistry::has_event_subscribers()` / `wants_event(name)`：热路径
  两道闸门（一次原子读 / 一次查表，都不序列化）；
- `ModuleRegistry::refresh_event_interest()`：重算兴趣集。**注册表自己在
  挂载 / 卸载 / 启停 / 重启 / reconcile 之后调用**——宿主不必记得；
- `ModuleRegistry::deliver_event_guarded(event)`：投递 + 重入保护 + 计数；
- `ModuleRegistry::event_counters()`：`(投递成功, 丢弃/失败)`；
- 新增私有模块 `runtime_state`（`RuntimeState` + `EventInterest`），带四项
  单元测试。

### 变更（语义澄清，非破坏）

- `set_enabled` 的**锁范围缩小**：原来持着 `enabled_state` 调 `mount`，而
  `mount` 现在会刷新兴趣集（也要读 `enabled_state`）——那是自锁。改为
  「读判断 → 释放 → 动作 → 写状态 → 刷新」。
- `enable(id, true)` 的语义明确写成**状态切换**（默认状态即"启用"，所以对
  从未挂载过的模块是 no-op）。初始挂载仍走 `mount` / `mount_all`。

### 影响

下游只需要删代码：宿主里那份缓存与兴趣集实现可以直接删掉，改为委托。
既有调用点（带 `&MountContext` 参数的版本）**全部保留**，不做破坏性改动。

## [0.9.0] — 2026-11-01

命令处理补上**异步入口**——v0.8 的 `handle_command` 是同步的，只能做"不等"的
事；真实命令域里几乎每条都要 IO（读盘、`spawn_blocking`、跨机查询）。

### 新增

- `Module::handle_command_async(name, payload) -> Option<BoxFuture<'a, Result<Value, String>>>`
  （默认把同步版本包成 ready future，所以只实现同步版的模块照常工作）；
- `ModuleRegistry::dispatch_command_async(name, payload)`：宿主实际入口；
- `BuiltinModule::with_command_handler_async`。

### 为什么用 boxed future 而不是 `async fn`

模块以 `Arc<dyn Module>` 存在，trait 必须**对象安全**，而原生 `async fn` in
trait 不是对象安全的（AFIT 目前不支持 `dyn`）。`payload` 按值收是因为 future
不能借调用方的栈。

## [0.8.0] — 2026-11-01

补上最后两类"声明了没人读"的贡献：**命令处理**与**有序文本块**。至此协议里
的六类贡献（Tool / Skill / Service / Events / ModuleInstance / Command / Text）
在 inproc 轨上都有真实消费方。

### 新增 — 命令处理（`Contribution::Command`）

- `CommandContribution { name, description }`：模块声明"这些命令我来处理"。
  命令名是**宿主词汇**（如 `RequestXxx`），框架只搬不解释。
- `Module::handled_commands()` + `Module::handle_command(name, payload)`：
  返回 `Some(Ok(v))` 处理了 / `Some(Err(e))` 认识但失败 / `None` 不认识。
- `ModuleRegistry::dispatch_command()`：按注册序问已挂载模块；`None` = 无人
  处理（调用方走默认分支）。**只问已挂载的模块**——模块被禁用后命令自动回到
  "无人处理"，不需要在 dispatch 处特判。
- `ModuleRegistry::command_owner()`：诊断用（某条命令当前归谁）。
- `ModuleDescriptor.handled_commands`：面板能回答「这条命令谁在处理」。

### 新增 — 有序文本块（`Contribution::Text`）

- `TextBlockContribution { key, label, order }`：只声明**身份与顺序**。
- `Module::text_blocks()`（声明）+ `Module::text_block_content(key, context)`
  （内容）——**两个方法分开是有意的**：描述符生成要能随时调用（纯读、无副
  作用），而内容依赖上下文（同一块在不同会话下可以不同）。
- `ModuleRegistry::collect_text_blocks(context)`：收集已挂载模块的块，按
  `order` 升序（等值保持注册序），**内容为空的块被丢掉**。
- `ModuleDescriptor.text_blocks`：面板能回答「这个模块贡献了哪些段落」。

### 为什么叫 `Text` 而不是 `Prompt`

本仓的 `framework-gates` 有一条"框架层不得出现应用领域词汇"的门禁，词表里
就有 `prompt`。这不是文字游戏：**框架确实不该知道文本会被拿去拼系统提示词**。
叫 `Text` 之后，任何"拼接有序文本"的宿主场景（提示词、文档、报告）都能用同一
套机制，而框架一行都不用改。

> 这条门禁在本轮当场抓到了第一版命名（`Contribution::Prompt`）——说明它有用。

## [0.7.0] — 2026-11-01

补上 `Contribution::Events` 的**进程内**那一半——它一直"声明了没人读"
（SDK 的 `serve` 里 `Contribution::Events(_) => None`，宿主零消费）。

### 新增

- `ModuleEvent { module, name, payload }`：一条模块事件。`module` 由**宿主**
  填充（注册表在挂载时把 id 注入上下文），模块自己 emit 时不必知道 id。
- 模块 → 宿主：`MountContext.events: Option<EventSink>` + `MountContext::emit`。
  **没有出口 = 静默丢弃**（降级不是错误）——单测与无 Panel 的部署不需要它。
- 宿主 → 模块：`Module::subscribed_events()`（默认空）+ `Module::on_event()`
  （默认 `Ok`）+ `ModuleRegistry::deliver_event()`（按名分发，`"*"` =
  `ALL_EVENTS` 全收）。**只投给已挂载的模块**（未挂载不该有副作用，同款原则）；
  `on_event` 返回 `Err` 只计数不卸载——事件是观察通道，不是控制通道。
- `BuiltinModule::with_subscriptions` / `with_event_handler`。
- `MountContext.module_id`：本模块的 id（顺带让模块能自己打带 id 的日志）。
- 注册表自己在挂/卸时发 `module.mounted` / `module.unmounted`（带 `id` /
  `depth`）——模块树的变化因此可观测。
- `ModuleScope` 的子作用域**继承事件出口**：整棵树汇到同一接收方，靠 `depth`
  区分来源。
- `ModuleDescriptor.subscribed_events`：面板能回答「这个模块会对什么有反应」。

### 为什么 emit 走 `MountContext` 而不是 trait 方法

`emit` 是**挂载期之后也要能用**的能力（模块在运行期随时通知宿主），而
`Module` 的 trait 方法拿不到 `MountContext`。把 sink 塞进上下文、让模块在
`mount` 里存一份（`Arc<dyn Fn>` 可克隆），与协议里 `Emit` 帧的
"即发即走、无返回值"语义一致。`on_event` 走 trait 是因为它的调用方是宿主、
时机是宿主决定的（推 vs 拉的分界）。

## [0.6.1] — 2026-11-01

修 v0.6.0 的一个漏洞：**类型化 `provide` 不广播变更通知**。

`register` 的注册/注销都会 `publish(Change)`（父链向上传播 + 唤醒等待者），
但 `provide` 只 `notify_waiters()`——于是 `Ctx::on_change` 的订阅者（宿主的
`reconcile`）看不到通过 `provide` 注册的服务。现在两条路径都走 `publish`。
+1 测试（`typed_provide_broadcasts_like_register`）。

[0.9.0]: #090--2026-11-01
[0.8.0]: #080--2026-11-01
[0.7.0]: #070--2026-11-01
[0.6.1]: #061--2026-11-01

## [0.6.0] — 2026-11-01

两块缺失的 dsh 语义补齐：**声明式 `inject` + 就绪等待**，以及**模块树 / 父 ctx**
（父模块宿主化子模块、卸载级联）。v0.5.0 的 `required_services` 升级为完整的
注入声明，旧写法继续生效（默认实现读 `inject().required`）。

### 新增 — 声明式注入

- `Inject { required, optional }`（`echo-module`）：
  - `required` 缺失 → 拒绝挂载（`ModuleMountError::MissingService`，**mount
    闭包不执行**）；
  - `optional` 只作声明（面板展示 / 宿主按需读），**不参与**挂载判定。
- `Module::inject()` 是唯一真值；`Module::required_services()` 降为
  `inject().required` 的便捷读法（兼容 v0.5.0 的覆写者）。
- `BuiltinModule::with_inject` / `with_required_services`（后者等价于
  `with_inject(Inject::new(..))`）。
- `ModuleDescriptor` 新增 `optional_services` / `pending_services`
  （`#[serde(default)]`）：面板能回答「它要什么」与「此刻还缺什么」——后者与
  `error` 不同，是"事实"而非"上次尝试的结果"。

### 新增 — 就绪等待与依赖变更

- `Ctx::wait_for_keys(&[String], timeout)`（`echo-context`）：等一组键**全部**
  就绪，命中立即返回、超时给 `ServiceError::Missing{key, available}`。
- `ModuleRegistry::mount_when_ready(id, ctx, timeout)`：
  **先等后挂**（不是"失败后重试"），中间不留空窗；`timeout = ZERO` 等价于
  `mount`。
- `ModuleRegistry::mount_all_when_ready(ctx, timeout)`：启动期按注册顺序批量
  等待挂载。
- `ModuleRegistry::reconcile(&ctx) -> ReconcileReport`：依赖变动后重新核对——
  pending 里依赖已齐的挂上、已挂载但依赖消失的卸下并重新进 pending。**只看
  事实不看意图**：人主动禁用的、从没挂过也没 pending 记录的，一律不动。
- `ModuleRegistry::pending_inject()`：`id → 缺的键`，面板"待挂载"视图用。

### 新增 — 模块树 / 父 ctx（dsh `ctx.module()` 的对应物）

- `Ctx::child()` / `parent()` / `depth()`：**父子作用域**。父链继承、子遮蔽父、
  子注册对父不可见。`resolve` / `contains` / `keys` / `service` 全部沿父链查找。
- `Ctx::on_change(f) -> Disposer` + `Change::{Provided, Disposed}`：变更通知
  **沿父链向上传播**（根能看见整棵子树）；等待在**整个家族内共享唤醒源**
  （子等父提供的服务不会漏唤醒）。
- `ModuleScope`（新模块 `scope`）：一层 = 一个 `Ctx` 作用域 + 一个
  `ModuleRegistry`。`ModuleScope::root(ctx)` 建根，`mount` / `mount_when_ready`
  / `unmount` / `shutdown` / `walk` / `children_of`。
- `MountContext::nest()` / `MountContext::tree`：模块在自己的 `mount` 里建
  **子作用域**并挂子模块。归属靠挂载期压栈记录（`mounting` 栈），不靠模块自报；
  `unmount(父)` 会 `shutdown()` 它名下的子作用域（**递归**，子先于父）。
- 无树把手的裸注册表：`nest()` 返回 `None`，挂载照常成功——树是**可选**能力。

### 修复

- **`Module::unmount()` 此前从未被调用**：`ModuleRegistry::unmount` 撤完
  disposer 就返回，模块覆写的 `unmount`（"disposer 之外的清理"）成了死代码。
  现在 disposer 撤完会调用它。顺带把 disposer 撤销顺序改成**逆序**（后注册的
  先撤），与嵌套资源直觉一致。
- `Ctx::register` 的 disposer 此前直接改表、不广播：现在走
  `remove_and_wake`，订阅者与等待者都能看见"注销"。

### 为什么不把 inject 类型化

服务键的**存在性**是 `echo-context` 的概念；键背后的类型是宿主与模块的私事。
框架若要求类型化，就得认识宿主的 trait（`echo-module` 零领域依赖的边界立刻破），
或者要求模块之间互相 `use` 对方的类型（模块就不再可独立替换）。存在性校验已
覆盖真实故障：注册顺序错、服务忘了注册、服务被提前注销。

## [0.5.0] — 2026-11-01

补上 in-process 模块缺失的两块机制：**配置下发**与**服务键依赖校验**。
此前 `modules.toml` 的 `[module.config]` 只能到子进程模块的 `Hello.config`，
内置模块的 `mount` 拿不到自己的配置；`Contribution::Service` 也只是一个
"声明了没人读"的字段。

### 新增

- `MountContext.config: serde_json::Value`（+ `with_config` / `config()` /
  `config_str(key)`）：宿主按模块 id 注入该模块的配置，**缺省 `Null` 而非
  `{}`**——模块要能区分「没配」与「配了空表」。框架不解析内容、不做 schema
  校验，那是宿主与模块之间的契约。
- `ModuleRegistry::set_config` / `set_configs` / `config_for`：组合根在挂载前
  登记配置；`mount()` 按 id 合并进传给该模块的 `MountContext`。运行期重新
  挂载（`set_enabled(true)` / `restart`）取**当时**登记的值。
- `Module::required_services() -> Vec<String>`（默认空）+ `BuiltinModule::
  with_required_services`：模块声明「挂我之前这些服务键必须在 `Ctx` 里」。
  `ModuleRegistry::mount` 在跑 mount 闭包**之前**逐个 `Ctx::contains` 校验，
  缺失即 `ModuleMountError::MissingService { key, available }`——
  **mount 闭包根本不会执行**（依赖没就绪就不该产生副作用）。
  这与 dsh 的 `inject` 同义，只是以存在性校验的形式落地（框架不认识具体类型）。
- `ModuleDescriptor.required_services`（`#[serde(default)]`）：面板据此回答
  「这个模块为什么不挂载」。

### 为什么 `required_services` 用 `Vec<String>` 而不是类型化

服务键的**存在性**是 `echo-context` 的概念（字符串 + 是否有），而"这个键背后
是什么类型"是宿主与模块之间的私事。框架若要求类型化，就得认识宿主的 trait
（`echo-module` 零领域依赖的边界就破了）。存在性校验已经能挡住真实故障：
组合根把服务注册挪到挂载之后、或忘了注册——启动期立刻失败并列出可用服务，
而不是"模块挂载成功、运行期才发现依赖不存在"。

[0.6.0]: #060--2026-11-01
[0.5.0]: #050--2026-11-01

## [0.4.1] — 2026-10-19

新增**跨语言模块示例与测试**——把「协议即 ABI」从说法变成可运行的事实。

### 新增

- `examples/minimal-module-py/module.py`：纯 Python 模块，约 150 行，**零 Rust
  依赖**。手写帧（4 字节小端长度前缀 + JSON），不用 SDK。文件头把线上协议完整
  写了一遍——帧格式、握手顺序（`Hello` → `Welcome` → `Register` → `Ready`）、
  消息形状、生命周期（`drain` / `dispose` / `cancel` / stdin EOF），可直接当别的
  语言的参考实现。
- `crates/echo-module-host/tests/cross_language.rs`（3 项）：
  - `python_module_speaks_the_protocol`：真 spawn Python 进程，验握手 + 贡献
    解析 + 调用往返 + 未知贡献返回结构化错误 + drain 优雅退出
  - `python_module_rejects_mismatched_protocol_version`：协议版本 999 必须被
    拒绝（fail-closed），不是"先连上再说"
  - `python_module_frames_parse_with_the_rust_types`：直接对帧做断言（不经
    supervisor），便于区分"协议问题"与"监督逻辑问题"
  - 环境无 `python3` 时**跳过而非失败**
- CI 新增跨语言步骤。

### 为什么值得一条测试

「不锁定语言」是这套框架相对 Node 生态模块框架的实质优势，但写在文档里只是说法。
Python 模块真发一遍帧，才算证据——顺带把 M4 修掉的 `ServiceContribution` 重复键
bug 也纳入了回归：把 `service_kind` 改回 `kind`，两条测试立刻 FAILED（已实测）。

## [0.4.0] — 2026-10-19

新增**可运行的证伪测试**（`examples/`）与第三方文档；顺手修掉一个从 P0 就存在的
协议 bug。

### 修复（真 bug）

- **`Contribution::Service` 过不了 stdio 轨**：`Contribution` 是内部标签枚举、
  tag 为 `kind`，而 `ServiceContribution` 的载荷字段也叫 `kind` → 序列化出
  重复键 `{"kind":"service","key":"g","kind":"g","version":1}`，宿主
  `serde_json` 报 `duplicate field`，整帧解析失败。
  - 为什么长期未发现：inproc 轨类型直连、零序列化，冲突不出现；当时唯一声明
    `Service` 贡献的模块跑在 inproc 上。M4 的最小示例（唯一同时用「Service
    贡献 + stdio 轨」的组合）把它撞出来了。
  - 修法：wire 字段改名 `service_kind`（此前这种帧**根本解析不了**，零兼容成本）。
  - 防复发：新增 `every_contribution_variant_roundtrips_through_json`
    （所有变体都过一遍 JSON）与 `service_contribution_wire_field_avoids_tag_collision`
    （断言 JSON 里 `"kind"` 只出现一次）。

### 新增

- `examples/minimal-host`（130 行）+ `examples/minimal-module`（70 行）：
  只用框架六件套的最小宿主与模块，**不引入** `echo-defs` / `echo-agent`。
  宿主内部带 `assert!`，跑通即证明框架可独立使用；CI 里作为独立步骤执行。
- `docs/getting-started.md`：第三方快速上手（写模块 / 拉起来 / 同进程轨 /
  崩溃隔离 / 跨语言模块 / 版本策略）。
- `examples/README.md`：预期输出与「每个示例在证明什么」。

## [0.3.0] — 2026-10-19

**breaking**：移除 `echo_context::kernel`（进程级单例表，`pub mod kernel`）。
框架仓的「零全局态」从此无条件成立。

### 移除

- `echo-context::kernel`（`set` / `get` / `get_or_init` 三个函数 + 那张
  `TypeId` 键控表）。它已搬到宿主侧：EchoAgentCore 的 `echo-bootstrap` crate。

### 为什么搬走

1. **框架自己一处不用**——六个 crate 里零调用；8 处使用者全在应用层
   （`agent_manager` / `module_host` / `global_policy` / `shell_manager` /
   搜索客户端 / 联邦导入）。
2. **它破坏复用前提**。「同一进程能起两套宿主」要求框架不在进程级藏状态；
   有这张表的话，第二套宿主会读到第一套的句柄。

终态应当是让宿主把句柄经 `echo_context::Ctx` 显式注入（`Ctx::provide` 已经
具备能力，且天然支持多份）。`echo-bootstrap` 是到达终态之前的过渡形态，
其 crate 文档写明了这一点。

### 迁移方式

宿主侧调用点把 `echo_context::kernel::set(…)` 改成 `echo_bootstrap::set(…)`
（14 处，纯改名，行为不变）。

## [0.2.0] — 2026-10-19

迁入宿主侧四件套，**六件套齐备**——本仓现在可以独立跑通端到端模块链路。
从 EchoAgentCore 逐字节搬迁，代码与行为零改动。

### 新增

- `echo-context` 0.1.0：`Ctx` 服务定位、强类型 `ServiceKey`、四模式 `EventBus`
  （Observe / Waterfall / Parallel / Serial）、可逆注册 `Disposer`、
  `ScopedRegistry`
- `echo-module` 0.1.0：manifest + 生命周期 + registry。M2 已去业务化
  ——`kind` 是开放标签（`String`），`MountContext` 只剩 `ctx`
- `echo-module-loader` 0.1.0：profile → bundle → patch 三层配置合成 + provenance
  + 依赖排序
- `echo-module-host` 0.1.0：`Transport`（inproc / stdio）+ `ModuleSupervisor`
  （握手 / 调用 / 取消 / 排空 / 崩溃重启 / 资源限额）+ conformance 套件
  （inproc 14 + stdio 10 + upgrade 10 + bootstrap 9 + host_call 3）
- `framework-gates`：新增 `domain_agnostic` 门禁（4 项）

### 变更

- `no_global_state` 门禁改为**精确文件豁免**：`crates/echo-context/src/kernel.rs`
  是宿主侧应用单例的收敛点（8 处调用全在应用层），终态应搬到宿主侧；白名单
  数量被 `whitelist_stays_minimal` 锁死为 1，棘轮不松

### 已知边界

- 协议贡献类型仍带领域分类（`Contribution::Tool` / `Skill` / `ModuleInstance`，
  wire tag 保留历史名 `adapter`），去业务化排在 M5，需 2 个以上真实外部使用者
  才启动
- `echo-context::kernel` 的搬迁（见上）

## [0.1.1] — 2026-10-19

只加**仓库自守门禁**，api / sdk 的代码与协议一字未改（无 API 变化）。

### 新增

- `crates/framework-gates`（零依赖，仅测试）：
  - `no_core_dependency`：框架 crate 只能有 `sdk → api` 一条 echo-* 依赖边；
    出现 `echo-defs` 这类领域 crate 即红
  - `no_global_state`：库代码零进程级单例（`static` / `OnceLock` / `LazyLock` /
    `lazy_static` / `thread_local` / `static mut`）
  - 两条各带自检，防止门禁退化成恒真

### 为什么

框架 crate 从 EchoAgentCore 迁出后，宿主仓的 `no_global_state` 与依赖方向扫描
以那边的 `source/` 为根——**保护随迁移静默消失**。本仓必须自建等价断言，否则
「框架不碰应用」只是文档里的一句话。

两条门禁都实测过能真报错（注入违规 → FAILED；撤销 → 全绿）。

## [0.1.0] — 2026-10-19

首个版本，从 EchoAgentCore 迁出（`source/module/echo-module-{api,sdk}`），
代码与行为**零改动**——本次发布只是让它们能被外部独立编译。

### 包含

- `echo-module-api` 0.1.0：冻结协议。协议名 `echo-module`，`PROTOCOL_VERSION = 1`。
  - 消息：`Hello` / `Welcome` / `Register` / `Ready` / `Invoke` / `InvokeResult` /
    `Cancel` / `Drain` / `Dispose` / `Emit` / `Event` / `Log` / `HostCall` /
    `HostCallResult` / `Failed`
  - 贡献类型：`Tool` / `Skill` / `Service` / `Events` / `ModuleInstance`
    （wire tag 为 `adapter`，为兼容已部署模块而保留）
  - 能力位：`tools` / `skills` / `services` / `events` / `cancel` / `host_call` / `adapter`
- `echo-module-sdk` 0.1.0：模块侧运行时（`serve_stdio` / `serve_io` /
  `InstanceModule` / `InstanceHandler` / `HostClient`），re-export `echo_module_api`。

### 已知边界

- 宿主侧（`echo-module-host` / `echo-module` / `echo-module-loader`）与 `echo-context`
  **尚未迁入**，计划 M3。在此之前本仓不能单独跑通端到端示例。
- `Contribution` 枚举仍包含应用领域分类（`Tool` / `Skill` / `ModuleInstance`）。
  机制本身与领域无关，但协议表面承认了这些分类；去业务化留待 M5 评估。
