# Changelog

本仓遵循 [语义化版本](https://semver.org/lang/zh-CN/)。0.x 期间允许 breaking，
但每一处都记在这里。`PROTOCOL_VERSION` 是**线上契约**版本，与 crate 版本独立
（判定规则见 [README](./README.md#版本与兼容)）。

## [0.14.0] — 未发布（breaking）

### 移除：贡献分类里的领域词汇

`Contribution` 曾有五个变体，其中三个是**应用领域分类**：面向模型的工具声明
（含 JSON Schema、UI 分类、"是否产生平台可见回复"）、技能目录（`SKILL.md`
路径）、聊天平台插件实例（平台名、能力位、会话标签、UI 描述符）。

2026-11 审计确认它们**零真实使用者**：EchoAgentCore 已经不依赖本仓（用自己
仓库内的副本），本仓内部只有测试与 conformance 夹具在构造它们。删掉的是：

- `Contribution::{Tool, Skill, PluginInstance}`
- `ToolContribution` / `SkillContribution` / `InstanceContribution` /
  `InstanceCapabilities`
- SDK 的 `InstancePlugin` / `InstanceHandler` / `serve_instance_stdio`
- `capabilities::{TOOLS, SKILLS, PLUGIN_INSTANCE}`

### 新增：`Contribution::Custom`

领域词汇的**唯一入口**：

```rust
Contribution::Custom {
    custom_kind: String,   // 宿主自己的分类（"report_generator" / 任何字符串）
    name: String,          // Invoke 寻址名（空 = 不参与寻址）
    payload: Value,        // 宿主与插件自行约定，框架不校验
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

删变体通常是线上格式变更。这次没 bump，理由可验证：**没有任何真实插件发送过
那三个分类**（唯一的生产者是 echo-server-core 的面板插件，它只发 `Service`）。
新宿主能解的帧集合小了一圈，但实际在跑的插件一个都没受影响。

若将来发现确有外部插件在用，正确做法不是回退，而是让它改发 `Custom`——那才是
它本来就该用的形状。

### 门禁

- 领域词审计不再排除 `echo-plugin-api` / `echo-plugin-sdk`（此前排除等于这两个
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

- **`CallContext::caller`**：本次回呼的来源插件 id，**由 supervisor 从连接归属
  填写**，线上协议里没有这个字段 —— 插件无法伪装成别的插件。宿主服务据此做来源
  授权与审计。同步 trait 没有这个参数（签名不变，向后兼容）。

- **`PluginHandle::wait_until(contribution, ctx, payload, timeout, predicate)`**：
  「握手完成」不等于「插件内部的服务真的起来了」。轮询 + 超时 + 判据这套就绪探测
  每个宿主都会写一遍，收进框架。

- **`PluginScope::snapshot()` → `PluginTreeSnapshot`**（可序列化）：把作用域 +
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
  两种形态 → 跑起来 → 写插件 → 装进宿主 → 怎么组织应用 → 文档」的短结构。
- 新增 [`document/structure.md`](./document/structure.md)「组织一个应用」：四层结构、
  五条纪律、一段代码该放哪的判据、两个反例、可搬的最小骨架。
- 修 `document/quickstart.md` 里过期的 `tag = "v0.7.0"`。
- 修 `document/crates.md` 流程图里的 `-.门禁.->` 虚线语法——ProDoc 只支持
  `-->` 与 `-->|标签|`，那两行原本会被解析器跳过。
- 对齐 `examples/README.md` 与 `docs/getting-started.md` 的行数与预期输出。

## [0.11.2] — 2026-11-28

### 新增

- `PluginHandle::upgrade_preserving_contributions`：面向“宿主在启动期物化贡献注册表”的兼容热替换。新实例握手后、流量切换前比对完整贡献集合；工具/schema/实例等贡献有任何变化即关闭 replacement、保留旧实例并返回明确错误，避免只换进程造成宿主注册表与插件能力漂移。

## [0.11.1] — 2026-11-21

### 新增

- `ServiceSlot<T>`：**在 `mount` 时解析、在处理时取用**的服务槽。

  插件的依赖常常是这样的形状：只有 `mount` 期间能查服务表（`MountContext`），
  而真正用到宿主能力的地方是命令处理器 / 事件回调——那些闭包在构造插件时就
  得交出去。手写 `Arc<RwLock<Option<T>>>` + "卸载时清空"在每个插件里都长得
  一样，而且极易漏掉清空（拿旧宿主的句柄继续跑）。

  `slot.bind(mctx, &KEY)?` 一步完成：解析（缺失即 `MissingService`）+ 记录
  + 返回"卸载清空"的 `Disposer`。

## [0.11.0] — 2026-11-21

**插件零捕获**：`MountContext` 现在能自己解析宿主提供的能力，于是
`mount` 闭包不必再捕获 `Arc<dyn SomeHost>` 这类运行期句柄，插件构造退化成
`plugin(version)`。

### 新增

- `MountContext::service<T>(&ServiceKey<T>) -> Option<T>`：从服务表解析；
- `MountContext::require<T>(&ServiceKey<T>) -> Result<T, PluginMountError>`：
  缺失即挂载失败，错误带 `available` 全部键名；
- `MountContext::context() -> Option<&Arc<Ctx>>`：需要 `provide` 或条件解析时
  取服务表本身。

### 为什么这属于框架

「宿主把句柄塞进闭包」与「插件按键解析」是**同一件事的两种写法**，但后者
让**目录**（谁存在）与**接线**（各自需要什么）解耦：组合根可以写
`push(catalog, tools::plugin(version))`，把「哪个 trait 对象给哪个插件」交给
服务表。这是 dsh `inject` 的**取用侧**——`inject()` 只声明，不取用。

### 不变

`MountContext` 的既有字段与构造方式一字未动（新方法都是附加的）。
裸注册表（无 `Ctx`）语义不变：`service` 回 `None`、`require` 回
`MissingService{available: []}`，不 panic。

## [0.10.0] — 2026-11-20

**运行态上收**：把「宿主为了用这套框架而必须自己维护的状态」收进注册表。
动机很具体——把内置能力逐一搬成插件之后，下游宿主（`echo-agent`）里那个
`PluginHost` 还剩 1,559 行，其中一大半是在**重新实现框架本该有的运行态**：

| 宿主自己维护的 | 为什么本来是框架的 |
|---|---|
| 缓存的 `MountContext` | `set_enabled` / `restart` / `reconcile` 每次都要它，宿主只能自己存一份 |
| 事件订阅**兴趣集** | `emit` 是热路径（流式正文每 token 一条），没人订阅时要能一次原子读就返回 |
| 投递**重入保护** | 插件的 `on_event` 里再 `emit` 会自递归 |
| 投递**计数** | 排查「事件为什么没到」的第一手证据 |

四者都与领域无关，任何宿主都会需要。

### 新增

- `PluginRegistry::set_mount_ctx(ctx)` / `mount_ctx()`：缓存挂载上下文；
- `PluginRegistry::enable(id, enabled)` / `restart_cached(id)` /
  `reconcile_cached()`：**不需要再传 `MountContext`**（用缓存的那份；
  未设置 = 默认上下文，裸注册表语义不变）；
- `PluginRegistry::has_event_subscribers()` / `wants_event(name)`：热路径
  两道闸门（一次原子读 / 一次查表，都不序列化）；
- `PluginRegistry::refresh_event_interest()`：重算兴趣集。**注册表自己在
  挂载 / 卸载 / 启停 / 重启 / reconcile 之后调用**——宿主不必记得；
- `PluginRegistry::deliver_event_guarded(event)`：投递 + 重入保护 + 计数；
- `PluginRegistry::event_counters()`：`(投递成功, 丢弃/失败)`；
- 新增私有模块 `runtime_state`（`RuntimeState` + `EventInterest`），带四项
  单元测试。

### 变更（语义澄清，非破坏）

- `set_enabled` 的**锁范围缩小**：原来持着 `enabled_state` 调 `mount`，而
  `mount` 现在会刷新兴趣集（也要读 `enabled_state`）——那是自锁。改为
  「读判断 → 释放 → 动作 → 写状态 → 刷新」。
- `enable(id, true)` 的语义明确写成**状态切换**（默认状态即"启用"，所以对
  从未挂载过的插件是 no-op）。初始挂载仍走 `mount` / `mount_all`。

### 影响

下游只需要删代码：宿主里那份缓存与兴趣集实现可以直接删掉，改为委托。
既有调用点（带 `&MountContext` 参数的版本）**全部保留**，不做破坏性改动。

## [0.9.0] — 2026-11-01

命令处理补上**异步入口**——v0.8 的 `handle_command` 是同步的，只能做"不等"的
事；真实命令域里几乎每条都要 IO（读盘、`spawn_blocking`、跨机查询）。

### 新增

- `Plugin::handle_command_async(name, payload) -> Option<BoxFuture<'a, Result<Value, String>>>`
  （默认把同步版本包成 ready future，所以只实现同步版的插件照常工作）；
- `PluginRegistry::dispatch_command_async(name, payload)`：宿主实际入口；
- `BuiltinPlugin::with_command_handler_async`。

### 为什么用 boxed future 而不是 `async fn`

插件以 `Arc<dyn Plugin>` 存在，trait 必须**对象安全**，而原生 `async fn` in
trait 不是对象安全的（AFIT 目前不支持 `dyn`）。`payload` 按值收是因为 future
不能借调用方的栈。

## [0.8.0] — 2026-11-01

补上最后两类"声明了没人读"的贡献：**命令处理**与**有序文本块**。至此协议里
的六类贡献（Tool / Skill / Service / Events / PluginInstance / Command / Text）
在 inproc 轨上都有真实消费方。

### 新增 — 命令处理（`Contribution::Command`）

- `CommandContribution { name, description }`：插件声明"这些命令我来处理"。
  命令名是**宿主词汇**（如 `RequestXxx`），框架只搬不解释。
- `Plugin::handled_commands()` + `Plugin::handle_command(name, payload)`：
  返回 `Some(Ok(v))` 处理了 / `Some(Err(e))` 认识但失败 / `None` 不认识。
- `PluginRegistry::dispatch_command()`：按注册序问已挂载插件；`None` = 无人
  处理（调用方走默认分支）。**只问已挂载的插件**——插件被禁用后命令自动回到
  "无人处理"，不需要在 dispatch 处特判。
- `PluginRegistry::command_owner()`：诊断用（某条命令当前归谁）。
- `PluginDescriptor.handled_commands`：面板能回答「这条命令谁在处理」。

### 新增 — 有序文本块（`Contribution::Text`）

- `TextBlockContribution { key, label, order }`：只声明**身份与顺序**。
- `Plugin::text_blocks()`（声明）+ `Plugin::text_block_content(key, context)`
  （内容）——**两个方法分开是有意的**：描述符生成要能随时调用（纯读、无副
  作用），而内容依赖上下文（同一块在不同会话下可以不同）。
- `PluginRegistry::collect_text_blocks(context)`：收集已挂载插件的块，按
  `order` 升序（等值保持注册序），**内容为空的块被丢掉**。
- `PluginDescriptor.text_blocks`：面板能回答「这个插件贡献了哪些段落」。

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

- `PluginEvent { plugin, name, payload }`：一条插件事件。`plugin` 由**宿主**
  填充（注册表在挂载时把 id 注入上下文），插件自己 emit 时不必知道 id。
- 插件 → 宿主：`MountContext.events: Option<EventSink>` + `MountContext::emit`。
  **没有出口 = 静默丢弃**（降级不是错误）——单测与无 Panel 的部署不需要它。
- 宿主 → 插件：`Plugin::subscribed_events()`（默认空）+ `Plugin::on_event()`
  （默认 `Ok`）+ `PluginRegistry::deliver_event()`（按名分发，`"*"` =
  `ALL_EVENTS` 全收）。**只投给已挂载的插件**（未挂载不该有副作用，同款原则）；
  `on_event` 返回 `Err` 只计数不卸载——事件是观察通道，不是控制通道。
- `BuiltinPlugin::with_subscriptions` / `with_event_handler`。
- `MountContext.plugin_id`：本插件的 id（顺带让插件能自己打带 id 的日志）。
- 注册表自己在挂/卸时发 `plugin.mounted` / `plugin.unmounted`（带 `id` /
  `depth`）——插件树的变化因此可观测。
- `PluginScope` 的子作用域**继承事件出口**：整棵树汇到同一接收方，靠 `depth`
  区分来源。
- `PluginDescriptor.subscribed_events`：面板能回答「这个插件会对什么有反应」。

### 为什么 emit 走 `MountContext` 而不是 trait 方法

`emit` 是**挂载期之后也要能用**的能力（插件在运行期随时通知宿主），而
`Plugin` 的 trait 方法拿不到 `MountContext`。把 sink 塞进上下文、让插件在
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

两块缺失的 dsh 语义补齐：**声明式 `inject` + 就绪等待**，以及**插件树 / 父 ctx**
（父插件宿主化子插件、卸载级联）。v0.5.0 的 `required_services` 升级为完整的
注入声明，旧写法继续生效（默认实现读 `inject().required`）。

### 新增 — 声明式注入

- `Inject { required, optional }`（`echo-plugin`）：
  - `required` 缺失 → 拒绝挂载（`PluginMountError::MissingService`，**mount
    闭包不执行**）；
  - `optional` 只作声明（面板展示 / 宿主按需读），**不参与**挂载判定。
- `Plugin::inject()` 是唯一真值；`Plugin::required_services()` 降为
  `inject().required` 的便捷读法（兼容 v0.5.0 的覆写者）。
- `BuiltinPlugin::with_inject` / `with_required_services`（后者等价于
  `with_inject(Inject::new(..))`）。
- `PluginDescriptor` 新增 `optional_services` / `pending_services`
  （`#[serde(default)]`）：面板能回答「它要什么」与「此刻还缺什么」——后者与
  `error` 不同，是"事实"而非"上次尝试的结果"。

### 新增 — 就绪等待与依赖变更

- `Ctx::wait_for_keys(&[String], timeout)`（`echo-context`）：等一组键**全部**
  就绪，命中立即返回、超时给 `ServiceError::Missing{key, available}`。
- `PluginRegistry::mount_when_ready(id, ctx, timeout)`：
  **先等后挂**（不是"失败后重试"），中间不留空窗；`timeout = ZERO` 等价于
  `mount`。
- `PluginRegistry::mount_all_when_ready(ctx, timeout)`：启动期按注册顺序批量
  等待挂载。
- `PluginRegistry::reconcile(&ctx) -> ReconcileReport`：依赖变动后重新核对——
  pending 里依赖已齐的挂上、已挂载但依赖消失的卸下并重新进 pending。**只看
  事实不看意图**：人主动禁用的、从没挂过也没 pending 记录的，一律不动。
- `PluginRegistry::pending_inject()`：`id → 缺的键`，面板"待挂载"视图用。

### 新增 — 插件树 / 父 ctx（dsh `ctx.plugin()` 的对应物）

- `Ctx::child()` / `parent()` / `depth()`：**父子作用域**。父链继承、子遮蔽父、
  子注册对父不可见。`resolve` / `contains` / `keys` / `service` 全部沿父链查找。
- `Ctx::on_change(f) -> Disposer` + `Change::{Provided, Disposed}`：变更通知
  **沿父链向上传播**（根能看见整棵子树）；等待在**整个家族内共享唤醒源**
  （子等父提供的服务不会漏唤醒）。
- `PluginScope`（新模块 `scope`）：一层 = 一个 `Ctx` 作用域 + 一个
  `PluginRegistry`。`PluginScope::root(ctx)` 建根，`mount` / `mount_when_ready`
  / `unmount` / `shutdown` / `walk` / `children_of`。
- `MountContext::nest()` / `MountContext::tree`：插件在自己的 `mount` 里建
  **子作用域**并挂子插件。归属靠挂载期压栈记录（`mounting` 栈），不靠插件自报；
  `unmount(父)` 会 `shutdown()` 它名下的子作用域（**递归**，子先于父）。
- 无树把手的裸注册表：`nest()` 返回 `None`，挂载照常成功——树是**可选**能力。

### 修复

- **`Plugin::unmount()` 此前从未被调用**：`PluginRegistry::unmount` 撤完
  disposer 就返回，插件覆写的 `unmount`（"disposer 之外的清理"）成了死代码。
  现在 disposer 撤完会调用它。顺带把 disposer 撤销顺序改成**逆序**（后注册的
  先撤），与嵌套资源直觉一致。
- `Ctx::register` 的 disposer 此前直接改表、不广播：现在走
  `remove_and_wake`，订阅者与等待者都能看见"注销"。

### 为什么不把 inject 类型化

服务键的**存在性**是 `echo-context` 的概念；键背后的类型是宿主与插件的私事。
框架若要求类型化，就得认识宿主的 trait（`echo-plugin` 零领域依赖的边界立刻破），
或者要求插件之间互相 `use` 对方的类型（插件就不再可独立替换）。存在性校验已
覆盖真实故障：注册顺序错、服务忘了注册、服务被提前注销。

## [0.5.0] — 2026-11-01

补上 in-process 插件缺失的两块机制：**配置下发**与**服务键依赖校验**。
此前 `plugins.toml` 的 `[plugin.config]` 只能到子进程插件的 `Hello.config`，
内置插件的 `mount` 拿不到自己的配置；`Contribution::Service` 也只是一个
"声明了没人读"的字段。

### 新增

- `MountContext.config: serde_json::Value`（+ `with_config` / `config()` /
  `config_str(key)`）：宿主按插件 id 注入该插件的配置，**缺省 `Null` 而非
  `{}`**——插件要能区分「没配」与「配了空表」。框架不解析内容、不做 schema
  校验，那是宿主与插件之间的契约。
- `PluginRegistry::set_config` / `set_configs` / `config_for`：组合根在挂载前
  登记配置；`mount()` 按 id 合并进传给该插件的 `MountContext`。运行期重新
  挂载（`set_enabled(true)` / `restart`）取**当时**登记的值。
- `Plugin::required_services() -> Vec<String>`（默认空）+ `BuiltinPlugin::
  with_required_services`：插件声明「挂我之前这些服务键必须在 `Ctx` 里」。
  `PluginRegistry::mount` 在跑 mount 闭包**之前**逐个 `Ctx::contains` 校验，
  缺失即 `PluginMountError::MissingService { key, available }`——
  **mount 闭包根本不会执行**（依赖没就绪就不该产生副作用）。
  这与 dsh 的 `inject` 同义，只是以存在性校验的形式落地（框架不认识具体类型）。
- `PluginDescriptor.required_services`（`#[serde(default)]`）：面板据此回答
  「这个插件为什么不挂载」。

### 为什么 `required_services` 用 `Vec<String>` 而不是类型化

服务键的**存在性**是 `echo-context` 的概念（字符串 + 是否有），而"这个键背后
是什么类型"是宿主与插件之间的私事。框架若要求类型化，就得认识宿主的 trait
（`echo-plugin` 零领域依赖的边界就破了）。存在性校验已经能挡住真实故障：
组合根把服务注册挪到挂载之后、或忘了注册——启动期立刻失败并列出可用服务，
而不是"插件挂载成功、运行期才发现依赖不存在"。

[0.6.0]: #060--2026-11-01
[0.5.0]: #050--2026-11-01

## [0.4.1] — 2026-10-19

新增**跨语言插件示例与测试**——把「协议即 ABI」从说法变成可运行的事实。

### 新增

- `examples/minimal-plugin-py/plugin.py`：纯 Python 插件，约 150 行，**零 Rust
  依赖**。手写帧（4 字节小端长度前缀 + JSON），不用 SDK。文件头把线上协议完整
  写了一遍——帧格式、握手顺序（`Hello` → `Welcome` → `Register` → `Ready`）、
  消息形状、生命周期（`drain` / `dispose` / `cancel` / stdin EOF），可直接当别的
  语言的参考实现。
- `crates/echo-plugin-host/tests/cross_language.rs`（3 项）：
  - `python_plugin_speaks_the_protocol`：真 spawn Python 进程，验握手 + 贡献
    解析 + 调用往返 + 未知贡献返回结构化错误 + drain 优雅退出
  - `python_plugin_rejects_mismatched_protocol_version`：协议版本 999 必须被
    拒绝（fail-closed），不是"先连上再说"
  - `python_plugin_frames_parse_with_the_rust_types`：直接对帧做断言（不经
    supervisor），便于区分"协议问题"与"监督逻辑问题"
  - 环境无 `python3` 时**跳过而非失败**
- CI 新增跨语言步骤。

### 为什么值得一条测试

「不锁定语言」是这套框架相对 Node 生态插件框架的实质优势，但写在文档里只是说法。
Python 插件真发一遍帧，才算证据——顺带把 M4 修掉的 `ServiceContribution` 重复键
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
    `Service` 贡献的插件跑在 inproc 上。M4 的最小示例（唯一同时用「Service
    贡献 + stdio 轨」的组合）把它撞出来了。
  - 修法：wire 字段改名 `service_kind`（此前这种帧**根本解析不了**，零兼容成本）。
  - 防复发：新增 `every_contribution_variant_roundtrips_through_json`
    （所有变体都过一遍 JSON）与 `service_contribution_wire_field_avoids_tag_collision`
    （断言 JSON 里 `"kind"` 只出现一次）。

### 新增

- `examples/minimal-host`（130 行）+ `examples/minimal-plugin`（70 行）：
  只用框架六件套的最小宿主与插件，**不引入** `echo-defs` / `echo-agent`。
  宿主内部带 `assert!`，跑通即证明框架可独立使用；CI 里作为独立步骤执行。
- `docs/getting-started.md`：第三方快速上手（写插件 / 拉起来 / 同进程轨 /
  崩溃隔离 / 跨语言插件 / 版本策略）。
- `examples/README.md`：预期输出与「每个示例在证明什么」。

## [0.3.0] — 2026-10-19

**breaking**：移除 `echo_context::kernel`（进程级单例表，`pub mod kernel`）。
框架仓的「零全局态」从此无条件成立。

### 移除

- `echo-context::kernel`（`set` / `get` / `get_or_init` 三个函数 + 那张
  `TypeId` 键控表）。它已搬到宿主侧：EchoAgentCore 的 `echo-bootstrap` crate。

### 为什么搬走

1. **框架自己一处不用**——六个 crate 里零调用；8 处使用者全在应用层
   （`agent_manager` / `plugin_host` / `global_policy` / `shell_manager` /
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

迁入宿主侧四件套，**六件套齐备**——本仓现在可以独立跑通端到端插件链路。
从 EchoAgentCore 逐字节搬迁，代码与行为零改动。

### 新增

- `echo-context` 0.1.0：`Ctx` 服务定位、强类型 `ServiceKey`、四模式 `EventBus`
  （Observe / Waterfall / Parallel / Serial）、可逆注册 `Disposer`、
  `ScopedRegistry`
- `echo-plugin` 0.1.0：manifest + 生命周期 + registry。M2 已去业务化
  ——`kind` 是开放标签（`String`），`MountContext` 只剩 `ctx`
- `echo-plugin-loader` 0.1.0：profile → bundle → patch 三层配置合成 + provenance
  + 依赖排序
- `echo-plugin-host` 0.1.0：`Transport`（inproc / stdio）+ `PluginSupervisor`
  （握手 / 调用 / 取消 / 排空 / 崩溃重启 / 资源限额）+ conformance 套件
  （inproc 14 + stdio 10 + upgrade 10 + bootstrap 9 + host_call 3）
- `framework-gates`：新增 `domain_agnostic` 门禁（4 项）

### 变更

- `no_global_state` 门禁改为**精确文件豁免**：`crates/echo-context/src/kernel.rs`
  是宿主侧应用单例的收敛点（8 处调用全在应用层），终态应搬到宿主侧；白名单
  数量被 `whitelist_stays_minimal` 锁死为 1，棘轮不松

### 已知边界

- 协议贡献类型仍带领域分类（`Contribution::Tool` / `Skill` / `PluginInstance`，
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

首个版本，从 EchoAgentCore 迁出（`source/plugin/echo-plugin-{api,sdk}`），
代码与行为**零改动**——本次发布只是让它们能被外部独立编译。

### 包含

- `echo-plugin-api` 0.1.0：冻结协议。协议名 `echo-plugin`，`PROTOCOL_VERSION = 1`。
  - 消息：`Hello` / `Welcome` / `Register` / `Ready` / `Invoke` / `InvokeResult` /
    `Cancel` / `Drain` / `Dispose` / `Emit` / `Event` / `Log` / `HostCall` /
    `HostCallResult` / `Failed`
  - 贡献类型：`Tool` / `Skill` / `Service` / `Events` / `PluginInstance`
    （wire tag 为 `adapter`，为兼容已部署插件而保留）
  - 能力位：`tools` / `skills` / `services` / `events` / `cancel` / `host_call` / `adapter`
- `echo-plugin-sdk` 0.1.0：插件侧运行时（`serve_stdio` / `serve_io` /
  `InstancePlugin` / `InstanceHandler` / `HostClient`），re-export `echo_plugin_api`。

### 已知边界

- 宿主侧（`echo-plugin-host` / `echo-plugin` / `echo-plugin-loader`）与 `echo-context`
  **尚未迁入**，计划 M3。在此之前本仓不能单独跑通端到端示例。
- `Contribution` 枚举仍包含应用领域分类（`Tool` / `Skill` / `PluginInstance`）。
  机制本身与领域无关，但协议表面承认了这些分类；去业务化留待 M5 评估。
