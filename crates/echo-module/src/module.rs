//! Module trait — the mount/unmount lifecycle seam.

use std::any::Any;
use std::sync::Arc;

use echo_context::Disposer;

use crate::manifest::ModuleManifest;
/// What a module may consume when mounting.
///
/// **只携带框架自己能解释的东西**：一个服务定位器 `Ctx`，加上**本模块的
/// 配置**（`modules.toml` 里 `[module.config]` 原样透传的 JSON）。框架不认识
/// "工具""技能"这类词汇——它们是**宿主的领域概念**，由宿主侧适配层在注册表
/// 里解释。
///
/// 历史（2026-10 M2）：这里曾有 `tools` / `skills` 两个字段，各配一个
/// `ModuleToolSink` / `ModuleSkillSink` trait。审计发现**零使用**——全仓
/// `mount` 闭包都是 `|_ctx| Ok(vec![])`，`with_skills` 从未被调用，
/// `register_tool` 零调用点。它们是"给未来外部工具模块预留的接缝"，
/// 而预留方式恰好把应用领域词汇写进了框架。删除后框架层零领域依赖。
///
/// 历史（2026-11 配置下发）：`config` 是**框架自己就能解释**的东西——它只是
/// 「一行配置」的 JSON 投影，不含任何领域词汇。此前 in-process 模块拿不到
/// 自己的配置（`modules.toml` 的 `[module.config]` 只送到子进程模块的
/// `Hello.config`），导致"改配置不改代码"对内置模块不成立。
///
/// [`Ctx::provide`]: echo_context::Ctx::provide
#[derive(Default, Clone)]
pub struct MountContext {
    /// Global service locator (services resolved by stable key).
    pub ctx: Option<Arc<echo_context::Ctx>>,
    /// 本模块的配置（`modules.toml` 的 `[module.config]` 对应的 JSON 对象；
    /// 未配置 = [`serde_json::Value::Null`]）。
    ///
    /// **内容由宿主与模块自行约定**，框架只按 `id` 分发、不看内部：
    /// - 缺省 `Null` 而不是 `{}`，这样模块能区分「没配」与「配了空表」；
    /// - 只读（`&self` 上的方法拿到的是 clone），模块改配置请走自己的
    ///   持久化面，别指望改这里能回写。
    pub config: serde_json::Value,
    /// 模块树把手：本模块**挂在哪个作用域**里（`None` = 裸注册表，无树）。
    ///
    /// 有它时模块可以在自己的 `mount` 里 [`MountContext::nest`] 出一个子
    /// 作用域，把子模块挂进去——卸载本模块时子作用域连带卸载（级联）。
    /// 这是 dsh `ctx.module()` 嵌套的对应物；**框架不认识"子模块"这个业务
    /// 概念**，只认"作用域归属"这一层结构。
    pub tree: Option<Arc<crate::ModuleScope>>,
    /// 本模块的 id（宿主在挂载时填；[`MountContext::emit`] 用它做来源标记）。
    ///
    /// 没填时为空串——裸 `ModuleRegistry` 的测试用法可以不管它。
    pub module_id: String,
    /// 事件出口（宿主注入；`None` = 没有接收方，[`MountContext::emit`] 丢弃）。
    ///
    /// 与子进程轨的 `Emit` 帧同语义：同步、即发即走、无返回值。
    pub events: Option<crate::EventSink>,
}

