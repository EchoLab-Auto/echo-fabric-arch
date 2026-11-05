//! ModuleRegistry — discovery, lifecycle and state of all modules.

use std::collections::BTreeMap;
use std::sync::Arc;

use echo_context::Disposer;

use crate::manifest::ModuleManifest;
use crate::module::{Module, ModuleMountError, ModuleMountResult, MountContext};
use crate::runtime_state::RuntimeState;

/// Frontend-facing snapshot of a module's state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModuleDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: String,
    pub entry: String,
    pub author: String,
    /// Whether the module is currently mounted (active).
    pub enabled: bool,
    /// Whether the module ships inside the binary (vs. discovered from disk).
    ///
    /// **来源**维度（正交于 `runtime`）：门控 manifest 可以编译内置，
    /// 而实现跑在独立进程里。
    pub builtin: bool,
    /// 运行形态（`in_process` / `subprocess`，见 [`ModuleRuntime`]）。
    ///
    /// [`ModuleRuntime`]: crate::manifest::ModuleRuntime
    pub runtime: String,
    /// 所属包 id（跨贡献类型的标签，见 [`ModuleManifest::package_id`]）；
    /// 未声明 = 模块 id 自身。
    #[serde(default)]
    pub package: String,
    /// Mount error description when `enabled == false && builtin`.
    #[serde(default)]
    pub error: Option<String>,
    /// 该模块在 `modules.toml` 里由**哪一层**配置（空 = 配置未提及）。
    ///
    /// 由组合根在装载配置时写入（[`ModuleRegistry::set_config_origin`]）：
    /// 分层合成（profile → bundles → patches → 行内）之后每个条目都带着
    /// 来源层，面板据此回答「这行为什么长这样」。
    #[serde(default)]
    pub config_origin: String,
    /// **运行态**（`ready` / `stopped` / `failed` / `pending` / `draining`）；
    /// 空 = 非子进程模块（进程内实现没有独立生命周期）。
    ///
    /// 与 `enabled`（挂载/启用开关）正交：`enabled = true` 但
    /// `state = failed` 是「配置开了、进程起不来」——面板要能区分。
    #[serde(default)]
    pub state: String,
    /// 崩溃后已自动重启的次数（仅子进程模块）。
    #[serde(default)]
    pub restarts: u32,
    /// 连接代际（每次就绪自增；仅子进程模块）。
    #[serde(default)]
    pub generation: u64,
    /// 模块**定义从哪读来**：`modules.toml` 的路径，或数据模块的
    /// `module.toml` 路径；空 = 编译内置（无外部定义文件）。
    ///
    /// 排查用：「我改的到底是不是这个文件」——比 `config_origin`（只说哪一层）
    /// 更具体，后者是线格式里的层名，这里是磁盘路径。
    #[serde(default)]
    pub source_path: String,
    /// 子进程模块的启动命令（空 = 进程内实现 / 未声明）。
    ///
    /// 与 `error` 配合回答「为什么起不来」：命令拼错、路径不存在、参数漏了
    /// ——面板不必让人翻 `modules.toml` 就能看出意图。
    #[serde(default)]
    pub command: String,
    /// 启动参数（`modules.toml` 的 `args`）。
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量的**键名**（值不下发）。
    ///
    /// 只给键：排查「我是不是漏传了某个变量」够用，而值里常有 token /
    /// 密钥，上协议即等于把它们送到前端内存与日志里。
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// 本模块**要求的服务键**（`Inject::required`；空 = 无要求）。
    ///
    /// 面板据此回答「这个模块为什么不挂载」：注册表在 mount 前逐个
    /// `Ctx::contains` 校验，缺失即 [`ModuleMountError::MissingService`]，
    /// 于是 `enabled` 仍为 true 而 `error` 有值。与 `args`/`env_keys`
    /// 同层——都是"排查挂载失败需要的信息"。
    #[serde(default)]
    pub required_services: Vec<String>,
    /// 本模块声明的**软依赖**（`Inject::optional`；缺失不阻止挂载）。
    #[serde(default)]
    pub optional_services: Vec<String>,
    /// 眼下**还缺**的硬依赖（空 = 不缺）。
    ///
    /// 与 `error` 的区别：`error` 是"挂载尝试的结果"，这里是"此刻的事实"
    /// ——依赖恢复后 [`ModuleRegistry::reconcile`] 会把模块挂上并清空它。
    #[serde(default)]
    pub pending_services: Vec<String>,
    /// 本模块订阅的事件名（`Contribution::Events` 的进程内等价物）。
    ///
    /// 空 = 不订阅。面板据此回答「这个模块会对什么有反应」。
    #[serde(default)]
    pub subscribed_events: Vec<String>,
    /// 本模块**处理的命令名**（`Contribution::Command` 的进程内等价物）。
    ///
    /// 空 = 不处理任何命令。面板据此回答「这个命令是谁在处理」，也让"命令
    /// 无人处理"这类问题一眼可见（模块被禁用 → 这里空了）。
    #[serde(default)]
    pub handled_commands: Vec<String>,
    /// 本模块贡献的**提示词块键**（`Contribution::Text` 的进程内等价物）。
    ///
    /// 只是声明（`key`），文本在装配时按上下文产生。空 = 不贡献。
    #[serde(default)]
    pub text_blocks: Vec<String>,
}

/// 模块的**定义来源**（面板技术信息展示用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleDefinition {
    /// 定义文件路径（`modules.toml` / 数据模块的 `module.toml`）。
    pub source_path: String,
    /// 子进程启动命令（进程内实现为空）。
    pub command: String,
    /// 启动参数。
    pub args: Vec<String>,
    /// 环境变量**键名**（值不下发：常含 token / 密钥）。
    pub env_keys: Vec<String>,
}

impl From<&ModuleManifest> for ModuleDescriptor {
    fn from(m: &ModuleManifest) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.clone(),
            kind: m.kind.clone(),
            description: m.description.clone(),
            entry: m.entry.clone(),
            author: m.author.clone(),
            enabled: true,
            builtin: m.builtin,
            runtime: m.runtime.as_str().to_string(),
            package: m.package_id().to_string(),
            error: None,
            config_origin: String::new(),
            state: String::new(),
            restarts: 0,
            generation: 0,
            source_path: String::new(),
            command: String::new(),
            args: Vec::new(),
            env_keys: Vec::new(),
            required_services: Vec::new(),
            optional_services: Vec::new(),
            pending_services: Vec::new(),
            subscribed_events: Vec::new(),
            handled_commands: Vec::new(),
            text_blocks: Vec::new(),
        }
    }
}

/// [`ModuleRegistry::reconcile`] 的结果：这一轮动了什么。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 依赖到位后被挂上的模块。
    pub mounted: Vec<String>,
    /// 依赖消失后被卸下的模块。
    pub unmounted: Vec<String>,
    /// 依赖已齐但挂载失败（闭包报错）的模块 `(id, 错误)`。
    ///
    /// 与"缺依赖"分开：这是模块自己的 bug，不会被下一次 reconcile 自动重试
    /// （它已经不在 pending 里了）——否则会变成静默的失败循环。
    pub failed: Vec<(String, String)>,
}

impl ReconcileReport {
    /// 这一轮是否什么都没变（调用方据此决定要不要打日志/推事件）。
    pub fn is_noop(&self) -> bool {
        self.mounted.is_empty() && self.unmounted.is_empty() && self.failed.is_empty()
    }
}

/// 「当前正在挂载的模块」栈的 guard：drop 时弹栈（含 panic 展开路径）。
struct OwnerGuard {
    tree: Option<Arc<crate::ModuleScope>>,
    id: String,
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        if let Some(tree) = &self.tree {
            tree.links_leave(&self.id);
        }
    }
}

/// Registry of modules with mount/unmount lifecycle.
///
/// Thread-safe: `register` is for composition-root-time registration of
/// builtins; `mount_all`/`unmount_all` manage the active set. `set_enabled`
/// toggles a module at runtime (used by the Panel ToggleModule command).
#[derive(Default)]
pub struct ModuleRegistry {
    modules: std::sync::Mutex<BTreeMap<String, Arc<dyn Module>>>,
    active: std::sync::Mutex<BTreeMap<String, Vec<Disposer>>>,
    enabled_state: std::sync::Mutex<BTreeMap<String, bool>>,
    /// 模块 id → 声明它的配置层（展示用；见 [`ModuleRegistry::set_config_origin`]）。
    config_origins: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// 模块 id → 该模块的配置（`modules.toml` 的 `[module.config]` 投影）。
    ///
    /// mount 时按 id 注入 [`MountContext::config`]（见 [`Self::mount`]）：
    /// 这是"配置下发"的唯一通路——模块不必自己找到并解析 `modules.toml`，
    /// 也就不会读到别的模块那一份。未登记的 id 得到 `Value::Null`。
    configs: std::sync::Mutex<BTreeMap<String, serde_json::Value>>,
    /// 模块 id → 装配/启动失败原因（展示用；`None` 值表示"曾失败后已恢复"）。
    errors: std::sync::Mutex<BTreeMap<String, String>>,
    /// 模块 id → 「定义从哪来 / 怎么启动」（展示用；见
    /// [`ModuleRegistry::set_definition`]）。
    definitions: std::sync::Mutex<BTreeMap<String, ModuleDefinition>>,
    /// 本注册表所属的**树作用域**（弱引用；`ModuleScope` 建时注入）。
    ///
    /// 用弱引用是为了不制造 `scope → registry → scope` 的 Arc 环：作用域
    /// 由调用方持有，注册表只借它做「归属记录 + 级联卸载」。
    tree: std::sync::Mutex<Option<std::sync::Weak<crate::ModuleScope>>>,
    /// 事件出口（宿主注入；`None` = 没有接收方）。
    ///
    /// 两处用它：① 挂载时包一层带本模块 id 的 sink 放进 `MountContext`，
    /// 模块的 `emit` 因此不必知道自己的 id；② 挂/卸时由注册表自己发
    /// `module.mounted` / `module.unmounted`（模块树的观测面）。
    event_sink: std::sync::Mutex<Option<crate::EventSink>>,
    /// 运行态：缓存的挂载上下文 / 事件订阅兴趣集 / 投递重入与计数。
    ///
    /// 这四样都与领域无关（任何宿主都要），此前由宿主包一层自己维护
    /// （下游宿主的适配层曾为此多出约 150 行状态管理）。上收到注册表之后，
    /// 宿主只剩领域翻译与配置格式。
    runtime: std::sync::Mutex<crate::runtime_state::RuntimeState>,
    /// 因**依赖未就绪**而没挂上的模块 → 当时缺的键。
    ///
    /// 与 `enabled_state = false`（人主动禁用）区分开：这些是"该挂、还没
    /// 条件挂"，[`Self::reconcile`] 在依赖到位后会把它们挂上。
    pending: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
    /// 注册顺序（`descriptors()` 按它输出）。
    ///
    /// 为什么单独存：底层 `modules` 是 `BTreeMap`（按 id 排序），而
    /// `modules.toml` 的**行序 = 挂载顺序**是有意义的配置表达（bundle 里
    /// 靠前的先挂）。按 id 排序会把这条信息丢掉。
    order: std::sync::Mutex<Vec<String>>,
}