impl MountContext {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_ctx(mut self, ctx: Arc<echo_context::Ctx>) -> Self {
        self.ctx = Some(ctx);
        self
    }
    /// 附带本模块的配置（组合根的 `ModuleRegistry` 在 mount 时按 id 注入）。
    pub fn with_config(mut self, config: serde_json::Value) -> Self {
        self.config = config;
        self
    }
    /// 本模块的配置（未配置时为 `Null`）。
    pub fn config(&self) -> &serde_json::Value {
        &self.config
    }
    /// 取配置里的一个字符串字段（`null` / 缺失 / 非字符串一律 `None`）。
    ///
    /// 给"只改一两项"的模块省掉重复的 `as_str` 样板——注意它**不**做类型
    /// 校验报告，配错类型等同未配置（要严格校验请自行读 [`Self::config`]）。
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(|v| v.as_str())
    }
    /// 附带模块树把手（`ModuleScope::mount` 在挂载前注入）。
    pub fn with_tree(mut self, tree: Arc<crate::ModuleScope>) -> Self {
        self.tree = Some(tree);
        self
    }
    /// 本模块所在的作用域（裸注册表挂载时为 `None`）。
    pub fn tree(&self) -> Option<&Arc<crate::ModuleScope>> {
        self.tree.as_ref()
    }
    /// 发一条事件给宿主（`None` 出口 = 静默丢弃，不算错误）。
    ///
    /// 名字与载荷都归宿主与模块自行约定；框架只搬。**不要在 `mount` 之外
    /// 依赖它一定送达**：出口是宿主的实现细节（面板没连、事件总线正忙时
    /// 都可能丢），要可靠通知请走命令回执。
    pub fn emit(&self, name: impl Into<String>, payload: serde_json::Value) {
        if let Some(sink) = &self.events {
            sink(crate::ModuleEvent::new(
                self.module_id.clone(),
                name,
                payload,
            ));
        }
    }

    /// 本模块的 id（裸用法下为空串）。
    pub fn module_id(&self) -> &str {
        &self.module_id
    }

    /// 从服务表解析一个**宿主提供的能力**（`None` = 没有或类型不符）。
    ///
    /// 这是"模块不捕获运行期句柄"的那把钥匙：`mount` 闭包不再需要宿主把
    /// `Arc<dyn SomeHost>` 塞进构造参数，模块自己按**稳定的键**解析即可。
    /// 于是组合根里的模块构造退化成 `module(version)`——目录只描述"有哪些
    /// 模块"，不描述"它们各自需要哪些宿主的内部对象"。
    ///
    /// 与 [`Module::inject`] 的分工：`inject()` 是**声明**（注册表在跑 mount
    /// 闭包之前校验，缺了就不执行），本方法是**取用**。两者应当读同一批键，
    /// 但框架不做强制——声明是给编排与面板看的，取用是模块自己的事。
    pub fn service<T: Any + Send + Sync + Clone>(
        &self,
        key: &echo_context::ServiceKey<T>,
    ) -> Option<T> {
        self.ctx.as_ref()?.service(key)
    }

    /// 同 [`Self::service`]，但缺失即**挂载失败**（`MissingService`）。
    ///
    /// 用它取代"宿主保证会传进来"的隐式契约：拿不到就报错，且错误里带着
    /// `available` 全部键名——比 `expect("host not wired")` 有用得多。
    pub fn require<T: Any + Send + Sync + Clone>(
        &self,
        key: &echo_context::ServiceKey<T>,
    ) -> Result<T, ModuleMountError> {
        match &self.ctx {
            Some(ctx) => ctx.require(key).map_err(|error| match error {
                echo_context::ServiceError::Missing { key, available } => {
                    ModuleMountError::MissingService { key, available }
                }
                // `require` 只会回 Missing；其它情况保守地当缺服务处理。
                other => ModuleMountError::MissingService {
                    key: other.to_string(),
                    available: ctx.keys(),
                },
            }),
            None => Err(ModuleMountError::MissingService {
                key: key.name().to_string(),
                available: Vec::new(),
            }),
        }
    }

    /// 服务表本身（需要 `provide` 或做条件解析时用）。
    ///
    /// 裸注册表用法（单测）下为 `None`——调用方据此降级。
    pub fn context(&self) -> Option<&Arc<echo_context::Ctx>> {
        self.ctx.as_ref()
    }

    /// 为**本模块**建一个子作用域（子模块挂这里）；无树可用时 `None`。
    ///
    /// 只在 `mount` 期间有效（作用域要知道当前挂载的是谁，才能把子作用域
    /// 记在它名下、随它卸载而级联卸掉）。返回的作用域有独立的 `Ctx`
    /// （父 = 本模块所在作用域的 ctx）与独立注册表。
    pub fn nest(&self) -> Option<Arc<crate::ModuleScope>> {
        self.tree.as_ref()?.nest()
    }
}