impl std::fmt::Debug for ModuleRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // ── 运行态（2026-11：从宿主上收）────────────────────────────────────
    //
    // 缓存的挂载上下文 / 事件兴趣集 / 投递重入与计数。四者都与领域无关，
    // 此前每个宿主都得自己包一层维护；现在注册表是唯一持有者。

    /// 记住挂载上下文。之后 [`Self::enable`] / [`Self::restart_cached`] /
    /// [`Self::reconcile_cached`] 不必再让调用方传它。
    ///
    /// 为什么需要缓存：`MountContext` 里带服务表（`Ctx`）与事件出口，它们在
    /// 进程寿命内是同一份；每次生命周期动作让调用方重复传，只会让宿主的
    /// 适配层被迫自己存一份（那是本方法存在之前的实际形状）。
    pub fn set_mount_ctx(&self, ctx: crate::MountContext) {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_mount_ctx(ctx);
    }

    /// 取回缓存的挂载上下文（未设置时 `None`）。
    pub fn mount_ctx(&self) -> Option<crate::MountContext> {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mount_ctx()
    }

    /// 取缓存上下文或默认值（生命周期方法用；未设置 = 裸注册表语义）。
    fn cached_ctx(&self) -> crate::MountContext {
        self.mount_ctx().unwrap_or_default()
    }

    /// 启用 / 停用一个模块（用**缓存的**上下文，见 [`Self::set_mount_ctx`]）。
    ///
    /// **语义 = 状态切换**，不是"确保挂载"：模块的默认状态是启用，所以对
    /// 一个从未挂载过的模块调 `enable(id, true)` 是 no-op（它已是"启用"
    /// 状态）。初始挂载走 [`Self::mount`] / [`Self::mount_all`] /
    /// [`Self::mount_plan`](crate::ModuleRegistry::mount_all)，停用后再启用
    /// 才由本方法真正重挂。
    pub fn enable(&self, id: &str, enabled: bool) -> Result<(), String> {
        let ctx = self.cached_ctx();
        self.set_enabled(id, enabled, &ctx)
    }

    /// 重启一个模块（卸载 + 重挂载，用缓存的上下文）。
    pub fn restart_cached(&self, id: &str) -> Result<(), String> {
        let ctx = self.cached_ctx();
        self.restart(id, &ctx)
    }

    /// 依赖变动后重挂 / 卸下 pending 模块（用缓存的上下文）。
    pub fn reconcile_cached(&self) -> ReconcileReport {
        let ctx = self.cached_ctx();
        self.reconcile(&ctx)
    }

    /// 是否有任何已挂载模块订阅了事件（**一次原子读**）。
    ///
    /// 热路径第一道闸门：`emit` 在流式正文里每 token 一条，无人订阅时必须
    /// 立刻返回、不序列化。
    pub fn has_event_subscribers(&self) -> bool {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_subscribers()
    }

    /// 该事件名当前是否有人订阅（不序列化；热路径第二道闸门）。
    pub fn wants_event(&self, name: &str) -> bool {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wants(name)
    }

    /// 重算事件订阅兴趣集。
    ///
    /// 由注册表在**挂载 / 卸载 / 启停 / 重启 / reconcile 之后自己调**
    /// （见各方法末尾），宿主不必记得——忘了只会让热路径多付序列化代价，
    /// 不会丢事件。
    ///
    /// 只统计**已挂载**且**已启用**的模块：没挂载的模块不该收到事件，替它
    /// 占位订阅会让热路径付无意义的代价。
    pub fn refresh_event_interest(&self) {
        let subscriptions: Vec<Vec<String>> = {
            let modules = self
                .modules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let states = self
                .enabled_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            modules
                .iter()
                .filter(|(id, _)| {
                    active.contains_key(*id) && states.get(*id).copied().unwrap_or(true)
                })
                .map(|(_, module)| module.subscribed_events())
                .collect()
        };
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refresh_interest(subscriptions.into_iter());
    }

    /// 投递一条模块事件，带**重入保护**与计数。
    ///
    /// 模块的 `on_event` 里若再触发一次 `emit`（比如它调了个会发事件的宿主
    /// API），内层直接丢并记一次失败——没有这道闸就是自递归。
    pub fn deliver_event_guarded(&self, event: &crate::ModuleEvent) -> usize {
        // 先判重入，再进投递段（`enter_delivery` 是原子的比较并交换）。
        let entered = {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !runtime.enter_delivery() {
                runtime.record_failed();
                false
            } else {
                true
            }
        };
        if !entered {
            tracing::debug!(
                name = %event.name,
                "module event dropped: delivery is already in progress (re-entrant emit)"
            );
            return 0;
        }
        /// 保证离开投递段（即使投递过程中 panic）。
        struct Leave<'a>(&'a RuntimeState);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.leave_delivery();
            }
        }
        let delivered = self.deliver_event(event);
        {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _leave = Leave(&runtime);
            runtime.record_delivered(delivered);
        }
        delivered
    }

    /// `(投递成功条数, 丢弃/失败条数)`——排查「事件为什么没到」的第一手证据。
    pub fn event_counters(&self) -> (u64, u64) {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counters()
    }

    /// Register a module definition (composition root / discovery). Does not
    /// mount it yet.
    pub fn register(&self, module: Arc<dyn Module>) -> Result<(), String> {
        let id = module.manifest().id.clone();
        let mut modules = self
            .modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if modules.contains_key(&id) {
            return Err(format!("module id already registered: {id}"));
        }
        modules.insert(id.clone(), Arc::clone(&module));
        drop(modules);
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
        Ok(())
    }

    /// Remove a registered (and unmounted) module by id.
    pub fn unregister(&self, id: &str) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.contains_key(id) {
            return false; // must be unmounted first
        }
        drop(active);
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|existing| existing != id);
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .is_some()
    }

    /// Activate a module: run its mount hooks and retain disposers.
    ///
    /// 1. **服务键就绪校验**：模块通过 [`Module::required_services`] 声明的
    ///    每个键都必须在 [`MountContext::ctx`] 里存在，否则
    ///    [`ModuleMountError::MissingService`]，**mount 闭包根本不会被调用**
    ///    （依赖没就绪就不该产生任何副作用）。
    /// 2. **配置注入**：把组合根通过 [`Self::set_config`] 登记的该模块配置
    ///    合并进传给 mount 的上下文；模块拿到的 `MountContext.config` 就是
    ///    它自己的那一份，不用自己去读 `modules.toml`（也就不会读到别人的）。
    ///
    /// [`Module::required_services`]: crate::Module::required_services
    pub fn mount(&self, id: &str, ctx: &MountContext) -> ModuleMountResult {
        let module = match self.module(id) {
            Some(module) => module,
            None => {
                return Err(ModuleMountError::Mount(format!(
                    "module not registered: {id}"
                )))
            }
        };
        // 1) 声明式注入（`inject.required`）：缺失即拒绝挂载，且错误里带
        //    "有哪些可用"。**闭包一次都不执行**——依赖没就绪就不该产生副作用。
        let inject = module.inject();
        let missing = inject.missing(ctx.ctx.as_ref());
        if let Some(key) = missing.first() {
            let available = ctx.ctx.as_ref().map(|c| c.keys()).unwrap_or_default();
            self.set_pending(id, missing.clone());
            return Err(ModuleMountError::MissingService {
                key: key.clone(),
                available,
            });
        }
        // 2) 本模块配置（未登记 = Null）+ 树把手（作用域归属）+ 事件出口。
        let module_ctx = MountContext {
            ctx: ctx.ctx.clone(),
            config: self.config_for(id),
            tree: ctx.tree.clone(),
            module_id: id.to_string(),
            events: self.sink_for_module(id),
        };
        // 3) 记录"当前正在挂载谁"：模块在 mount 里 `nest()` 出的子作用域
        //    据此记在它名下，卸载它时级联卸载（dsh 的 ctx.module 语义）。
        let owner = self.enter_owner(id);
        let result = module.mount(&module_ctx);
        drop(owner);
        let disposers = match result {
            Ok(disposers) => {
                self.set_module_error_cleared(id);
                disposers
            }
            Err(e) => {
                // 挂载失败必须**可观测**：只把它放进 `mount_all` 的返回值里，
                // 事后就没人看得见了——面板只能显示"没挂载"，操作者得翻日志
                // 才知道为什么。记到 descriptor 的 `error` 上，快照/面板就能
                // 直接回答"它为什么没起来"。
                //
                // 与 `pending`（缺依赖，可恢复）不同：这里是模块自己坏了，
                // 语义见 document/lifecycle.md。
                self.set_module_error(id, e.to_string());
                return Err(e);
            }
        };
        // 逆序撤销（后注册的先撤）——与嵌套注册的直觉一致，也让"外层资源
        // 在里层资源之后消失"这条约束自然成立。
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.insert(id.to_string(), disposers.into_iter().rev().collect());
        drop(active);
        // 挂载成功 → 观测事件（模块树的变化因此可见）。
        self.emit_host(
            "module.mounted",
            serde_json::json!({ "id": id, "depth": self.depth() }),
        );
        self.clear_pending(id);
        // 订阅兴趣集随之变化——热路径闸门由注册表自己维护，宿主不必记得
        // （忘了只会让热路径多付序列化代价，不会丢事件）。
        self.refresh_event_interest();
        Ok(Vec::new())
    }

    /// Deactivate a module: dispose all registrations (reversible effects).
    ///
    /// **级联**：该模块挂载期建出的子作用域（模块树）先被 `shutdown()`——
    /// 子先于父，且子作用域自己的 ctx 随之丢弃（父链引用断掉）。没有树把手
    /// 时（裸注册表挂载）就是单层卸载，行为与 v0.5.0 一致。
    pub fn unmount(&self, id: &str) -> Result<(), ModuleMountError> {
        // 子作用域先走：否则父的服务表先撤，子模块会看到"父服务突然消失"
        // 这种半死状态（它的 unmount 可能还要用父服务）。
        for child in self.take_child_scopes(id) {
            child.shutdown();
        }
        let removed = {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.remove(id)
        };
        if let Some(mut disposers) = removed {
            for d in disposers.drain(..) {
                d.dispose();
            }
            // `Module::unmount` = disposer 之外的清理（关文件、停线程…）。
            // **2026-11 修**：此前从未被调用——disposer 撤完就返回，模块
            // 覆写的 `unmount` 成了死代码。
            if let Some(module) = self.module(id) {
                module.unmount()?;
            }
            self.emit_host(
                "module.unmounted",
                serde_json::json!({ "id": id, "depth": self.depth() }),
            );
        }
        // 订阅兴趣集随之收缩（同 `mount` 末尾的理由）。
        self.refresh_event_interest();
        Ok(())
    }

    /// 挂载，但**先等注入就绪**（最多 `timeout`；dsh `inject` 的等待语义）。
    ///
    /// 与 [`Self::mount`] 的差别只有前置条件：`mount` 立即校验、缺失即失败
    /// （组合根"要么现在行、要么启动失败"）；本方法把「暂时没到」当成可等
    /// 待的事实，等到就挂、超时才失败。等待本身用
    /// [`Ctx::wait_for_keys`]——先就绪后挂载，中间不留空窗。
    ///
    /// `timeout = Duration::ZERO` 等价于 [`Self::mount`]（只查一次）。
    ///
    /// [`Ctx::wait_for_keys`]: echo_context::Ctx::wait_for_keys
    pub async fn mount_when_ready(
        &self,
        id: &str,
        ctx: &MountContext,
        timeout: std::time::Duration,
    ) -> ModuleMountResult {
        let module = match self.module(id) {
            Some(module) => module,
            None => {
                return Err(ModuleMountError::Mount(format!(
                    "module not registered: {id}"
                )))
            }
        };
        let required = module.inject().required;
        if !required.is_empty() {
            match ctx.ctx.as_ref() {
                Some(cx) => cx.wait_for_keys(&required, timeout).await.map_err(|err| {
                    let (key, available) = match err {
                        echo_context::ServiceError::Missing { key, available } => (key, available),
                        echo_context::ServiceError::Conflict { key } => (key, Vec::new()),
                    };
                    self.set_pending(id, vec![key.clone()]);
                    ModuleMountError::MissingService { key, available }
                })?,
                None => {
                    self.set_pending(id, required.clone());
                    return Err(ModuleMountError::MissingService {
                        key: required[0].clone(),
                        available: Vec::new(),
                    });
                }
            }
        }
        self.mount(id, ctx)
    }

    /// 按注册顺序批量挂载，每个都先等注入就绪（启动期用）。
    ///
    /// 返回 `(已挂载 id, 错误 (id, 描述))`。与 [`Self::mount_all`] 一样
    /// "尽力而为"：某个模块失败不阻断其余——但依赖它的模块会因缺服务而一并
    /// 记进 `pending`，等 [`Self::reconcile`] 收拾。
    pub async fn mount_all_when_ready(
        &self,
        ctx: &MountContext,
        timeout: std::time::Duration,
    ) -> (Vec<String>, Vec<(String, String)>) {
        let mut mounted = Vec::new();
        let mut errors = Vec::new();
        for id in self.ids() {
            if !self.is_enabled(&id) {
                continue;
            }
            match self.mount_when_ready(&id, ctx, timeout).await {
                Ok(_) => mounted.push(id),
                Err(e) => errors.push((id, e.to_string())),
            }
        }
        (mounted, errors)
    }

    /// 依赖变更后**重新核对**：该挂的挂上、该卸的卸下（dsh 的 inject 响应）。
    ///
    /// 三类动作，只看"事实"不看"意图"：
    ///
    /// 1. **挂**：注册过、启用着、没挂载、且**记在 pending 里**（上次因缺
    ///    依赖失败）的模块，其硬依赖现在齐了 → 挂上；
    /// 2. **卸**：已挂载、但硬依赖**没了** → 卸下并记进 pending（等它回来）；
    /// 3. 其余一律不动——尤其是"人主动禁用"的（`enabled_state = false`）与
    ///    "从没挂过也没 pending 记录"的，都不会被顺手拉起。
    ///
    /// 幂等：没变化时是两次表扫描，无副作用。宿主可以在
    /// [`Ctx::on_change`] 回调里直接调用（回调期间别重入即可，见
    /// `ModuleHost::reconcile` 的重入保护）。
    ///
    /// [`Ctx::on_change`]: echo_context::Ctx::on_change
    pub fn reconcile(&self, ctx: &MountContext) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        // 1) pending 里依赖已齐的 → 挂
        for id in pending {
            if !self.is_enabled(&id) || self.is_mounted(&id) {
                continue;
            }
            let Some(module) = self.module(&id) else {
                continue;
            };
            let missing = module.inject().missing(ctx.ctx.as_ref());
            if !missing.is_empty() {
                self.set_pending(&id, missing);
                continue;
            }
            match self.mount(&id, ctx) {
                Ok(_) => report.mounted.push(id),
                Err(e) => report.failed.push((id, e.to_string())),
            }
        }
        // 2) 已挂载但硬依赖消失的 → 卸
        for id in self.mounted_ids() {
            let Some(module) = self.module(&id) else {
                continue;
            };
            let missing = module.inject().missing(ctx.ctx.as_ref());
            if missing.is_empty() {
                continue;
            }
            if self.unmount(&id).is_ok() {
                self.set_pending(&id, missing);
                report.unmounted.push(id);
            }
        }
        report
    }

    /// 当前**还缺**硬依赖的模块（`id → 缺的键`）；面板"待挂载"视图用。
    pub fn pending_inject(&self) -> Vec<(String, Vec<String>)> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(id, keys)| (id.clone(), keys.clone()))
            .collect()
    }

    /// 注入**事件出口**（组合根装配期一次；子作用域建时自动继承）。
    ///
    /// 没有出口时一切 `emit` 都是静默丢弃——这是**降级而不是错误**：单测、
    /// 无 Panel 的部署都不需要它。
    pub fn set_event_sink(&self, sink: crate::EventSink) {
        *self
            .event_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    /// 当前的出口（子作用域建时用来继承）。
    pub fn event_sink(&self) -> Option<crate::EventSink> {
        self.event_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// **宿主**发一条事件（`module` 留空，表示不是某个模块发的）。
    pub fn emit_host(&self, name: &str, payload: serde_json::Value) {
        if let Some(sink) = self.event_sink() {
            sink(crate::ModuleEvent::host(name, payload));
        }
    }

    /// 把事件**投递给订阅了它的已挂载模块**，返回成功投递数。
    ///
    /// 订阅匹配：`subscribed_events()` 里含事件名，或含 [`crate::ALL_EVENTS`]。
    /// 投递失败（模块 `on_event` 返回 `Err`）只计数不卸载——事件是观察通道。
    ///
    /// 只投给**已挂载**的模块：没挂载就没有副作用，也就不该收到事件
    /// （与"禁用的模块不获得任何副作用"同一原则）。
    pub fn deliver_event(&self, event: &crate::ModuleEvent) -> usize {
        let mut delivered = 0;
        for id in self.mounted_ids() {
            let Some(module) = self.module(&id) else {
                continue;
            };
            let subscriptions = module.subscribed_events();
            if subscriptions.is_empty() {
                continue;
            }
            let wanted = subscriptions
                .iter()
                .any(|name| name == &event.name || name == crate::ALL_EVENTS);
            if !wanted {
                continue;
            }
            if module.on_event(event).is_ok() {
                delivered += 1;
            }
        }
        delivered
    }

    /// **分发一条命令**给声明了它的模块（`Contribution::Command`）。
    ///
    /// 返回：
    /// - `Some(Ok(v))` —— 某个模块处理了；
    /// - `Some(Err(e))` —— 某个模块认识但处理失败（`e` 原样给调用方）；
    /// - `None` —— **没有**任何已挂载模块声明这个命令（调用方走默认分支，
    ///   或在"本该有人处理"时回明确错误）。
    ///
    /// 只问**已挂载**的模块：模块被禁用/卸载后命令自动回到无人处理状态——
    /// 这正是"命令域归模块"该有的语义，不需要在 dispatch 处特判。
    ///
    /// 多个模块声明同一命令时按**注册序**取第一个真的处理了它的（模块可以用
    /// `handle_command` 返回 `None` 让位）。生产者侧应该避免这种重叠，这里
    /// 的行为只是"不至于崩"。
    pub fn dispatch_command(
        &self,
        name: &str,
        payload: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        for id in self.mounted_ids() {
            let Some(module) = self.module(&id) else {
                continue;
            };
            if !module.handled_commands().iter().any(|cmd| cmd == name) {
                continue;
            }
            if let Some(outcome) = module.handle_command(name, payload) {
                return Some(outcome);
            }
        }
        None
    }

    /// 收集所有已挂载模块贡献的文本块（按 `order` 升序，等值按注册序）。
    ///
    /// `context` 原样透传给模块；返回值里的文本已是**内容**（不是声明）。
    /// **内容为空的块会被丢掉**——"这个上下文下没话说"与"贡献一个空块"不是
    /// 一回事，让调用方少一层判空。
    pub fn collect_text_blocks(
        &self,
        context: &serde_json::Value,
    ) -> Vec<(crate::ModuleTextBlock, String)> {
        self.collect_text_blocks_filtered(context, |_| true)
    }

    /// 同 [`Self::collect_text_blocks`]，但按模块 id 过滤。
    ///
    /// 为什么需要过滤：宿主的"这个使用者能看什么"是**按使用者**决定的（宿主
    /// 里的白名单/授权），而模块挂载是**进程级**的。一次装配要能只取某些模块
    /// 贡献的块——框架只提供"按 id 过滤"这个钩子，判断逻辑完全归宿主（框架
    /// 不认识白名单是什么）。
    pub fn collect_text_blocks_filtered(
        &self,
        context: &serde_json::Value,
        keep: impl Fn(&str) -> bool,
    ) -> Vec<(crate::ModuleTextBlock, String)> {
        let mut out: Vec<(crate::ModuleTextBlock, String)> = Vec::new();
        for id in self.mounted_ids() {
            if !keep(&id) {
                continue;
            }
            let Some(module) = self.module(&id) else {
                continue;
            };
            for block in module.text_blocks() {
                if let Some(text) = module.text_block_content(&block.key, context) {
                    if !text.trim().is_empty() {
                        out.push((block, text));
                    }
                }
            }
        }
        // 稳定排序：`sort_by_key` 稳定，等值保持注册序。
        out.sort_by_key(|(block, _)| block.order);
        out
    }

    /// **异步**分发一条命令（宿主实际入口；处理里可能要 await IO）。
    ///
    /// 语义与 [`Self::dispatch_command`] 完全一致，只是把模块侧改成
    /// [`Module::handle_command_async`]——只实现了同步版本的模块照常工作
    /// （默认实现把同步结果包成 ready future）。
    ///
    /// [`Module::handle_command_async`]: crate::Module::handle_command_async
    pub async fn dispatch_command_async(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        for id in self.mounted_ids() {
            let Some(module) = self.module(&id) else {
                continue;
            };
            if !module.handled_commands().iter().any(|cmd| cmd == name) {
                continue;
            }
            // 先把 future 取出来（不等它），再 await——否则 `if let` 里持有
            // 的 `module` 借用会与 `&self` 的其他使用冲突。
            let fut = module.handle_command_async(name, payload.clone());
            if let Some(fut) = fut {
                return Some(fut.await);
            }
        }
        None
    }

    /// 某条命令当前由谁处理（诊断/测试用；`None` = 无人处理）。
    pub fn command_owner(&self, name: &str) -> Option<String> {
        self.mounted_ids().into_iter().find(|id| {
            self.module(id)
                .is_some_and(|p| p.handled_commands().iter().any(|cmd| cmd == name))
        })
    }

    /// 本注册表距树根的深度（裸注册表 = 0）。
    fn depth(&self) -> usize {
        self.tree_handle().map(|scope| scope.depth()).unwrap_or(0)
    }

    /// 给某个模块包装一个"自动填 id"的出口。
    fn sink_for_module(&self, id: &str) -> Option<crate::EventSink> {
        let sink = self.event_sink()?;
        let id = id.to_string();
        Some(Arc::new(move |mut event: crate::ModuleEvent| {
            if event.module.is_empty() {
                event.module = id.clone();
            }
            sink(event);
        }))
    }

    /// 本注册表所属的树作用域（`ModuleScope` 建时注入；裸注册表为 `None`）。
    pub fn tree_handle(&self) -> Option<Arc<crate::ModuleScope>> {
        self.tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }

    /// 注入树作用域（`ModuleScope::build` 调用一次）。
    pub(crate) fn attach_tree(&self, tree: std::sync::Weak<crate::ModuleScope>) {
        *self
            .tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tree);
    }

    /// 是否已挂载（`enabled` 是"用户想开"，这里问"实际挂着吗"）。
    pub fn is_mounted(&self, id: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(id)
    }

    fn mounted_ids(&self) -> Vec<String> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    fn set_pending(&self, id: &str, missing: Vec<String>) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), missing);
    }

    fn clear_pending(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    fn set_module_error_cleared(&self, id: &str) {
        self.errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// 压栈"正在挂载的模块"，返回的 guard 在 drop 时弹栈（panic 安全）。
    fn enter_owner(&self, id: &str) -> OwnerGuard {
        if let Some(tree) = self.tree_handle() {
            tree.links_enter(id);
            OwnerGuard {
                tree: Some(tree),
                id: id.to_string(),
            }
        } else {
            OwnerGuard {
                tree: None,
                id: id.to_string(),
            }
        }
    }

    fn take_child_scopes(&self, id: &str) -> Vec<Arc<crate::ModuleScope>> {
        match self.tree_handle() {
            Some(tree) => tree.links_take_children(id),
            None => Vec::new(),
        }
    }

    /// Mount every registered module (starting-up). Modules whose mount fails
    /// are skipped and reported via `descriptors()`.
    pub fn mount_all(&self, ctx: &MountContext) -> Vec<(String, String)> {
        let mut errors = Vec::new();
        let ids: Vec<String> = {
            let modules = self
                .modules
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            modules.keys().cloned().collect()
        };
        for id in ids {
            if !self.is_enabled(&id) {
                continue;
            }
            if let Err(e) = self.mount(&id, ctx) {
                errors.push((id, e.to_string()));
            }
        }
        errors
    }

    /// Unmount everything (shutdown).
    pub fn unmount_all(&self) {
        let ids: Vec<String> = {
            let active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.unmount(&id);
        }
    }

    /// Runtime enable/disable. Disabling unmounts; enabling remounts.
    /// `persisted` is the caller's durable enable/disable state (host-defined
    /// storage; the framework never reads it back).
    pub fn set_enabled(&self, id: &str, enabled: bool, ctx: &MountContext) -> Result<(), String> {
        let exists = self
            .modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(id);
        if !exists {
            return Err(format!("module not found: {id}"));
        }
        // **锁范围刻意缩小**（2026-11）：`mount` / `unmount` 内部会刷新订阅
        // 兴趣集，而刷新要读 `enabled_state`——持着它调用就是自锁。所以这里
        // 只做「读判断 → 释放 → 动作 → 写状态 → 刷新」。
        {
            let states = self
                .enabled_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if states.get(id).copied().unwrap_or(true) == enabled {
                return Ok(()); // 已经是目标状态
            }
        }
        if enabled {
            self.mount(id, ctx).map_err(|e| e.to_string())?;
        } else {
            self.unmount(id).map_err(|e| e.to_string())?;
        }
        self.enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), enabled);
        // 状态变了 → 兴趣集重算（`mount` 里那次算的是旧 enabled 值）。
        self.refresh_event_interest();
        Ok(())
    }

    /// Whether a module is currently enabled (default true before any
    /// `apply_disabled` / `set_enabled`).
    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .copied()
            .unwrap_or(true)
    }

    /// Apply persisted disable state at boot (before mount_all).
    pub fn apply_disabled(&self, disabled: &[String]) {
        let mut states = self
            .enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in disabled {
            states.insert(id.clone(), false);
        }
    }

    /// 全部已注册模块 id（**注册序**，与 [`ModuleRegistry::descriptors`] 一致）。
    pub fn ids(&self) -> Vec<String> {
        self.descriptors().into_iter().map(|d| d.id).collect()
    }

    pub fn module(&self, id: &str) -> Option<Arc<dyn Module>> {
        self.modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Full descriptor list for the Panel (modules + active state).
    pub fn descriptors(&self) -> Vec<ModuleDescriptor> {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let states = self
            .enabled_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 注册序（= 组合顺序 / config 行序），不是 id 字典序。
        let ids: Vec<String> = {
            let order = self
                .order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut ids: Vec<String> = order
                .iter()
                .filter(|id| modules.contains_key(*id))
                .cloned()
                .collect();
            // 防御：绕过 register 塞进 map 的（不该存在）补在末尾，不丢条目。
            let mut seen: std::collections::HashSet<String> = ids.iter().cloned().collect();
            for id in modules.keys() {
                if seen.insert(id.clone()) {
                    ids.push(id.clone());
                }
            }
            ids
        };
        let mut out = Vec::with_capacity(ids.len());
        let origins = self.config_origins();
        let definitions = self
            .definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let errors = self
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for id in ids {
            if let Some(p) = modules.get(&id) {
                let mut d = ModuleDescriptor::from(p.manifest());
                d.enabled = states.get(&id).copied().unwrap_or(true);
                let inject = p.inject();
                d.required_services = inject.required.clone();
                d.optional_services = inject.optional.clone();
                d.pending_services = pending.get(&id).cloned().unwrap_or_default();
                d.subscribed_events = p.subscribed_events();
                d.handled_commands = p.handled_commands();
                // 只问**声明**（键），不碰内容——所以描述符生成是纯读操作。
                d.text_blocks = p.text_blocks().into_iter().map(|b| b.key).collect();
                if let Some(origin) = origins.get(&id) {
                    d.config_origin = origin.clone();
                }
                if let Some(error) = errors.get(&id) {
                    d.error = Some(error.clone());
                }
                if let Some(definition) = definitions.get(&id) {
                    d.source_path = definition.source_path.clone();
                    d.command = definition.command.clone();
                    d.args = definition.args.clone();
                    d.env_keys = definition.env_keys.clone();
                }
                out.push(d);
            }
        }
        out
    }

    /// 批量写入来源层（组合根一次装载多处声明时用）。
    pub fn set_config_origins<I, K, V>(&self, origins: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map = self
            .config_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, origin) in origins {
            map.insert(id.into(), origin.into());
        }
    }

    /// 登记**某个模块的配置**（组合根装载 `modules.toml` 后写入）。
    ///
    /// 只影响 `mount` 时注入给该模块的 [`MountContext::config`]：框架不解析
    /// 内容、不做 schema 校验（那是模块与宿主之间的契约），也不回写任何文件。
    /// 运行期重新挂载（`set_enabled(true)` / `restart`）会取**当时**登记的值
    /// ——先 `set_config` 再启用，模块看到的就是新配置。
    pub fn set_config(&self, id: impl Into<String>, config: serde_json::Value) {
        self.configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), config);
    }

    /// 批量登记模块配置（组合根一次装载多处声明时用）。
    pub fn set_configs<I, K>(&self, configs: I)
    where
        I: IntoIterator<Item = (K, serde_json::Value)>,
        K: Into<String>,
    {
        let mut map = self
            .configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, config) in configs {
            map.insert(id.into(), config);
        }
    }

    /// 某模块登记的配置（未登记 = `Null`）。
    pub fn config_for(&self, id: &str) -> serde_json::Value {
        self.configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    /// 记录「定义从哪来 / 怎么启动」（面板技术信息用）。
    ///
    /// 只影响展示：运行期行为仍由 manifest 与配置决定。
    pub fn set_definition(&self, id: impl Into<String>, definition: ModuleDefinition) {
        self.definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), definition);
    }

    /// 记录模块装配/启动失败原因（组合根在启动子进程 / 装载数据模块后写入）。
    ///
    /// 面板据此区分「用户主动禁用」与「装配失败被跳过」——两者
    /// `enabled` 都是 false，但后者要人去修配置。
    pub fn set_module_error(&self, id: impl Into<String>, error: impl Into<String>) {
        self.errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.into(), error.into());
    }

    fn config_origins(&self) -> std::collections::HashMap<String, String> {
        self.config_origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 重载一个已启用的模块：卸下（执行全部 disposer）再重新挂载。
    ///
    /// 与 `set_enabled(false) → set_enabled(true)` 的差别：**不动启用状态**
    /// （不写 `disabled_modules`、不触发持久化），因此可以安全地用于
    /// "让某个模块按新配置/新外部状态重新初始化"。已禁用的模块直接报错
    /// ——重启的前提是先启用。
    pub fn restart(&self, id: &str, ctx: &MountContext) -> Result<(), String> {
        if !self.is_enabled(id) {
            return Err(format!(
                "module {id} is disabled; enable it before restarting"
            ));
        }
        self.unmount(id).map_err(|e| e.to_string())?;
        self.mount(id, ctx).map(|_| ()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ModuleManifest;
    use crate::module::{BuiltinModule, ModuleMountError};

    #[test]
    fn register_and_descriptors() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("t.t.t", "test", "1.0", "tool", "entry", "d"),
            |_| Ok(vec![]),
        )))
        .unwrap();
        let ds = reg.descriptors();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].id, "t.t.t");
        assert!(ds[0].enabled);
        assert!(ds[0].builtin);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let reg = ModuleRegistry::new();
        let p = || {
            Arc::new(BuiltinModule::new(
                ModuleManifest::builtin("a.a.a", "x", "1", "skill", "", ""),
                |_| Ok(vec![]),
            ))
        };
        reg.register(p()).unwrap();
        assert!(reg.register(p()).is_err());
    }

    #[test]
    fn mount_unmount_roundtrip() {
        let reg = ModuleRegistry::new();
        let ctx = MountContext::new();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("m.m.m", "m", "1", "tool", "", ""),
            move |_| {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.mount_all(&ctx);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        reg.unmount_all();
        reg.mount_all(&ctx);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// `restart` = 卸下 + 重新挂载，**不动**启用状态；
    /// 禁用的模块不允许重启（前提是先启用）。
    #[test]
    fn restart_remounts_without_touching_enabled_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = ModuleRegistry::new();
        let ctx = MountContext::new();
        let mounts = Arc::new(AtomicUsize::new(0));
        let disposes = Arc::new(AtomicUsize::new(0));
        let (m, d) = (mounts.clone(), disposes.clone());
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("r.r.r", "r", "1", "tool", "", ""),
            move |_| {
                m.fetch_add(1, Ordering::SeqCst);
                let d = d.clone();
                Ok(vec![Disposer::from_fn(move || {
                    d.fetch_add(1, Ordering::SeqCst);
                })])
            },
        )))
        .unwrap();
        reg.mount_all(&ctx);
        assert_eq!(mounts.load(Ordering::SeqCst), 1);

        reg.restart("r.r.r", &ctx).expect("restart enabled module");
        assert_eq!(mounts.load(Ordering::SeqCst), 2, "应重新挂载一次");
        assert_eq!(
            disposes.load(Ordering::SeqCst),
            1,
            "旧挂载的 disposer 应执行"
        );
        // 启用状态未变（restart 不是 set_enabled 的别名）。
        assert!(reg.is_enabled("r.r.r"));
        assert!(reg.descriptors()[0].enabled);

        // 禁用后拒绝重启。
        reg.set_enabled("r.r.r", false, &ctx).unwrap();
        let err = reg.restart("r.r.r", &ctx).expect_err("disabled module");
        assert!(err.contains("disabled"), "{err}");
    }

    /// 注册序 = 输出序：`modules.toml` 的行序（= 挂载顺序）是有意义的配置
    /// 表达，`descriptors()` 不能按 id 字典序重排它。
    #[test]
    fn descriptors_follow_registration_order() {
        let reg = ModuleRegistry::new();
        for id in ["zzz.last", "aaa.first", "mmm.middle"] {
            reg.register(Arc::new(BuiltinModule::new(
                ModuleManifest::builtin(id, id, "1", "tool", "", ""),
                |_| Ok(vec![]),
            )))
            .unwrap();
        }
        let ids: Vec<String> = reg.descriptors().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["zzz.last", "aaa.first", "mmm.middle"]);
        // ids() 与 descriptors() 同序（同一份真值）。
        assert_eq!(reg.ids(), ids);
        // 注销后顺序里也不留空洞。
        reg.unregister("aaa.first");
        assert_eq!(reg.ids(), vec!["zzz.last", "mmm.middle"]);
    }

    /// 装配失败原因进 descriptor：面板据此区分「用户主动禁用」（error 空）
    /// 与「启动失败被跳过」（error 有值）。
    #[test]
    fn module_errors_reach_descriptors() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("e.e.e", "e", "1", "tool", "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        assert_eq!(reg.descriptors()[0].error, None);
        reg.set_module_error("e.e.e", "spawn failed: No such file or directory");
        assert_eq!(
            reg.descriptors()[0].error.as_deref(),
            Some("spawn failed: No such file or directory")
        );
    }

    /// 定义来源进 descriptor：来源文件 / 启动命令 / 参数 / 环境变量键名。
    ///
    /// 只下发 env 的**键**：值里常带 token，上协议等于送进前端内存与日志。
    #[test]
    fn definitions_reach_descriptors_without_env_values() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("x.ext", "x", "1", "tool", "", "")
                .with_runtime(crate::ModuleRuntime::Subprocess)
                .with_builtin_flag(false),
            |_| Ok(vec![]),
        )))
        .unwrap();
        // 缺省：无定义来源（编译内置的形态）。
        assert_eq!(reg.descriptors()[0].source_path, "");
        assert_eq!(reg.descriptors()[0].command, "");

        reg.set_definition(
            "x.ext",
            ModuleDefinition {
                source_path: "/etc/echo/modules.toml".into(),
                command: "/opt/x/module".into(),
                args: vec!["--mode".into(), "fast".into()],
                env_keys: vec!["X_TOKEN".into(), "X_HOME".into()],
            },
        );
        let d = &reg.descriptors()[0];
        assert_eq!(d.source_path, "/etc/echo/modules.toml");
        assert_eq!(d.command, "/opt/x/module");
        assert_eq!(d.args, vec!["--mode", "fast"]);
        assert_eq!(d.env_keys, vec!["X_TOKEN", "X_HOME"]);
    }

    /// 配置来源层进 descriptor（面板展示「来自哪一层」）。
    #[test]
    fn config_origin_flags_reach_descriptors() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("o.o.o", "o", "1", "tool", "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        // 缺省：配置未提及。
        assert_eq!(reg.descriptors()[0].config_origin, "");
        reg.set_config_origins([("o.o.o", "base.toml · layer 0")]);
        assert_eq!(reg.descriptors()[0].config_origin, "base.toml · layer 0");
    }

    #[test]
    fn disabled_modules_skip_mount() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("d.d.d", "d", "1", "tool", "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        reg.apply_disabled(&["d.d.d".into()]);
        reg.mount_all(&MountContext::new());
        let ds = reg.descriptors();
        assert!(!ds[0].enabled);
    }

    #[test]
    fn mount_failure_is_reported() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("f.f.f", "f", "1", "tool", "", ""),
            |_| Err(ModuleMountError::Mount("boom".into())),
        )))
        .unwrap();
        let errors = reg.mount_all(&MountContext::new());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "f.f.f");
    }

    /// 配置下发：mount 时模块拿到**自己那一份** `MountContext.config`。
    ///
    /// 这是 `modules.toml` 的 `[module.config]` 进入 in-process 模块的唯一
    /// 通路：框架按 id 分发，模块不必（也不能）自己去读别人的配置。
    #[test]
    fn mount_injects_per_module_config() {
        use std::sync::Mutex;

        let reg = ModuleRegistry::new();
        // 两个模块各自记录 mount 时看到的 config。
        let seen_a: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::Value::Null));
        let seen_b = Arc::new(Mutex::new(serde_json::Value::Null));
        for (id, slot) in [("cfg.a", seen_a.clone()), ("cfg.b", seen_b.clone())] {
            let slot = slot.clone();
            reg.register(Arc::new(BuiltinModule::new(
                ModuleManifest::builtin(id, id, "1", "tool", "", ""),
                move |mctx: &MountContext| {
                    *slot.lock().unwrap() = mctx.config().clone();
                    Ok(vec![])
                },
            )))
            .unwrap();
        }
        reg.set_config("cfg.a", serde_json::json!({"port": 9001}));
        reg.set_config("cfg.b", serde_json::json!({"mode": "fast"}));

        reg.mount_all(&MountContext::new());

        assert_eq!(seen_a.lock().unwrap()["port"], 9001);
        assert_eq!(seen_b.lock().unwrap()["mode"], "fast");
        // 各拿各的：B 看不到 A 的键。
        assert!(seen_b.lock().unwrap().get("port").is_none());
    }

    /// 未登记配置 = `Null`（而不是 `{}`）：模块要能区分「没配」与「配了空表」。
    #[test]
    fn absent_config_is_null_not_empty_object() {
        use std::sync::Mutex;
        let reg = ModuleRegistry::new();
        let seen: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::json!("unset")));
        let slot = seen.clone();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("cfg.none", "n", "1", "tool", "", ""),
            move |mctx: &MountContext| {
                *slot.lock().unwrap() = mctx.config().clone();
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.mount_all(&MountContext::new());
        assert!(seen.lock().unwrap().is_null());
        assert_eq!(reg.descriptors()[0].required_services, Vec::<String>::new());
    }

    /// 运行时重新启用会取**当时**登记的配置（先 set_config 再启用即生效）。
    #[test]
    fn reenable_picks_up_new_config() {
        use std::sync::Mutex;
        let reg = ModuleRegistry::new();
        let ctx = MountContext::new();
        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let slot = seen.clone();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("cfg.re", "r", "1", "tool", "", ""),
            move |mctx: &MountContext| {
                slot.lock().unwrap().push(mctx.config().clone());
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.set_config("cfg.re", serde_json::json!({"v": 1}));
        reg.mount_all(&ctx);
        assert_eq!(reg.set_enabled("cfg.re", false, &ctx), Ok(()));
        reg.set_config("cfg.re", serde_json::json!({"v": 2}));
        assert_eq!(reg.set_enabled("cfg.re", true, &ctx), Ok(()));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["v"], 1);
        assert_eq!(seen[1]["v"], 2);
    }

    /// 服务键就绪校验：依赖缺失 → mount 失败，且 **mount 闭包不执行**
    /// （依赖没就绪就不该产生任何副作用）。
    #[test]
    fn missing_required_service_blocks_mount() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let reg = ModuleRegistry::new();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("svc.x", "x", "1", "loop", "", ""),
                move |_| {
                    flag.store(true, Ordering::SeqCst);
                    Ok(vec![])
                },
            )
            .with_required_services(["loop"]),
        ))
        .unwrap();

        // 1) 没有任何 Ctx：缺失。
        let err = match reg.mount("svc.x", &MountContext::new()) {
            Ok(_) => panic!("expected MissingService, got Ok"),
            Err(e) => e,
        };
        match err {
            ModuleMountError::MissingService { key, available } => {
                assert_eq!(key, "loop");
                assert!(available.is_empty());
            }
            other => panic!("expected MissingService, got {other:?}"),
        }
        assert!(!ran.load(Ordering::SeqCst), "mount 闭包不该执行");

        // 2) 有 Ctx 但没这个键：仍然缺失，错误里列出可用键。
        let ctx = echo_context::Ctx::default();
        let ctx = Arc::new(ctx);
        let _keep = ctx.register::<u8>("other", 1u8);
        let err = match reg.mount("svc.x", &MountContext::new().with_ctx(ctx.clone())) {
            Ok(_) => panic!("expected MissingService, got Ok"),
            Err(e) => e,
        };
        match err {
            ModuleMountError::MissingService { key, available } => {
                assert_eq!(key, "loop");
                assert_eq!(available, vec!["other".to_string()]);
            }
            other => panic!("expected MissingService, got {other:?}"),
        }
        assert!(!ran.load(Ordering::SeqCst));

        // 3) 服务就绪：挂载成功。
        let _keep2 = ctx.register::<u8>("loop", 2u8);
        reg.mount("svc.x", &MountContext::new().with_ctx(ctx))
            .expect("service present -> mount succeeds");
        assert!(ran.load(Ordering::SeqCst), "服务就绪后 mount 闭包应执行");
    }

    /// 声明的服务键出现在 descriptor 里（面板可见「它要什么」）。
    #[test]
    fn descriptors_expose_required_services() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("svc.d", "d", "1", "loop", "", ""),
                |_| Ok(vec![]),
            )
            .with_required_services(["loop", "sanitizer"]),
        ))
        .unwrap();
        assert_eq!(
            reg.descriptors()[0].required_services,
            vec!["loop".to_string(), "sanitizer".to_string()]
        );
    }

    // ── 声明式 inject：就绪等待 + 依赖变动重挂 ─────────────────────────

    /// 造一个"要 `loop` 服务"的模块，挂载时记账。
    fn needs_loop(
        id: &str,
        mounted: Arc<std::sync::atomic::AtomicUsize>,
        unmounted: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Arc<dyn Module> {
        use std::sync::atomic::Ordering;
        let manifest = ModuleManifest::builtin(id, id, "1", "loop", "", "");
        let module = BuiltinModule::new(manifest, move |_| {
            mounted.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        })
        .with_inject(crate::Inject::new(["loop"]));
        struct WithUnmount {
            inner: BuiltinModule,
            unmounted: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Module for WithUnmount {
            fn manifest(&self) -> &ModuleManifest {
                self.inner.manifest()
            }
            fn mount(&self, ctx: &MountContext) -> ModuleMountResult {
                self.inner.mount(ctx)
            }
            fn inject(&self) -> crate::Inject {
                self.inner.inject()
            }
            fn unmount(&self) -> Result<(), ModuleMountError> {
                self.unmounted
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }
        Arc::new(WithUnmount {
            inner: module,
            unmounted,
        })
    }

    /// `mount_when_ready`：服务晚到也能挂上（等到再挂，不留空窗）。
    #[tokio::test]
    async fn mount_when_ready_waits_for_the_service() {
        let reg = ModuleRegistry::new();
        let mounted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        reg.register(needs_loop(
            "w.w",
            mounted.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        ))
        .unwrap();
        let ctx = Arc::new(echo_context::Ctx::default());
        let mctx = MountContext::new().with_ctx(ctx.clone());

        // 100ms 后才提供 `loop`。
        let provider = ctx.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _keep = provider.register::<u8>("loop", 1u8);
            // 保持存活直到测试结束（把 disposer 泄在这儿不影响断言）
            std::mem::forget(_keep);
        });
        let started = std::time::Instant::now();
        reg.mount_when_ready("w.w", &mctx, std::time::Duration::from_secs(2))
            .await
            .expect("应该等到 loop 再挂上");
        handle.await.unwrap();
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(80),
            "必须真的等过（而不是立即失败）"
        );
        assert_eq!(mounted.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(reg.is_mounted("w.w"));
        assert!(reg.pending_inject().is_empty(), "挂上后不该再有 pending");
    }

    /// 超时：`mount_when_ready` 等不到就失败，并记进 pending（面板可见）。
    #[tokio::test]
    async fn mount_when_ready_times_out_and_records_pending() {
        let reg = ModuleRegistry::new();
        reg.register(needs_loop(
            "w.slow",
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        ))
        .unwrap();
        let mctx = MountContext::new().with_ctx(Arc::new(echo_context::Ctx::default()));
        let err = reg
            .mount_when_ready("w.slow", &mctx, std::time::Duration::from_millis(50))
            .await;
        match err {
            Err(ModuleMountError::MissingService { key, .. }) => assert_eq!(key, "loop"),
            Err(other) => panic!("expected MissingService, got {other}"),
            Ok(_) => panic!("expected MissingService, got Ok"),
        }
        assert_eq!(
            reg.pending_inject(),
            vec![("w.slow".to_string(), vec!["loop".to_string()])]
        );
        assert!(!reg.is_mounted("w.slow"));
        // descriptor 里能看到 pending（面板据此显示"等 loop"）
        let d = &reg.descriptors()[0];
        assert_eq!(d.required_services, vec!["loop".to_string()]);
        assert_eq!(d.pending_services, vec!["loop".to_string()]);
    }

    /// `timeout = ZERO` 等价于立即 mount（只查一次）。
    #[tokio::test]
    async fn zero_timeout_equals_immediate_check() {
        let reg = ModuleRegistry::new();
        reg.register(needs_loop(
            "w.zero",
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        ))
        .unwrap();
        let mctx = MountContext::new().with_ctx(Arc::new(echo_context::Ctx::default()));
        let err = reg
            .mount_when_ready("w.zero", &mctx, std::time::Duration::ZERO)
            .await;
        assert!(matches!(err, Err(ModuleMountError::MissingService { .. })));
    }

    /// `reconcile`：依赖到位 → 自动挂上；依赖消失 → 自动卸下。
    ///
    /// 这是 dsh `inject` 的完整语义（不只是启动期检查一次）。
    #[test]
    fn reconcile_mounts_when_deps_arrive_and_unmounts_when_they_leave() {
        use std::sync::atomic::Ordering;
        let reg = ModuleRegistry::new();
        let mounted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let unmounted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        reg.register(needs_loop("r.r", mounted.clone(), unmounted.clone()))
            .unwrap();
        let ctx = Arc::new(echo_context::Ctx::default());
        let mctx = MountContext::new().with_ctx(ctx.clone());

        // 1) 依赖缺失：首次挂载失败 → 进 pending；reconcile 无事可做。
        assert!(reg.mount("r.r", &mctx).is_err());
        assert!(reg.reconcile(&mctx).is_noop());
        assert_eq!(mounted.load(Ordering::SeqCst), 0);

        // 2) 服务到位 → reconcile 挂上。
        let keep = ctx.register::<u8>("loop", 1u8);
        let report = reg.reconcile(&mctx);
        assert_eq!(report.mounted, vec!["r.r".to_string()]);
        assert_eq!(mounted.load(Ordering::SeqCst), 1);
        assert!(reg.pending_inject().is_empty());
        // 幂等：再核一次什么都不动。
        assert!(reg.reconcile(&mctx).is_noop());

        // 3) 服务消失 → reconcile 卸下并重新进 pending。
        keep.dispose();
        let report = reg.reconcile(&mctx);
        assert_eq!(report.unmounted, vec!["r.r".to_string()]);
        assert_eq!(unmounted.load(Ordering::SeqCst), 1);
        assert!(!reg.is_mounted("r.r"));
        assert_eq!(
            reg.pending_inject(),
            vec![("r.r".to_string(), vec!["loop".to_string()])]
        );

        // 4) 服务回来 → 又挂上（这就是"依赖变更 → 模块重载"）。
        let _keep = ctx.register::<u8>("loop", 2u8);
        assert_eq!(reg.reconcile(&mctx).mounted, vec!["r.r".to_string()]);
        assert_eq!(mounted.load(Ordering::SeqCst), 2);
    }

    /// reconcile **不会**顺手拉起"从没挂过也没 pending 记录"的模块，
    /// 也不会动"人主动禁用"的模块。
    #[test]
    fn reconcile_does_not_touch_unrelated_or_disabled_modules() {
        use std::sync::atomic::Ordering;
        let reg = ModuleRegistry::new();
        let mounted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let unmounted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        reg.register(needs_loop("q.never", mounted.clone(), unmounted.clone()))
            .unwrap();
        let ctx = Arc::new(echo_context::Ctx::default());
        let _keep = ctx.register::<u8>("loop", 1u8);
        let mctx = MountContext::new().with_ctx(ctx);

        // 从没挂过、也没 pending 记录 → reconcile 不动它。
        assert!(reg.reconcile(&mctx).is_noop());
        assert_eq!(mounted.load(Ordering::SeqCst), 0);

        // 人主动禁用 → 即便有 pending 记录也不挂。
        assert!(reg.mount("q.never", &mctx).is_ok());
        reg.unmount("q.never").unwrap();
        reg.apply_disabled(&["q.never".to_string()]);
        // 伪造 pending：手动制造"缺依赖失败"的状态再启用检查。
        assert!(reg.reconcile(&mctx).is_noop());
        assert_eq!(mounted.load(Ordering::SeqCst), 1);
    }

    /// 可选依赖（`Inject::optional`）不影响挂载，但会出现在 descriptor 里。
    #[test]
    fn optional_services_are_declared_but_never_block_mount() {
        let reg = ModuleRegistry::new();
        let module = BuiltinModule::new(
            ModuleManifest::builtin("o.o", "o", "1", "tool", "", ""),
            |_| Ok(vec![]),
        )
        .with_inject(crate::Inject::new(["loop"]).with_optional(["sanitizer", "extra"]));
        reg.register(Arc::new(module)).unwrap();
        let ctx = Arc::new(echo_context::Ctx::default());
        let _keep = ctx.register::<u8>("loop", 1u8);
        reg.mount("o.o", &MountContext::new().with_ctx(ctx))
            .expect("optional 缺失不该阻止挂载");
        let d = &reg.descriptors()[0];
        assert_eq!(d.required_services, vec!["loop".to_string()]);
        assert_eq!(
            d.optional_services,
            vec!["sanitizer".to_string(), "extra".to_string()]
        );
        assert!(d.pending_services.is_empty());
    }
}

#[cfg(test)]
mod command_text_block_tests {
    use super::*;
    use crate::manifest::ModuleManifest;
    use crate::module::{BuiltinModule, ModuleTextBlock};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 命令分发：只有声明过的名字被路由到，`None` = 无人处理。
    #[test]
    fn dispatch_routes_only_declared_commands() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("cmd.a", "a", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_commands(["DoThing", "Echo"])
            .with_command_handler(|name, payload| match name {
                "DoThing" => Some(Ok(serde_json::json!({"did": true}))),
                "Echo" => Some(Ok(payload.clone())),
                _ => None,
            }),
        ))
        .unwrap();
        reg.mount_all(&MountContext::new());

        assert_eq!(reg.command_owner("DoThing").as_deref(), Some("cmd.a"));
        let out = reg
            .dispatch_command("DoThing", &serde_json::Value::Null)
            .expect("已声明 → 有结果");
        assert_eq!(out.unwrap()["did"], true);
        // payload 原样透传（框架不解析内容）
        let echoed = reg
            .dispatch_command("Echo", &serde_json::json!({"n": 7}))
            .unwrap()
            .unwrap();
        assert_eq!(echoed["n"], 7);
        // 没声明 → None（调用方走默认分支）
        assert!(reg
            .dispatch_command("Unrelated", &serde_json::Value::Null)
            .is_none());
    }

    /// 模块被禁用后命令自动回到"无人处理"——不在 dispatch 处特判。
    #[test]
    fn disabled_module_stops_owning_its_commands() {
        let reg = ModuleRegistry::new();
        let ctx = MountContext::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("cmd.b", "b", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_commands(["DoThing"])
            .with_command_handler(|_, _| Some(Ok(serde_json::Value::Null))),
        ))
        .unwrap();
        reg.mount_all(&ctx);
        assert!(reg
            .dispatch_command("DoThing", &serde_json::Value::Null)
            .is_some());

        reg.set_enabled("cmd.b", false, &ctx).unwrap();
        assert!(reg.command_owner("DoThing").is_none());
        assert!(reg
            .dispatch_command("DoThing", &serde_json::Value::Null)
            .is_none());
    }

    /// 多个声明者：第一个**真的处理了**的胜出（返回 None 的让位）。
    #[test]
    fn later_declarer_takes_over_when_the_first_declines() {
        let reg = ModuleRegistry::new();
        for (id, answer) in [("cmd.first", None), ("cmd.second", Some("second"))] {
            reg.register(Arc::new(
                BuiltinModule::new(ModuleManifest::builtin(id, id, "1", "tool", "", ""), |_| {
                    Ok(vec![])
                })
                .with_commands(["Shared"])
                .with_command_handler(move |_, _| answer.map(|a| Ok(serde_json::json!(a)))),
            ))
            .unwrap();
        }
        reg.mount_all(&MountContext::new());
        let out = reg
            .dispatch_command("Shared", &serde_json::Value::Null)
            .expect("第二个应接住");
        assert_eq!(out.unwrap(), "second");
        // owner 仍报第一个（它声明了，只是让位）——诊断视图看的是声明关系
        assert_eq!(reg.command_owner("Shared").as_deref(), Some("cmd.first"));
    }

    /// 处理失败原样传出（不是 panic，也不是静默吞掉）。
    #[test]
    fn command_errors_are_passed_through_verbatim() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("cmd.err", "e", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_commands(["Boom"])
            .with_command_handler(|_, _| Some(Err("磁盘满了".into()))),
        ))
        .unwrap();
        reg.mount_all(&MountContext::new());
        let out = reg
            .dispatch_command("Boom", &serde_json::Value::Null)
            .expect("有人处理")
            .unwrap_err();
        assert_eq!(out, "磁盘满了");
    }

    /// 文本块：按 order 排序，等值按注册序；空内容被丢掉。
    #[test]
    fn text_blocks_are_collected_in_order_and_empties_dropped() {
        let reg = ModuleRegistry::new();
        let mounted = Arc::new(AtomicUsize::new(0));
        // 注册序：late(20) → early(10) → equal(10) → empty
        for (id, key, order, text) in [
            ("pm.late", "late", 20, Some("L")),
            ("pm.early", "early", 10, Some("E")),
            ("pm.equal", "equal", 10, Some("Q")),
            ("pm.empty", "empty", 0, None),
        ] {
            let m = mounted.clone();
            reg.register(Arc::new(
                BuiltinModule::new(
                    ModuleManifest::builtin(id, id, "1", "tool", "", ""),
                    move |_| {
                        m.fetch_add(1, Ordering::SeqCst);
                        Ok(vec![])
                    },
                )
                .with_text_blocks([ModuleTextBlock::new(key, key, order)])
                .with_text_block_content(move |requested, _ctx| {
                    (requested == key).then(|| text.unwrap_or_default().to_string())
                }),
            ))
            .unwrap();
        }
        reg.mount_all(&MountContext::new());

        let blocks = reg.collect_text_blocks(&serde_json::Value::Null);
        let keys: Vec<&str> = blocks.iter().map(|(b, _)| b.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["early", "equal", "late"],
            "order 升序 + 等值保持注册序"
        );
        assert!(
            blocks.iter().all(|(_, text)| !text.is_empty()),
            "空块应被丢掉"
        );
    }

    /// 上下文原样透传：同一个块按不同会话给出不同文本。
    #[test]
    fn text_block_context_reaches_the_module_verbatim() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("pm.ctx", "c", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_text_blocks([ModuleTextBlock::new("ctx", "上下文块", 0)])
            .with_text_block_content(|_, ctx| {
                // 模块自己解释上下文（框架不解释）
                ctx.get("session_id")
                    .and_then(|v| v.as_str())
                    .map(|s| format!("当前会话 {s}"))
            }),
        ))
        .unwrap();
        reg.mount_all(&MountContext::new());

        let a = reg.collect_text_blocks(&serde_json::json!({"session_id": "s1"}));
        assert_eq!(a[0].1, "当前会话 s1");
        let b = reg.collect_text_blocks(&serde_json::json!({"session_id": "s2"}));
        assert_eq!(b[0].1, "当前会话 s2");
        // 没有上下文 → 模块返回 None → 块被丢掉
        assert!(reg.collect_text_blocks(&serde_json::Value::Null).is_empty());
    }

    /// descriptor 暴露声明（面板可见「谁处理哪些命令 / 贡献哪些块」）。
    #[test]
    fn descriptors_expose_commands_and_text_block_keys() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("pm.desc", "d", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_commands(["A", "B"])
            .with_text_blocks([
                ModuleTextBlock::new("k1", "区块一", 0),
                ModuleTextBlock::new("k2", "区块二", 5),
            ])
            .with_text_block_content(|_, _| Some("x".into())),
        ))
        .unwrap();
        let d = &reg.descriptors()[0];
        assert_eq!(d.handled_commands, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(d.text_blocks, vec!["k1".to_string(), "k2".to_string()]);
    }
}