/// 一个**在 `mount` 时解析、在处理时取用**的服务槽。
///
/// 模块的能力依赖常常是这样的：`mount` 期间才有 `MountContext`（因此才能查
/// 服务表），而真正用到宿主能力的地方是命令处理器 / 事件回调——那些闭包在
/// 构造模块时就得交出去，拿不到 `MountContext`。
///
/// 手写这段（`Arc<RwLock<Option<T>>>` + 卸载时清空）在每个模块里都长得一样，
/// 且极易漏掉"卸载要清空"（清不掉就会拿着旧宿主的句柄继续跑）。这里收成
/// 一个类型：
///
/// ```ignore
/// let host = ServiceSlot::new();
/// let bind = host.clone();
/// BuiltinModule::new(manifest, move |mctx| {
///     let keep = bind.bind(mctx, &HOST_KEY)?;   // 解析 + 记录 + 卸载清理
///     Ok(vec![keep])
/// })
/// .with_command_handler_async({
///     let host = host.clone();
///     move |name, payload| {
///         let host = host.clone();
///         Box::pin(async move {
///             let Some(host) = host.get() else { return None };
///             host.handle(name, payload).await
///         })
///     }
/// })
/// ```
///
/// **不缓存 `MountContext` 本身**：那会把整张服务表钉在模块里；这里只留模块
/// 声明过的那一个键。
pub struct ServiceSlot<T: Clone> {
    inner: Arc<std::sync::RwLock<Option<T>>>,
}

impl<T: Clone + Send + Sync + 'static> Default for ServiceSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> Clone for ServiceSlot<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> ServiceSlot<T> {
    /// 空槽。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// 当前绑定的值（未挂载 / 已卸载时为 `None`）。
    pub fn get(&self) -> Option<T> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 是否已绑定。
    pub fn is_bound(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// 手工绑定（不经服务表；单测与"宿主直接塞"的场景）。
    pub fn set(&self, value: T) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
    }

    /// 手工清空。
    pub fn clear(&self) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// 从挂载上下文解析并绑定；返回的 [`Disposer`] 在卸载时清空槽位。
    ///
    /// 缺失即 `MissingService`——**挂载失败比"挂着但什么都做不了"好**：
    /// 后者在运行期表现为"点了没反应"。
    pub fn bind(
        &self,
        mctx: &MountContext,
        key: &echo_context::ServiceKey<T>,
    ) -> Result<Disposer, ModuleMountError> {
        let value = mctx.require(key)?;
        self.set(value);
        let slot = self.clone();
        Ok(Disposer::from_fn(move || slot.clear()))
    }
}

/// Mount error kinds.
#[derive(Debug, thiserror::Error)]
pub enum ModuleMountError {
    #[error("mount failed: {0}")]
    Mount(String),
    /// 模块声明的服务键在 `Ctx` 里**不存在**——依赖未就绪。
    ///
    /// 单独成一类（而不是塞进 `Mount(String)`）是为了让宿主能原样转述
    /// "缺什么、有什么"：`available` 是该 `Ctx` 现有的全部服务键。
    #[error("required service `{key}` is not registered (available: {available:?})")]
    MissingService {
        /// 缺失的服务键。
        key: String,
        /// 当前 `Ctx` 里可用的服务键（排查用）。
        available: Vec<String>,
    },
    #[error("module kind {kind} is not supported by this host")]
    UnsupportedKind { kind: &'static str },
}

/// Result of a successful mount: disposers that undo the registration.
pub type ModuleMountResult = Result<Vec<Disposer>, ModuleMountError>;

/// 声明式依赖：本模块要用哪些服务键（dsh 的 `inject`）。
///
/// 两种语义，都由宿主实现：
///
/// - **required**（硬依赖）：挂载前必须都在。缺失 → 拒绝挂载
///   （[`ModuleMountError::MissingService`]，mount 闭包不执行）；
/// - **optional**（软依赖）：有则用、无则降级，只作为**声明**记录下来
///   （面板展示 / 宿主按需读），不参与挂载判定。
///
/// 为什么是服务键而不是类型：键的**存在性**是 `echo-context` 的概念，
/// 键背后的类型是宿主与模块的私事。框架认类型就得认识宿主的 trait，
/// `echo-module` 零领域依赖的边界立刻破掉（见 `framework-gates`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inject {
    /// 硬依赖：缺失即拒绝挂载。
    pub required: Vec<String>,
    /// 软依赖：仅声明（缺失不阻止挂载）。
    pub optional: Vec<String>,
}

impl Inject {
    /// 只要硬依赖。
    pub fn new<I, S>(required: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            required: required.into_iter().map(Into::into).collect(),
            optional: Vec::new(),
        }
    }

    /// 追加软依赖。
    pub fn with_optional<I, S>(mut self, optional: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.optional.extend(optional.into_iter().map(Into::into));
        self
    }

    /// 硬依赖里当前**缺失**的键（`ctx` 为 `None` 时全部算缺失）。
    pub fn missing(&self, ctx: Option<&Arc<echo_context::Ctx>>) -> Vec<String> {
        match ctx {
            Some(ctx) => self
                .required
                .iter()
                .filter(|key| !ctx.contains(key))
                .cloned()
                .collect(),
            None => self.required.clone(),
        }
    }

    /// 硬依赖是否全部就绪。
    pub fn is_ready(&self, ctx: Option<&Arc<echo_context::Ctx>>) -> bool {
        self.missing(ctx).is_empty()
    }

    /// 是否什么都没声明。
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.optional.is_empty()
    }
}

/// 命令处理的 future（`Module::handle_command_async` 的返回项）。
///
/// 别名只为可读：裸写 `Option<Pin<Box<dyn Future<..> + Send + 'a>>>` 会被
/// clippy 判"类型过于复杂"，而它出现在 trait 方法与实现两处。
pub type CommandFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
>;

/// 模块贡献的一块**有序文本**（宿主装配提示词/文档等"拼接型内容"时收集）。
///
/// 框架不知道"提示词"是什么——它只知道"有一个键、一个标题、一段文本、一个
/// 顺序"。宿主的装配逻辑按 `order` 排序、按 `key` 去重即可。
///
/// 为什么内容在运行期取（而不是挂在 [`Contribution::Text`] 上）：同一模块
/// 可能因上下文不同给出不同文本（典型场景：宿主按"当前是哪个会话"拼不同的
/// 上下文段落）。声明只固定身份与顺序，文本按需产生。
///
/// [`Contribution::Text`]: echo_module_api::Contribution::Text
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleTextBlock {
    /// 块键（去重/覆盖用；建议 `<模块>:<块名>`）。
    pub key: String,
    /// 展示标题（空 = 用 `key`）。
    pub label: String,
    /// 排序权重（小在前）。
    pub order: i32,
}

impl ModuleTextBlock {
    /// 造一个声明。
    pub fn new(key: impl Into<String>, label: impl Into<String>, order: i32) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            order,
        }
    }
}

/// A mountable module.
pub trait Module: Send + Sync {
    fn manifest(&self) -> &ModuleManifest;

    /// Mount the module: perform all registrations, returning disposers.
    /// The registry holds these and calls them on unmount.
    fn mount(&self, ctx: &MountContext) -> ModuleMountResult;

    /// Extra cleanup beyond disposers (default: nothing).
    fn unmount(&self) -> Result<(), ModuleMountError> {
        Ok(())
    }