#[cfg(test)]
mod text_block_filter_tests {
    use super::*;
    use crate::manifest::ModuleManifest;
    use std::sync::Arc;

    fn module(id: &'static str, key: &'static str) -> Arc<dyn Module> {
        Arc::new(
            crate::module::BuiltinModule::new(
                ModuleManifest::builtin(id, id, "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_text_blocks([crate::ModuleTextBlock::new(key, key, 0)])
            .with_text_block_content(move |_, _| Some(format!("from {id}"))),
        )
    }

    /// 过滤钩子只取留下的那些模块贡献的块（宿主按使用者做判断）。
    #[test]
    fn filter_keeps_only_selected_modules() {
        let reg = ModuleRegistry::new();
        reg.register(module("tb.a", "a")).unwrap();
        reg.register(module("tb.b", "b")).unwrap();
        reg.mount_all(&MountContext::new());

        let all = reg.collect_text_blocks(&serde_json::Value::Null);
        assert_eq!(all.len(), 2, "不过滤 = 全要");

        let only_b = reg.collect_text_blocks_filtered(&serde_json::Value::Null, |id| id == "tb.b");
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].0.key, "b");
        assert_eq!(only_b[0].1, "from tb.b");

        let none = reg.collect_text_blocks_filtered(&serde_json::Value::Null, |_| false);
        assert!(none.is_empty(), "全否 = 空");
    }
}