    /// 本模块的**声明式依赖**（服务键；默认空 = 无要求）。
    ///
    /// 这是模块表达「我需要什么」的唯一声明面：
    /// - [`ModuleRegistry::mount`] 立即校验 required，缺失即
    ///   [`ModuleMountError::MissingService`]，**mount 闭包不执行**；
    /// - [`ModuleRegistry::mount_when_ready`] 先等它就绪（带超时）再挂；
    /// - [`ModuleRegistry::reconcile`] 在依赖变动后据此重挂 / 卸下。
    ///
    /// 为什么在框架层：服务键是 `echo-context` 的概念（字符串 + 存在性），
    /// 不含任何应用领域词汇；"依赖没到就不启动、到了就启动"是模块机制的
    /// 基本语义（dsh 的 `inject`）。此前内联模块只能靠组合根手工编排启动
    /// 顺序，顺序错了毫无反馈——模块照样"挂载成功"，直到运行期才 NPE。
    ///
    /// [`ModuleRegistry::mount`]: crate::ModuleRegistry::mount
    /// [`ModuleRegistry::mount_when_ready`]: crate::ModuleRegistry::mount_when_ready
    /// [`ModuleRegistry::reconcile`]: crate::ModuleRegistry::reconcile
    fn inject(&self) -> Inject {
        Inject::default()
    }

    /// 本模块想订阅的**事件名**（默认空 = 不订阅）。
    ///
    /// 与协议里 `Contribution::Events { subscribe }` 同义。`["*"]`（
    /// [`crate::ALL_EVENTS`]）表示全收。宿主经
    /// [`ModuleRegistry::deliver_event`] 按名分发，只有声明过的才会收到。
    ///
    /// [`ModuleRegistry::deliver_event`]: crate::ModuleRegistry::deliver_event
    fn subscribed_events(&self) -> Vec<String> {
        Vec::new()
    }

    /// 收到一条订阅过的事件（默认忽略）。
    ///
    /// 返回 `Err` 表示"这条我没处理成"：宿主会把它计数成投递失败，**不会**
    /// 卸载模块（事件是观察通道，不是控制通道）。
    fn on_event(&self, event: &crate::ModuleEvent) -> Result<(), String> {
        let _ = event;
        Ok(())
    }

    /// 本模块**处理哪些命令**（默认空 = 不处理）。名字是宿主词汇。
    ///
    /// 宿主的分发路径：先问注册表"这个命令谁处理"，再落自己的默认分支。
    /// 于是"某个命令域归某个模块"是**注册关系**而不是硬编码分支——模块不在，
    /// 命令自然无人处理（宿主回明确错误，不是静默超时）。
    fn handled_commands(&self) -> Vec<String> {
        Vec::new()
    }

    /// 处理一条命令（默认 `None` = 我不认识这个命令）。
    ///
    /// 返回 `Some(Ok(v))` = 我处理了，结果在这里；`Some(Err(e))` = 我认识但
    /// 处理失败（宿主把 `e` 原样回给前端）；`None` = 不归我管（宿主继续找）。
    ///
    /// 参数与返回值都是自由 JSON：命令类型是宿主与模块之间的契约，框架不搬。
    fn handle_command(
        &self,
        name: &str,
        payload: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        let _ = (name, payload);
        None
    }

    /// 处理一条命令（**异步**入口，宿主实际调用这个）。
    ///
    /// 默认实现把同步版本包成 ready future——所以只实现 [`Self::handle_command`]
    /// 的模块照常工作。命令处理里出现 IO（读盘 / `spawn_blocking` / 跨机查询）
    /// 时覆写本方法。
    ///
    /// 为什么是 boxed future 而不是 `async fn`：模块以 `Arc<dyn Module>` 存在，
    /// trait 必须对象安全，而原生 async trait 方法不是。`payload` 按值收是因为
    /// future 不能借调用方的栈。
    fn handle_command_async<'a>(
        &'a self,
        name: &'a str,
        payload: serde_json::Value,
    ) -> Option<CommandFuture<'a>> {
        self.handle_command(name, &payload)
            .map(|outcome| -> CommandFuture<'a> { Box::pin(async move { outcome }) })
    }

    /// 本模块贡献的**文本块声明**（键 / 标题 / 顺序；默认空）。
    ///
    /// 只做声明、**不产生文本**——因此可以在任何时刻调用（面板列举、宿主
    /// 装配前预判顺序），不会因为上下文缺失而给出错误内容。
    fn text_blocks(&self) -> Vec<ModuleTextBlock> {
        Vec::new()
    }

    /// 取某个块的**文本**（`None` = 这个上下文下没有内容）。
    ///
    /// `context` 是宿主给的上下文（自由 JSON；如 `{"session_id": "..."}`）——
    /// 框架不解释它，只负责透传。这正是"声明"与"内容"分开的原因：同一个块在
    /// 不同会话下可以给出不同文本。
    fn text_block_content(&self, key: &str, context: &serde_json::Value) -> Option<String> {
        let _ = (key, context);
        None
    }

    /// 硬依赖的键（[`Self::inject`] 的便捷读法：`self.inject().required`）。
    ///
    /// 保留它是**兼容 v0.5.0 的接缝**：那一版只有 `required_services`，
    /// 现有实现覆写的是它。默认实现读 `inject().required`，所以两种写法
    /// 都生效、且只有一个真值。
    fn required_services(&self) -> Vec<String> {
        self.inject().required
    }
}

/// A ready-made module for built-in modules: manifest + a mount closure.
pub struct BuiltinModule {
    manifest: ModuleManifest,
    mount_fn: Box<dyn Fn(&MountContext) -> ModuleMountResult + Send + Sync>,
    inject: Inject,
    subscriptions: Vec<String>,
    event_handler: Option<EventHandler>,
    commands: Vec<String>,
    command_handler: Option<CommandHandler>,
    async_command_handler: Option<AsyncCommandHandler>,
    text_blocks: Option<TextBlockSource>,
    text_content: Option<TextContentSource>,
}

/// 事件处理闭包（`BuiltinModule::with_event_handler` 的入参）。
///
/// 别名只为让字段声明可读（裸 `Option<Box<dyn Fn(..) -> Result<..>>>` 会被
/// clippy 判"类型过于复杂"）。
type EventHandler = Box<dyn Fn(&crate::ModuleEvent) -> Result<(), String> + Send + Sync>;

/// 命令处理闭包（`BuiltinModule::with_command_handler` 的入参）。
///
/// 返回语义与 [`Module::handle_command`] 一致：`None` = 不认识，交给下一个。
type CommandHandler = Box<
    dyn Fn(&str, &serde_json::Value) -> Option<Result<serde_json::Value, String>> + Send + Sync,
>;