#[cfg(test)]
mod async_command_tests {
    use super::*;
    use crate::manifest::ModuleManifest;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 异步处理真能 await（用 yield 让出一次执行权）。
    #[tokio::test]
    async fn async_handler_is_awaited() {
        let reg = ModuleRegistry::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let p = polls.clone();
        reg.register(Arc::new(
            crate::module::BuiltinModule::new(
                ModuleManifest::builtin("ac.a", "a", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_commands(["Slow"])
            .with_command_handler_async(move |name, payload| {
                let p = p.clone();
                Box::pin(async move {
                    // 真的让出一次：证明宿主在 await，而不是拿到 ready future
                    tokio::task::yield_now().await;
                    p.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({ "name": name, "echo": payload }))
                })
            }),
        ))
        .unwrap();
        reg.mount_all(&MountContext::new());

        let out = reg
            .dispatch_command_async("Slow", serde_json::json!({"n": 1}))
            .await
            .expect("有人处理")
            .unwrap();
        assert_eq!(out["name"], "Slow");
        assert_eq!(out["echo"]["n"], 1);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    /// 只实现同步版本的模块，走异步分发也能工作（默认实现委托）。
    #[tokio::test]
    async fn sync_only_module_works_through_async_dispatch() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            crate::module::BuiltinModule::new(
                ModuleManifest::builtin("ac.b", "b", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_commands(["Fast"])
            .with_command_handler(|_, _| Some(Ok(serde_json::json!({"fast": true})))),
        ))
        .unwrap();
        reg.mount_all(&MountContext::new());
        let out = reg
            .dispatch_command_async("Fast", serde_json::Value::Null)
            .await
            .expect("有人处理")
            .unwrap();
        assert_eq!(out["fast"], true);
        // 未声明的名字仍然回到 None
        assert!(reg
            .dispatch_command_async("Nope", serde_json::Value::Null)
            .await
            .is_none());
    }

    // ── 运行态（2026-11 从宿主上收）────────────────────────────────────

    use crate::module::BuiltinModule;

    /// 缓存上下文后，`enable` / `restart_cached` / `reconcile_cached` 不必
    /// 再让调用方传 `MountContext`——宿主因此不需要自己存一份。
    #[test]
    fn cached_mount_ctx_lets_lifecycle_calls_skip_the_argument() {
        let reg = ModuleRegistry::new();
        let mounted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = mounted.clone();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("rt.cached", "cached", "1.0", "tool", "e", "d"),
            move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.set_mount_ctx(MountContext::new());
        assert!(reg.mount_ctx().is_some(), "上下文必须被缓存");

        // 初始挂载走 mount_all（`enable(true)` 对"已启用"是 no-op，见其文档）。
        reg.mount_all(&MountContext::new());
        assert!(reg.is_mounted("rt.cached"));
        reg.enable("rt.cached", false)
            .expect("停用（用缓存上下文）");
        assert!(!reg.is_mounted("rt.cached"));
        reg.enable("rt.cached", true)
            .expect("重新启用（用缓存上下文）");
        assert!(reg.is_mounted("rt.cached"));
        reg.restart_cached("rt.cached")
            .expect("重启（用缓存上下文）");
        assert!(reg.is_mounted("rt.cached"));
    }

    /// 未设置缓存上下文也能用（裸注册表语义：默认 `MountContext`）。
    #[test]
    fn lifecycle_calls_work_without_a_cached_ctx() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("rt.naked", "naked", "1.0", "tool", "e", "d"),
            |_| Ok(vec![]),
        )))
        .unwrap();
        // 无缓存上下文 = 默认 `MountContext`（裸注册表语义）。
        reg.mount_all(&MountContext::new());
        assert!(reg.is_mounted("rt.naked"));
        reg.enable("rt.naked", false).expect("停用不需要上下文");
        reg.enable("rt.naked", true).expect("重新启用不需要上下文");
        assert!(reg.is_mounted("rt.naked"));
    }

    /// 兴趣集由**注册表自己**在挂/卸后刷新：宿主不必记得调。
    ///
    /// 忘了刷新的后果只是热路径多付序列化代价，不会丢事件——但"必须记得"
    /// 本身就是一层不该存在的负担。
    #[test]
    fn event_interest_refreshes_itself_on_mount_and_unmount() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("rt.sub", "sub", "1.0", "tool", "e", "d"),
                |_| Ok(vec![]),
            )
            .with_subscriptions(["tool.done"]),
        ))
        .unwrap();
        assert!(!reg.has_event_subscribers(), "未挂载时不该有人订阅");
        reg.mount_all(&MountContext::new());
        assert!(reg.has_event_subscribers(), "挂载后兴趣集必须刷新");
        assert!(reg.wants_event("tool.done"));
        assert!(!reg.wants_event("other"));
        reg.enable("rt.sub", false).expect("停用");
        assert!(!reg.has_event_subscribers(), "停用后兴趣集必须收缩");
    }

    /// 通配订阅：`*` 让所有名字都命中。
    #[test]
    fn wildcard_subscription_wants_everything() {
        let reg = ModuleRegistry::new();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("rt.all", "all", "1.0", "tool", "e", "d"),
                |_| Ok(vec![]),
            )
            .with_subscriptions([crate::ALL_EVENTS]),
        ))
        .unwrap();
        reg.mount_all(&MountContext::new());
        assert!(reg.wants_event("anything.at.all"));
    }

    /// 投递带重入保护：模块的 `on_event` 里再 emit，内层被丢并计数。
    #[test]
    fn guarded_delivery_drops_reentrant_emits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Reentrant {
            manifest: ModuleManifest,
            registry: std::sync::Mutex<Option<Arc<ModuleRegistry>>>,
            attempts: Arc<AtomicUsize>,
        }
        impl Module for Reentrant {
            fn manifest(&self) -> &ModuleManifest {
                &self.manifest
            }
            fn mount(&self, _ctx: &MountContext) -> ModuleMountResult {
                Ok(vec![])
            }
            fn subscribed_events(&self) -> Vec<String> {
                vec!["ping".to_string()]
            }
            fn on_event(&self, event: &crate::ModuleEvent) -> Result<(), String> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                // 在事件处理里再投递一次——内层应当被重入保护丢弃。
                if let Some(reg) = self.registry.lock().unwrap().as_ref() {
                    reg.deliver_event_guarded(event);
                }
                Ok(())
            }
        }
        let reg = Arc::new(ModuleRegistry::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(Reentrant {
            manifest: ModuleManifest::builtin("rt.re", "re", "1.0", "tool", "e", "d"),
            registry: std::sync::Mutex::new(Some(Arc::clone(&reg))),
            attempts: attempts.clone(),
        }))
        .unwrap();
        reg.mount_all(&MountContext::new());
        reg.deliver_event_guarded(&crate::ModuleEvent::host("ping", serde_json::json!({})));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "内层必须被丢弃");
        let (delivered, failed) = reg.event_counters();
        assert_eq!(delivered, 1, "外层那次投递成功");
        assert_eq!(failed, 1, "内层那次记一次失败");
    }
}