/// 命令处理闭包（异步版；`BuiltinModule::with_command_handler_async` 的入参）。
///
/// 返回 boxed future 才能同时满足"trait 对象安全"与"处理里可以 await IO"。
type AsyncCommandHandler = Box<
    dyn Fn(
            String,
            serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>,
        > + Send
        + Sync,
>;

/// 提示词块声明来源（`BuiltinModule::with_text_blocks` 的入参）。
type TextBlockSource = Box<dyn Fn() -> Vec<ModuleTextBlock> + Send + Sync>;

/// 提示词块内容来源（`BuiltinModule::with_text_block_content` 的入参）。
type TextContentSource = Box<dyn Fn(&str, &serde_json::Value) -> Option<String> + Send + Sync>;

impl BuiltinModule {
    pub fn new(
        manifest: ModuleManifest,
        mount_fn: impl Fn(&MountContext) -> ModuleMountResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            manifest,
            mount_fn: Box::new(mount_fn),
            inject: Inject::default(),
            subscriptions: Vec::new(),
            event_handler: None,
            commands: Vec::new(),
            command_handler: None,
            async_command_handler: None,
            text_blocks: None,
            text_content: None,
        }
    }

    /// 声明订阅的事件名（见 [`Module::subscribed_events`]）。
    pub fn with_subscriptions<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.subscriptions = names.into_iter().map(Into::into).collect();
        self
    }

    /// 附带事件处理闭包（见 [`Module::on_event`]）。
    pub fn with_event_handler(
        mut self,
        f: impl Fn(&crate::ModuleEvent) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.event_handler = Some(Box::new(f));
        self
    }

    /// 声明本模块处理哪些命令，并附带处理闭包（见 [`Module::handle_command`]）。
    ///
    /// 两个参数分开是因为**声明可以先于实现**：宿主在装配期就能看到"这个模块
    /// 处理哪些命令"（面板展示 / 冲突检测），而闭包可以稍后接线。
    pub fn with_commands<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.commands = names.into_iter().map(Into::into).collect();
        self
    }

    /// 附带命令处理闭包（返回 `None` = 不认识，交给下一个处理者）。
    pub fn with_command_handler(
        mut self,
        f: impl Fn(&str, &serde_json::Value) -> Option<Result<serde_json::Value, String>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.command_handler = Some(Box::new(f));
        self
    }

    /// 附带**异步**命令处理闭包（需要 IO 的命令用这个）。
    ///
    /// 与 `with_command_handler` 二选一；都设时以本方法为准。
    pub fn with_command_handler_async<F>(mut self, f: F) -> Self
    where
        F: Fn(
                String,
                serde_json::Value,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>,
            > + Send
            + Sync
            + 'static,
    {
        self.async_command_handler = Some(Box::new(f));
        self
    }

    /// 声明提示词块（见 [`Module::text_blocks`]）。
    pub fn with_text_blocks<I>(mut self, blocks: I) -> Self
    where
        I: IntoIterator<Item = ModuleTextBlock>,
    {
        let blocks: Vec<ModuleTextBlock> = blocks.into_iter().collect();
        self.text_blocks = Some(Box::new(move || blocks.clone()));
        self
    }

    /// 附带提示词块**内容**来源（见 [`Module::text_block_content`]）。
    pub fn with_text_block_content(
        mut self,
        f: impl Fn(&str, &serde_json::Value) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.text_content = Some(Box::new(f));
        self
    }

    /// 声明本模块的依赖（见 [`Module::inject`]）：硬依赖 + 可选软依赖。
    pub fn with_inject(mut self, inject: Inject) -> Self {
        self.inject = inject;
        self
    }

    /// 只声明硬依赖（**兼容 v0.5.0**：等价于
    /// `with_inject(Inject::new(keys))`）。
    pub fn with_required_services<I, S>(self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_inject(Inject::new(keys))
    }
}

impl Module for BuiltinModule {
    fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    fn mount(&self, ctx: &MountContext) -> ModuleMountResult {
        (self.mount_fn)(ctx)
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn subscribed_events(&self) -> Vec<String> {
        self.subscriptions.clone()
    }

    fn on_event(&self, event: &crate::ModuleEvent) -> Result<(), String> {
        match &self.event_handler {
            Some(handler) => handler(event),
            None => Ok(()),
        }
    }

    fn handled_commands(&self) -> Vec<String> {
        self.commands.clone()
    }

    fn handle_command(
        &self,
        name: &str,
        payload: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        self.command_handler.as_ref()?(name, payload)
    }

    fn handle_command_async<'a>(
        &'a self,
        name: &'a str,
        payload: serde_json::Value,
    ) -> Option<CommandFuture<'a>> {
        if let Some(handler) = &self.async_command_handler {
            let name = name.to_string();
            return Some(handler(name, payload));
        }
        self.handle_command(name, &payload)
            .map(|outcome| -> CommandFuture<'a> { Box::pin(async move { outcome }) })
    }

    fn text_blocks(&self) -> Vec<ModuleTextBlock> {
        match &self.text_blocks {
            Some(source) => source(),
            None => Vec::new(),
        }
    }

    fn text_block_content(&self, key: &str, context: &serde_json::Value) -> Option<String> {
        self.text_content.as_ref()?(key, context)
    }
}

#[cfg(test)]
mod mount_context_service_tests {
    use super::*;
    use echo_context::{Ctx, ServiceKey};

    static HOST: ServiceKey<Arc<dyn Fn() -> u32 + Send + Sync>> = ServiceKey::new("test.host");

    /// `require` 拿到宿主注册的能力——模块因此**不需要**构造参数。
    ///
    /// 这条是"模块零捕获"的地基：`mount` 闭包里能取到 `ctx` 注册的一切，
    /// 于是组合根的 `module(version)` 签名可以不带任何 `Arc<dyn …>`。
    #[test]
    fn require_resolves_a_host_provided_capability() {
        let ctx = Arc::new(Ctx::default());
        let counter: Arc<dyn Fn() -> u32 + Send + Sync> = Arc::new(|| 7);
        let _keep = ctx.provide(&HOST, counter);

        let mctx = MountContext::new().with_ctx(ctx);
        let resolved = mctx.require(&HOST).expect("宿主应已注册");
        assert_eq!(resolved(), 7);
    }

    /// 缺失 → `MissingService`，且错误里带 `available`（排查用）。
    #[test]
    fn require_reports_missing_service_with_available_keys() {
        let ctx = Arc::new(Ctx::default());
        let _keep = ctx.provide(
            &HOST,
            Arc::new(|| 1u32) as Arc<dyn Fn() -> u32 + Send + Sync>,
        );

        let mctx = MountContext::new().with_ctx(ctx);
        let error = mctx
            .require(&ServiceKey::<u8>::new("test.absent"))
            .expect_err("未注册的键必须失败");
        match error {
            ModuleMountError::MissingService { key, available } => {
                assert_eq!(key, "test.absent");
                assert!(
                    available.iter().any(|k| k == "test.host"),
                    "available 应列出已有的键：{available:?}"
                );
            }
            other => panic!("unexpected: {other}"),
        }
    }

    /// 没有服务表（裸注册表 / 单测）→ 也是 `MissingService`，不是 panic。
    #[test]
    fn require_without_a_context_fails_cleanly() {
        let mctx = MountContext::new();
        assert!(mctx.service(&HOST).is_none());
        assert!(matches!(
            mctx.require(&HOST),
            Err(ModuleMountError::MissingService { .. })
        ));
        assert!(mctx.context().is_none());
    }
}

#[cfg(test)]
mod service_slot_tests {
    use super::*;
    use echo_context::{Ctx, ServiceKey};

    static HOST: ServiceKey<Arc<dyn Fn() -> u32 + Send + Sync>> = ServiceKey::new("slot.test.host");

    /// 绑定后可取用；`Disposer` 落下即清空（卸载不留旧句柄）。
    #[test]
    fn bind_then_dispose_clears_the_slot() {
        let ctx = Arc::new(Ctx::default());
        let _keep = ctx.provide(
            &HOST,
            Arc::new(|| 9u32) as Arc<dyn Fn() -> u32 + Send + Sync>,
        );
        let mctx = MountContext::new().with_ctx(ctx);

        let slot: ServiceSlot<Arc<dyn Fn() -> u32 + Send + Sync>> = ServiceSlot::new();
        assert!(!slot.is_bound());

        let disposer = slot.bind(&mctx, &HOST).expect("宿主已注册");
        assert!(slot.is_bound());
        assert_eq!(slot.get().unwrap()(), 9);

        disposer.dispose();
        assert!(
            !slot.is_bound(),
            "卸载必须清空——否则会拿着旧宿主的句柄继续跑"
        );
    }

    /// 缺失即挂载失败（不留"挂着但什么都做不了"的半状态）。
    #[test]
    fn bind_fails_when_the_host_is_missing() {
        let ctx = Arc::new(Ctx::default());
        let mctx = MountContext::new().with_ctx(ctx);
        let slot: ServiceSlot<Arc<dyn Fn() -> u32 + Send + Sync>> = ServiceSlot::new();
        assert!(matches!(
            slot.bind(&mctx, &HOST),
            Err(ModuleMountError::MissingService { .. })
        ));
        assert!(!slot.is_bound());
    }

    /// 克隆共享同一个槽（mount 闭包与命令处理器各持一份）。
    #[test]
    fn clones_share_one_slot() {
        let a: ServiceSlot<u8> = ServiceSlot::new();
        let b = a.clone();
        a.set(3);
        assert_eq!(b.get(), Some(3));
        b.clear();
        assert_eq!(a.get(), None);
    }
}
